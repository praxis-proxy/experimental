//! `switchyard_route`: Mixture-of-Models routing via NVIDIA `NeMo` Switchyard
//! (Capability mode). Decision-only: judge → weak/strong tier → cluster+model.
//!
//! Demo greps `judge verdict` / `routed` / `floor_skip` / `reuse` /
//! `default_strong` / `routing failed` / `fail-open` in
//! `demos/switchyard-route/run-demo.sh`.

#![expect(
    clippy::large_futures,
    clippy::large_stack_frames,
    clippy::too_many_lines,
    reason = "POC filter: pingora/switchyard types are large; sequential HTTP logic is clearer inline"
)]

mod config;
mod failure;
mod session;

use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytes::Bytes;
use pingora_core::upstreams::peer::HttpPeer;
use praxis_core::subrequest::{SubRequest, SubRequestClient};
use praxis_filter::{BodyAccess, BodyMode, FilterAction, FilterError, HttpFilter, HttpFilterContext, Rejection};
use switchyard_libsy::Algorithm;
use tracing::{debug, warn};

use self::config::{RouteConfig, SessionFloor, Tier};

/// Metadata key for the chosen cluster (body phase → `on_request`).
const METADATA_CLUSTER: &str = "switchyard_route.cluster";

/// Why this request used a cluster: live route, reuse, default Strong, …
const METADATA_DECISION: &str = "switchyard_route.decision";

/// Truncated routing error; set on every judge/decode failure.
const METADATA_ERROR: &str = "switchyard_route.error";

/// Default max body size for buffering (1 MiB).
const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024;

/// The `switchyard_route` HTTP filter.
pub(crate) struct SwitchyardRouteFilter {
    /// Validated configuration.
    config: RouteConfig,
    /// The Capability-mode classifier, built once at config time.
    algorithm: Arc<dyn Algorithm>,
    /// Last successful judge tier per session key (in-process, TTL + cap).
    sessions: Mutex<session::SessionStore>,
}

impl std::fmt::Debug for SwitchyardRouteFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwitchyardRouteFilter")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl SwitchyardRouteFilter {
    /// Creates the filter from parsed YAML config.
    ///
    /// # Errors
    ///
    /// Returns a [`FilterError`] when the YAML is invalid or Switchyard
    /// rejects the classifier configuration.
    pub(crate) fn from_config(yaml: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let config = config::parse(yaml)?;
        let algorithm = build_algorithm(&config)?;
        Ok(Box::new(Self {
            config,
            algorithm,
            sessions: Mutex::new(session::SessionStore::with_defaults()),
        }))
    }

    /// Runs the routing decision: parse body, call judge, pick tier, rewrite.
    ///
    /// When `session_floor` is enabled and the session is already at Strong,
    /// the judge is skipped entirely (the outcome would be Strong regardless).
    async fn route(&self, ctx: &mut HttpFilterContext<'_>, body: &mut Option<Bytes>) -> Result<Tier, RouteError> {
        let value = parse_body(body.as_ref())?;

        // Detect wire format from path
        let path = ctx.request.uri.path();
        if !path.ends_with("/chat/completions") {
            return Err(RouteError::UnsupportedPath);
        }

        let session_key = session::session_key_from_request(&ctx.request.headers, &value);
        let now = Instant::now();

        // Floor optimization: if the session floor is already at max, skip the judge.
        if self.config.session_floor == SessionFloor::Enabled
            && let Some(key) = &session_key
        {
            let floor = self.lock_sessions().last_success(key, now);
            if let Some(floor) = floor.filter(|tier| tier.is_max()) {
                let cluster = rewrite_for_tier(&self.config, body, value, floor)?;
                ctx.set_metadata(METADATA_CLUSTER, cluster);
                ctx.set_metadata(METADATA_DECISION, failure::DECISION_FLOOR_SKIP);
                debug!(tier = %floor.tag(), "switchyard_route: floor_skip");
                return Ok(floor);
            }
        }

        let llm_request = decode_for_judge(&value)?;
        let client = ctx
            .subrequest_client
            .as_ref()
            .ok_or(RouteError::MissingSubrequestClient)?;
        let tier = self.decide(client, llm_request).await?;
        let cluster = rewrite_for_tier(&self.config, body, value, tier)?;
        ctx.set_metadata(METADATA_CLUSTER, cluster);
        ctx.set_metadata(METADATA_DECISION, failure::DECISION_ROUTED);
        if let Some(key) = session_key {
            self.lock_sessions()
                .remember(&key, tier, now, self.config.session_floor);
        }
        Ok(tier)
    }

    /// Recovers from a poisoned mutex so one panicked request cannot stick the filter.
    fn lock_sessions(&self) -> MutexGuard<'_, session::SessionStore> {
        self.sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records the error and applies `on_failure` (reuse / default Strong / 503 / unrouted).
    fn on_route_error(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        err: &RouteError,
    ) -> FilterAction {
        warn!(error = %err, "switchyard_route: routing failed");
        record_error_metadata(ctx, err);
        let may_apply = err.may_apply_failure_tier();
        let parsed = may_apply.then(|| parse_body(body.as_ref()).ok()).flatten();
        let remembered = parsed.as_ref().and_then(|value| {
            let key = session::session_key_from_request(&ctx.request.headers, value)?;
            self.lock_sessions().last_success(&key, Instant::now())
        });
        match failure::failure_action(self.config.on_failure, may_apply, remembered) {
            failure::FailureAction::Reject => reject_closed(ctx),
            failure::FailureAction::Unrouted => fail_open_unrouted(ctx),
            failure::FailureAction::Apply(apply) => match parsed {
                Some(value) => self.apply_failure_tier(ctx, body, apply, value),
                None => fail_open_unrouted(ctx),
            },
        }
    }

    /// Rewrites `model` and cluster metadata for an `open` failure apply.
    ///
    /// Closed already rejected in [`failure::failure_action`]; this path is
    /// `on_failure: open` only. If rewrite fails, the request continues unrouted.
    fn apply_failure_tier(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        apply: failure::FailureApply,
        value: serde_json::Value,
    ) -> FilterAction {
        match rewrite_for_tier(&self.config, body, value, apply.tier) {
            Ok(cluster) => {
                ctx.set_metadata(METADATA_CLUSTER, cluster);
                ctx.set_metadata(METADATA_DECISION, apply.kind.metadata());
                log_failure_apply(apply);
                FilterAction::Continue
            },
            Err(err) => {
                debug!(error = %err, "switchyard_route: fallback rewrite failed");
                fail_open_unrouted(ctx)
            },
        }
    }

    /// Drives Switchyard's step stream to get a routing decision.
    async fn decide(
        &self,
        client: &SubRequestClient,
        llm_request: switchyard_protocol::LlmRequest,
    ) -> Result<Tier, RouteError> {
        use futures::StreamExt as _;
        use switchyard_libsy::Step;
        use switchyard_protocol::{Context, Metadata, Request};

        let request = Request {
            llm_request,
            raw_request: None,
            metadata: Some(Metadata::default()),
        };

        let stream = Arc::clone(&self.algorithm).run_stream(Context::default(), request, None);
        futures::pin_mut!(stream);

        while let Some(item) = stream.next().await {
            let step = item.map_err(|err| RouteError::Run(err.to_string()))?;
            match step {
                Step::Decision(decision) if decision.is_routed_call() => {
                    let tag = decision.selected_model();
                    return Tier::from_tag(tag).ok_or_else(|| RouteError::UnknownTier(tag.into()));
                },
                Step::Decision(_) => {},
                Step::CallLlm(call) => {
                    if call.get_decision().is_routed_call() {
                        let tag = call.get_decision().selected_model();
                        return Tier::from_tag(tag).ok_or_else(|| RouteError::UnknownTier(tag.into()));
                    }
                    // Serve the judge call - unbox the CallLlmRequest
                    self.serve_judge(client, *call).await?;
                },
                Step::ReturnToAgent(_) => return Err(RouteError::NoDecision),
            }
        }
        Err(RouteError::NoDecision)
    }

    /// Serves a judge `CallLlm` step via `SubRequestClient`.
    ///
    /// Switchyard prepares the judge request (system prompt, messages,
    /// response format). This method only encodes it onto the `OpenAI` chat
    /// wire, POSTs it, and decodes the reply back into Switchyard IR.
    async fn serve_judge(
        &self,
        client: &SubRequestClient,
        call: switchyard_libsy::CallLlmRequest,
    ) -> Result<(), RouteError> {
        let body_bytes = encode_judge_request(&call, &self.config.judge.model)?;
        // Demo: useful when debugging judge callouts from `run-demo.sh` / server.log.
        debug!(bytes = body_bytes.len(), "switchyard_route: judge request encoded");

        let endpoint = JudgeEndpoint::parse(&self.config.judge.endpoint)?;
        let addrs = resolve_judge_addrs(&endpoint.host, endpoint.port).await?;
        let subrequest = endpoint.build_request(body_bytes, self.config.judge.auth_token.as_deref())?;
        let timeout = Duration::from_millis(self.config.judge.timeout_ms);

        let callout = JudgeCallout {
            client,
            endpoint: &endpoint,
            subrequest: &subrequest,
            timeout,
            verify_tls: self.config.judge.verify_tls,
        };

        let mut last_error = String::from("no address attempted");
        for addr in addrs {
            match fetch_judge_body(&callout, addr).await {
                Ok(body) => {
                    // Demo: `run-demo.sh` greps this line under "routing decisions".
                    log_judge_verdict(&body);
                    match decode_judge_aggregated(addr, &body) {
                        Ok(aggregated) => return respond_judge(call, aggregated),
                        Err(JudgeAttemptError::Retryable(message)) => {
                            last_error = message;
                            warn!(%last_error, "switchyard_route: judge attempt failed, trying next address");
                        },
                        Err(JudgeAttemptError::Fatal(err)) => return Err(err),
                    }
                },
                Err(JudgeAttemptError::Retryable(message)) => {
                    last_error = message;
                    warn!(%last_error, "switchyard_route: judge attempt failed, trying next address");
                },
                Err(JudgeAttemptError::Fatal(err)) => return Err(err),
            }
        }

        Err(RouteError::Judge(last_error))
    }
}

/// Bundles inputs for a judge HTTP callout (keeps argument count down).
struct JudgeCallout<'callout> {
    /// Subrequest client from the filter context.
    client: &'callout SubRequestClient,
    /// Parsed judge URL.
    endpoint: &'callout JudgeEndpoint,
    /// Encoded OpenAI-style judge POST.
    subrequest: &'callout SubRequest,
    /// Per-attempt timeout.
    timeout: Duration,
    /// Whether to verify TLS certificates for HTTPS judges.
    verify_tls: bool,
}

/// Outcome of a single judge address attempt.
enum JudgeAttemptError {
    /// Try the next resolved address.
    Retryable(String),
    /// Stop the callout (translation or respond failure).
    Fatal(RouteError),
}

/// Builds an `HttpPeer` for a judge address, optionally skipping TLS verify.
fn build_judge_peer(addr: std::net::SocketAddr, endpoint: &JudgeEndpoint, verify_tls: bool) -> HttpPeer {
    let mut peer = HttpPeer::new(addr, endpoint.tls, endpoint.sni.clone());
    if endpoint.tls && !verify_tls {
        peer.options.verify_cert = false;
        peer.options.verify_hostname = false;
    }
    peer
}

/// POSTs the judge request to one address and returns the response body on 2xx.
async fn fetch_judge_body(callout: &JudgeCallout<'_>, addr: std::net::SocketAddr) -> Result<Bytes, JudgeAttemptError> {
    let peer = build_judge_peer(addr, callout.endpoint, callout.verify_tls);
    let response = callout
        .client
        .execute(&peer, callout.subrequest, DEFAULT_MAX_BODY_BYTES, callout.timeout, None)
        .await
        .map_err(|err| JudgeAttemptError::Retryable(format!("{addr}: {err}")))?;

    if !(200..300).contains(&response.status) {
        return Err(JudgeAttemptError::Retryable(format!(
            "HTTP {} from {addr} body_len={} preview={:?}",
            response.status,
            response.body.len(),
            body_preview(&response.body)
        )));
    }
    Ok(response.body)
}

/// Parses and translates a judge JSON body into Switchyard IR.
fn decode_judge_aggregated(
    addr: std::net::SocketAddr,
    body: &Bytes,
) -> Result<switchyard_protocol::AggLlmResponse, JudgeAttemptError> {
    use switchyard_protocol::WireFormat;

    let value: serde_json::Value = serde_json::from_slice(body).map_err(|err| {
        JudgeAttemptError::Retryable(format!(
            "judge_non_json from {addr}: {err}; body_len={} preview={:?}",
            body.len(),
            body_preview(body)
        ))
    })?;

    switchyard_translation::decode_aggregated_response(&value, WireFormat::OpenAiChat)
        .map_err(|err| JudgeAttemptError::Fatal(RouteError::Judge(format!("response translation failed: {err}"))))
}

/// Delivers a decoded judge response back into the Switchyard step loop.
fn respond_judge(
    call: switchyard_libsy::CallLlmRequest,
    aggregated: switchyard_protocol::AggLlmResponse,
) -> Result<(), RouteError> {
    use switchyard_protocol::{LlmResponse, Response};

    call.respond(Ok(Response {
        llm_response: LlmResponse::Agg(aggregated),
        metadata: None,
    }))
    .map_err(|err| RouteError::Judge(format!("failed to deliver judge response: {err}")))
}

#[async_trait]
impl HttpFilter for SwitchyardRouteFilter {
    fn name(&self) -> &'static str {
        "switchyard_route"
    }

    fn request_body_access(&self) -> BodyAccess {
        BodyAccess::ReadWrite
    }

    fn request_body_mode(&self) -> BodyMode {
        BodyMode::StreamBuffer {
            max_bytes: Some(DEFAULT_MAX_BODY_BYTES),
        }
    }

    fn selects_cluster(&self) -> bool {
        true
    }

    fn selected_clusters(&self) -> Vec<String> {
        vec![self.config.weak.cluster.clone(), self.config.strong.cluster.clone()]
    }

    async fn on_request_body(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        // StreamBuffer may invoke this before the full body is available.
        if !end_of_stream {
            return Ok(FilterAction::Continue);
        }

        match self.route(ctx, body).await {
            Ok(tier) => {
                if ctx.get_metadata(METADATA_DECISION) != Some(failure::DECISION_FLOOR_SKIP) {
                    debug!(tier = %tier.tag(), "switchyard_route: routed");
                }
                Ok(FilterAction::Continue)
            },
            Err(err) => Ok(self.on_route_error(ctx, body, &err)),
        }
    }

    async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        if let Some(cluster) = ctx.get_metadata(METADATA_CLUSTER) {
            ctx.cluster = Some(cluster.into());
        }
        Ok(FilterAction::Continue)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parsed judge URL components for the sub-request callout.
struct JudgeEndpoint {
    /// Whether the judge endpoint uses HTTPS.
    tls: bool,
    /// Hostname used for DNS resolution.
    host: String,
    /// TCP port.
    port: u16,
    /// TLS SNI; empty for cleartext HTTP.
    sni: String,
    /// Original URL authority, sent as the `Host` header.
    authority: http::HeaderValue,
    /// Path and query for the POST.
    uri: http::Uri,
}

impl JudgeEndpoint {
    /// Parses an absolute http(s) judge URL.
    fn parse(endpoint: &str) -> Result<Self, RouteError> {
        let parsed: http::Uri = endpoint
            .parse()
            .map_err(|err| RouteError::Judge(format!("bad endpoint: {err}")))?;
        let tls = match parsed.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            Some(other) => return Err(RouteError::Judge(format!("unsupported scheme '{other}'"))),
            None => return Err(RouteError::Judge("endpoint must be an absolute http(s) URL".into())),
        };
        let authority = parsed
            .authority()
            .ok_or_else(|| RouteError::Judge("endpoint missing host".into()))?;
        let host = authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let port = authority.port_u16().unwrap_or(if tls { 443 } else { 80 });
        let sni = if tls { host.clone() } else { String::new() };
        let authority = http::HeaderValue::from_str(authority.as_str())
            .map_err(|err| RouteError::Judge(format!("invalid authority: {err}")))?;
        let uri: http::Uri = parsed
            .path_and_query()
            .map_or("/", http::uri::PathAndQuery::as_str)
            .parse()
            .map_err(|err| RouteError::Judge(format!("bad path: {err}")))?;
        Ok(Self {
            tls,
            host,
            port,
            sni,
            authority,
            uri,
        })
    }

    /// Builds the POST sub-request carrying the encoded judge body.
    fn build_request(&self, body: Bytes, auth_token: Option<&str>) -> Result<SubRequest, RouteError> {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::HOST, self.authority.clone());
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        if let Some(token) = auth_token {
            let value = format!("Bearer {token}");
            let header = http::HeaderValue::from_str(&value)
                .map_err(|err| RouteError::Judge(format!("invalid auth header: {err}")))?;
            headers.insert(http::header::AUTHORIZATION, header);
        }
        Ok(SubRequest {
            method: http::Method::POST,
            uri: self.uri.clone(),
            headers,
            body,
        })
    }
}

/// Resolves every address for the judge host so connects can fall back.
async fn resolve_judge_addrs(host: &str, port: u16) -> Result<Vec<std::net::SocketAddr>, RouteError> {
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|err| RouteError::Judge(format!("DNS resolution failed for {host}: {err}")))?
        .collect();
    if addrs.is_empty() {
        return Err(RouteError::Judge(format!("no addresses resolved for {host}")));
    }
    Ok(addrs)
}

/// Truncates a body for log-safe error previews.
fn body_preview(body: &Bytes) -> String {
    String::from_utf8_lossy(body).chars().take(200).collect()
}

/// Builds the Capability-mode classifier from config.
fn build_algorithm(config: &RouteConfig) -> Result<Arc<dyn Algorithm>, FilterError> {
    use switchyard_libsy::{
        ClassifierContractConfig, LlmClassifierConfig, LlmTarget, LlmTaskClassifier, TaskClassifierConfig,
    };

    let classifier_config = LlmClassifierConfig::Capability {
        judge_target: LlmTarget {
            semantic_name: "judge".to_owned(),
            llm_client: None,
        },
        efficient_target: LlmTarget {
            semantic_name: Tier::Weak.tag().to_owned(),
            llm_client: None,
        },
        capable_target: LlmTarget {
            semantic_name: Tier::Strong.tag().to_owned(),
            llm_client: None,
        },
        config: TaskClassifierConfig {
            base_threshold: config.threshold,
            session_affinity: false,
            contract: ClassifierContractConfig::default(),
            ..TaskClassifierConfig::default()
        },
    };

    let classifier = LlmTaskClassifier::new(classifier_config)
        .map_err(|err| FilterError::from(format!("switchyard config rejected: {err}")))?;

    let arc: Arc<dyn Algorithm> = Arc::new(classifier);
    Ok(arc)
}

/// Encodes the prepared judge request onto the `OpenAI` chat wire.
fn encode_judge_request(call: &switchyard_libsy::CallLlmRequest, judge_model: &str) -> Result<Bytes, RouteError> {
    let mut llm_request = call.get_request().llm_request.clone();
    llm_request.model = Some(judge_model.to_owned());
    llm_request.stream = false;
    let wire = switchyard_translation::encode_request(&llm_request, switchyard_protocol::WireFormat::OpenAiChat)
        .map_err(|err| RouteError::Judge(format!("request encoding failed: {err}")))?;
    let bytes =
        serde_json::to_vec(&wire).map_err(|err| RouteError::Judge(format!("request serialization failed: {err}")))?;
    Ok(Bytes::from(bytes))
}

/// Demo: log Switchyard verdict fields for `run-demo.sh` ("routing decisions").
fn log_judge_verdict(body: &[u8]) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return;
    };
    let Some(content) = value
        .pointer("/choices/0/message/content")
        .and_then(serde_json::Value::as_str)
    else {
        return;
    };
    let Ok(verdict) = serde_json::from_str::<serde_json::Value>(content) else {
        return;
    };
    let p_solve = verdict.get("p_solve").and_then(serde_json::Value::as_f64);
    let rule = verdict
        .get("primary_rule")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let boundary = verdict
        .get("capability_boundary")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    debug!(?p_solve, rule, boundary, "switchyard_route: judge verdict");
}

/// Parses the buffered body as JSON.
fn parse_body(body: Option<&Bytes>) -> Result<serde_json::Value, RouteError> {
    let raw = body.ok_or(RouteError::Body("missing"))?;
    if raw.is_empty() {
        return Err(RouteError::Body("empty"));
    }
    serde_json::from_slice(raw).map_err(|err| RouteError::Json(err.to_string()))
}

/// Decodes an `OpenAI` chat body into Switchyard IR for the judge.
fn decode_for_judge(body: &serde_json::Value) -> Result<switchyard_protocol::LlmRequest, RouteError> {
    switchyard_translation::decode_request(switchyard_protocol::WireFormat::OpenAiChat, body)
        .map_err(|err| RouteError::Translation(err.to_string()))
}

/// Rewrites the `model` field and re-serializes.
fn rewrite_model(mut body: serde_json::Value, model: &str) -> Result<Vec<u8>, RouteError> {
    body.as_object_mut()
        .ok_or(RouteError::Body("not an object"))?
        .insert("model".to_owned(), serde_json::Value::String(model.into()));
    serde_json::to_vec(&body).map_err(|err| RouteError::Serialize(err.to_string()))
}

/// Rewrites the buffered JSON to the tier's model and returns the cluster name.
fn rewrite_for_tier(
    config: &RouteConfig,
    body: &mut Option<Bytes>,
    value: serde_json::Value,
    tier: Tier,
) -> Result<String, RouteError> {
    let target = config.target(tier);
    let new_body = rewrite_model(value, &target.model)?;
    *body = Some(Bytes::from(new_body));
    Ok(target.cluster.clone())
}

/// Truncates a routing error for `switchyard_route.error` metadata.
fn record_error_metadata(ctx: &mut HttpFilterContext<'_>, err: &RouteError) {
    let short: String = err.to_string().chars().take(250).collect();
    ctx.set_metadata(METADATA_ERROR, short);
}

/// HTTP 503 for `on_failure: closed`.
fn reject_closed(ctx: &mut HttpFilterContext<'_>) -> FilterAction {
    ctx.set_metadata(METADATA_DECISION, failure::DECISION_REJECTED);
    FilterAction::Reject(Rejection::status(503))
}

/// Continue without a Switchyard cluster (`open` and we cannot rewrite).
fn fail_open_unrouted(ctx: &mut HttpFilterContext<'_>) -> FilterAction {
    ctx.set_metadata(METADATA_DECISION, failure::DECISION_UNROUTED);
    debug!("switchyard_route: fail-open, passing through");
    FilterAction::Continue
}

/// Demo-greppable log line for reuse vs default Strong.
fn log_failure_apply(apply: failure::FailureApply) {
    match apply.kind {
        failure::FailureApplyKind::Reuse => {
            debug!(tier = %apply.tier.tag(), "switchyard_route: reuse");
        },
        failure::FailureApplyKind::DefaultStrong => {
            debug!(tier = %apply.tier.tag(), "switchyard_route: default_strong");
        },
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Routing failures (internal to this filter).
#[derive(Debug, thiserror::Error)]
enum RouteError {
    /// Request body missing, empty, or not a JSON object.
    #[error("request body {0}")]
    Body(&'static str),
    /// Body bytes were not valid JSON.
    #[error("invalid JSON: {0}")]
    Json(String),
    /// Path is not an `OpenAI` chat completions endpoint.
    #[error("unsupported path (only /chat/completions)")]
    UnsupportedPath,
    /// `OpenAI` ↔ Switchyard IR translation failed.
    #[error("translation failed: {0}")]
    Translation(String),
    /// Failed to re-serialize the rewritten request body.
    #[error("serialize failed: {0}")]
    Serialize(String),
    /// Filter context had no `SubRequestClient` for the judge callout.
    #[error("subrequest client unavailable")]
    MissingSubrequestClient,
    /// Judge HTTP callout failed after retries.
    #[error("judge callout failed: {0}")]
    Judge(String),
    /// Switchyard `run_stream` returned an error.
    #[error("switchyard run failed: {0}")]
    Run(String),
    /// Stream ended without a routed weak/strong decision.
    #[error("no routing decision")]
    NoDecision,
    /// Decision tag was not `weak` or `strong`.
    #[error("unknown tier '{0}'")]
    UnknownTier(String),
}

impl RouteError {
    /// Whether this failure is a chat request we may rewrite to a fallback tier.
    fn may_apply_failure_tier(&self) -> bool {
        match self {
            Self::Body(_) | Self::Json(_) | Self::UnsupportedPath | Self::Serialize(_) => false,
            Self::Translation(_)
            | Self::MissingSubrequestClient
            | Self::Judge(_)
            | Self::Run(_)
            | Self::NoDecision
            | Self::UnknownTier(_) => true,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test-module suppressions")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::assertions_on_result_states,
    clippy::arithmetic_side_effects,
    clippy::let_underscore_must_use,
    clippy::min_ident_chars,
    clippy::float_cmp,
    clippy::too_many_lines,
    reason = "unwrap/expect/panic and terse helpers are acceptable in tests"
)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{
            LazyLock,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use praxis_core::subrequest::SubRequestConnector;
    use praxis_filter::{FilterRegistry, Request};

    use super::*;

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    /// Deterministic ID generator for the test filter context.
    static TEST_IDS: LazyLock<praxis_core::id::IdGenerator> =
        LazyLock::new(|| praxis_core::id::IdGenerator::with_seed(0));

    /// Wall-clock source for the test filter context.
    static TEST_TIME: praxis_core::time::SystemTimeSource = praxis_core::time::SystemTimeSource;

    /// Builds a POST request for `path`.
    fn make_request(path: &str) -> Request {
        Request {
            method: http::Method::POST,
            uri: path.parse().expect("test path is a valid URI"),
            headers: http::HeaderMap::new(),
        }
    }

    /// Builds a POST request carrying an explicit Switchyard session id.
    fn make_request_with_session(path: &str, session: &str) -> Request {
        let mut request = make_request(path);
        request.headers.insert(
            http::HeaderName::from_static(session::SESSION_ID_HEADER),
            http::HeaderValue::from_str(session).expect("test session id is a valid header value"),
        );
        request
    }

    /// First turn: judge returns Strong so later turns can exercise floor-skip.
    async fn seed_strong_floor(filter: &dyn HttpFilter, request: &Request, client: &SubRequestClient) {
        let mut ctx = make_ctx(request, Some(client));
        let mut body = Some(chat_body("complex question"));
        drop(
            filter
                .on_request_body(&mut ctx, &mut body, true)
                .await
                .expect("first turn routes to Strong"),
        );
        assert_eq!(
            ctx.get_metadata(METADATA_CLUSTER),
            Some("strong-cluster"),
            "seed turn must store a Strong floor"
        );
    }

    /// Builds a filter context, mirroring every field of `HttpFilterContext`.
    fn make_ctx<'ctx>(request: &'ctx Request, client: Option<&'ctx SubRequestClient>) -> HttpFilterContext<'ctx> {
        HttpFilterContext {
            buffered_request_body: None,
            body_done_indices: Vec::new(),
            branch_iterations: std::collections::HashMap::new(),
            client_addr: None,
            cluster: None,
            current_filter_id: None,
            downstream_tls: false,
            metrics_route: None,
            peer_identity: None,
            extensions: praxis_filter::RequestExtensions::default(),
            executed_filter_indices: Vec::new(),
            extra_request_headers: Vec::new(),
            request_headers_to_remove: Vec::new(),
            request_headers_to_set: Vec::new(),
            filter_metadata: std::collections::HashMap::new(),
            pre_read_mutations: Vec::new(),
            structured_metadata: std::collections::HashMap::new(),
            filter_results: std::collections::HashMap::new(),
            filter_state: std::collections::HashMap::new(),
            health_registry: None,
            id_generator: &TEST_IDS,
            kv_stores: None,
            session_stores: None,
            subrequest_client: client,
            subrequest_response_mode: praxis_filter::SubRequestResponseMode::Buffered,
            request,
            request_body_bytes: 0,
            request_body_mode: BodyMode::Stream,
            request_start: Instant::now(),
            response_body_bytes: 0,
            response_body_mode: BodyMode::Stream,
            response_header: None,
            response_headers_modified: false,
            selected_endpoint_index: None,
            attempted_endpoints: Vec::new(),
            retry_policy: None,
            route_retry_policy: None,
            cluster_retry_state: None,
            cluster_retry_state_released: false,
            endpoint_reselector: None,
            pinned_endpoint_address: None,
            time_source: &TEST_TIME,
            rewritten_path: None,
            upstream: None,
        }
    }

    /// Builds filter YAML pointing at `endpoint` with the given failure mode.
    fn config_yaml(endpoint: &str, on_failure: &str) -> serde_yaml::Value {
        config_yaml_full(endpoint, on_failure, "enabled")
    }

    /// Builds filter YAML with an explicit `session_floor` setting.
    fn config_yaml_full(endpoint: &str, on_failure: &str, session_floor: &str) -> serde_yaml::Value {
        let yaml = format!(
            concat!(
                "judge:\n",
                "  endpoint: \"{}\"\n",
                "  model: \"judge-model\"\n",
                "  timeout_ms: 4000\n",
                "targets:\n",
                "  weak:\n",
                "    cluster: \"weak-cluster\"\n",
                "    model: \"weak-model\"\n",
                "  strong:\n",
                "    cluster: \"strong-cluster\"\n",
                "    model: \"strong-model\"\n",
                "threshold: 0.5\n",
                "on_failure: {}\n",
                "session_floor: {}\n",
            ),
            endpoint, on_failure, session_floor
        );
        serde_yaml::from_str(&yaml).expect("test config YAML parses")
    }

    /// Builds a filter whose judge lives at `endpoint`.
    fn make_filter(endpoint: &str, on_failure: &str) -> Box<dyn HttpFilter> {
        SwitchyardRouteFilter::from_config(&config_yaml(endpoint, on_failure)).expect("test filter config is valid")
    }

    /// Builds a filter with an explicit `session_floor` setting.
    fn make_filter_with_floor(endpoint: &str, on_failure: &str, session_floor: &str) -> Box<dyn HttpFilter> {
        SwitchyardRouteFilter::from_config(&config_yaml_full(endpoint, on_failure, session_floor))
            .expect("test filter config is valid")
    }

    /// A minimal `OpenAI` chat request body.
    fn chat_body(prompt: &str) -> Bytes {
        Bytes::from(
            serde_json::json!({
                "model": "client-model",
                "messages": [{"role": "user", "content": prompt}],
            })
            .to_string(),
        )
    }

    /// The judge verdict payload, mirroring `demos/switchyard-route/upstreams.py`.
    fn verdict(p_solve: f64, rule: &str, boundary: &str) -> String {
        serde_json::json!({
            "crux": "test prompt",
            "primary_rule": rule,
            "capability_boundary": boundary,
            "p_solve": p_solve,
        })
        .to_string()
    }

    /// Wraps `content` in an `OpenAI` chat completion response.
    fn judge_body(content: &str) -> String {
        serde_json::json!({
            "id": "chat-test",
            "object": "chat.completion",
            "model": "judge-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop",
            }],
        })
        .to_string()
    }

    /// True once the buffer holds a complete request (headers plus declared body).
    fn request_is_complete(raw: &[u8]) -> bool {
        let text = String::from_utf8_lossy(raw);
        let Some(header_end) = text.find("\r\n\r\n") else {
            return false;
        };
        let declared = text
            .lines()
            .find_map(|line| {
                let lowered = line.to_ascii_lowercase();
                lowered
                    .strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        raw.len() >= header_end + 4 + declared
    }

    /// Serves one canned HTTP response per request on an ephemeral port.
    async fn spawn_judge(status_line: &'static str, body: String) -> SocketAddr {
        spawn_judge_sequence(vec![(status_line, body)]).await
    }

    /// Serves `responses` in order, one per request, repeating the last one.
    ///
    /// Requests are counted rather than connections so that a pooled keep-alive
    /// connection cannot desynchronise the sequence.
    async fn spawn_judge_sequence(responses: Vec<(&'static str, String)>) -> SocketAddr {
        assert!(!responses.is_empty(), "the mock judge needs at least one response");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock judge binds an ephemeral port");
        let addr = listener.local_addr().expect("mock judge reports its address");
        let responses = Arc::new(responses);
        let served = Arc::new(AtomicUsize::new(0));
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let responses = Arc::clone(&responses);
                let served = Arc::clone(&served);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                    loop {
                        let mut raw = Vec::new();
                        let mut chunk = [0_u8; 4096];
                        let complete = loop {
                            match stream.read(&mut chunk).await {
                                Ok(0) | Err(_) => break false,
                                Ok(read) => raw.extend_from_slice(&chunk[..read]),
                            }
                            if request_is_complete(&raw) {
                                break true;
                            }
                        };
                        if !complete {
                            break;
                        }
                        let index = served.fetch_add(1, Ordering::SeqCst).min(responses.len() - 1);
                        let (status_line, body) = &responses[index];
                        let response = format!(
                            "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        if stream.write_all(response.as_bytes()).await.is_err() {
                            break;
                        }
                        drop(stream.flush().await);
                    }
                });
            }
        });
        addr
    }

    /// A subrequest client suitable for judge callouts in tests.
    fn make_client() -> SubRequestClient {
        SubRequestClient::new(SubRequestConnector::new(2, None))
    }

    // -----------------------------------------------------------------------
    // Registration and filter metadata
    // -----------------------------------------------------------------------

    #[test]
    fn filter_is_registered() {
        let mut registry = FilterRegistry::with_builtins();
        crate::register_filters(&mut registry);
        let names = registry.available_filters();
        assert!(
            names.contains(&"switchyard_route"),
            "expected switchyard_route in {names:?}"
        );
    }

    #[test]
    fn advertises_body_buffering_and_cluster_selection() {
        let filter = make_filter("http://127.0.0.1:1/v1/chat/completions", "open");
        assert_eq!(
            filter.name(),
            "switchyard_route",
            "advertised name must match registration"
        );
        assert!(
            matches!(filter.request_body_access(), BodyAccess::ReadWrite),
            "the filter rewrites the request body, so it needs read-write access"
        );
        assert!(
            matches!(
                filter.request_body_mode(),
                BodyMode::StreamBuffer {
                    max_bytes: Some(DEFAULT_MAX_BODY_BYTES)
                }
            ),
            "the routing decision needs the whole body buffered"
        );
        assert!(
            filter.selects_cluster(),
            "the filter picks the upstream cluster, so it must declare that"
        );
        assert_eq!(
            filter.selected_clusters(),
            vec!["weak-cluster".to_owned(), "strong-cluster".to_owned()],
            "both tiers must be declared so pipeline validation can see them"
        );
    }

    #[test]
    fn debug_omits_the_algorithm() {
        let config =
            config::parse(&config_yaml("http://127.0.0.1:1/v1/chat/completions", "open")).expect("config is valid");
        let algorithm = build_algorithm(&config).expect("the classifier builds from a valid config");
        let filter = SwitchyardRouteFilter {
            config,
            algorithm,
            sessions: Mutex::new(session::SessionStore::with_defaults()),
        };
        let rendered = format!("{filter:?}");
        assert!(
            rendered.contains("SwitchyardRouteFilter") && rendered.contains("weak-cluster"),
            "Debug must name the type and show config, got {rendered}"
        );
        assert!(
            !rendered.contains("Algorithm"),
            "the classifier is not Debug and must stay out, got {rendered}"
        );
    }

    #[test]
    fn from_config_rejects_invalid_yaml() {
        let yaml: serde_yaml::Value = serde_yaml::from_str("judge: {}").expect("YAML parses");
        assert!(
            SwitchyardRouteFilter::from_config(&yaml).is_err(),
            "a config without targets must be rejected"
        );
    }

    // -----------------------------------------------------------------------
    // JudgeEndpoint::parse
    // -----------------------------------------------------------------------

    #[test]
    fn parses_http_endpoint_with_default_port() {
        let endpoint = JudgeEndpoint::parse("http://judge.internal/v1/chat/completions").expect("URL parses");
        assert!(!endpoint.tls, "http:// must not enable TLS");
        assert_eq!(endpoint.host, "judge.internal", "host comes from the authority");
        assert_eq!(endpoint.port, 80, "http:// defaults to port 80");
        assert_eq!(endpoint.sni, "", "cleartext endpoints carry no SNI");
        assert_eq!(
            endpoint.uri.to_string(),
            "/v1/chat/completions",
            "the POST target is the path and query only"
        );
    }

    #[test]
    fn parses_https_endpoint_with_default_port_and_sni() {
        let endpoint = JudgeEndpoint::parse("https://judge.example.com/v1/chat/completions").expect("URL parses");
        assert!(endpoint.tls, "https:// must enable TLS");
        assert_eq!(endpoint.port, 443, "https:// defaults to port 443");
        assert_eq!(endpoint.sni, "judge.example.com", "TLS endpoints use the host as SNI");
        assert_eq!(
            endpoint.authority.to_str().expect("authority is ASCII"),
            "judge.example.com",
            "the Host header carries the original authority"
        );
    }

    #[test]
    fn parses_explicit_port_and_preserves_query() {
        let endpoint = JudgeEndpoint::parse("https://judge:9443/v1/chat?api-version=2024-02-01").expect("URL parses");
        assert_eq!(endpoint.port, 9443, "an explicit port overrides the scheme default");
        assert_eq!(
            endpoint.uri.to_string(),
            "/v1/chat?api-version=2024-02-01",
            "the query string must survive into the sub-request"
        );
    }

    #[test]
    fn parses_ipv6_host_without_brackets() {
        let endpoint = JudgeEndpoint::parse("http://[::1]:8000/v1/chat/completions").expect("URL parses");
        assert_eq!(endpoint.host, "::1", "DNS lookup needs the bare IPv6 literal");
        assert_eq!(endpoint.port, 8000, "the explicit port is used");
        assert_eq!(
            endpoint.authority.to_str().expect("authority is ASCII"),
            "[::1]:8000",
            "the Host header keeps the bracketed form"
        );
    }

    #[test]
    fn parses_endpoint_without_path_as_root() {
        let endpoint = JudgeEndpoint::parse("http://judge.internal").expect("URL parses");
        assert_eq!(endpoint.uri.to_string(), "/", "a missing path becomes /");
    }

    #[test]
    fn rejects_unsupported_scheme() {
        let Err(err) = JudgeEndpoint::parse("ftp://judge.internal/v1") else {
            panic!("ftp is not a supported judge scheme");
        };
        assert!(
            err.to_string().contains("unsupported scheme 'ftp'"),
            "the error must name the scheme, got {err}"
        );
    }

    #[test]
    fn rejects_relative_endpoint() {
        let Err(err) = JudgeEndpoint::parse("/v1/chat/completions") else {
            panic!("a bare path is not an absolute endpoint");
        };
        assert!(
            err.to_string().contains("absolute http(s) URL"),
            "the error must ask for an absolute URL, got {err}"
        );
    }

    #[test]
    fn rejects_unparsable_endpoint() {
        let Err(err) = JudgeEndpoint::parse("http://ju dge/v1") else {
            panic!("a space is not valid in an authority");
        };
        assert!(
            err.to_string().contains("bad endpoint"),
            "URI parse failures are reported as bad endpoints, got {err}"
        );
    }

    // -----------------------------------------------------------------------
    // JudgeEndpoint::build_request
    // -----------------------------------------------------------------------

    #[test]
    fn builds_unauthenticated_judge_post() {
        let endpoint = JudgeEndpoint::parse("http://judge.internal:8000/v1/chat/completions").expect("URL parses");
        let subrequest = endpoint
            .build_request(Bytes::from_static(b"{}"), None)
            .expect("request builds");
        assert_eq!(subrequest.method, http::Method::POST, "judge calls are POSTs");
        assert_eq!(
            subrequest.uri.to_string(),
            "/v1/chat/completions",
            "the sub-request targets the judge path"
        );
        assert_eq!(
            subrequest
                .headers
                .get(http::header::HOST)
                .map(|value| value.to_str().expect("ASCII")),
            Some("judge.internal:8000"),
            "the Host header carries the judge authority"
        );
        assert_eq!(
            subrequest
                .headers
                .get(http::header::CONTENT_TYPE)
                .map(|value| value.to_str().expect("ASCII")),
            Some("application/json"),
            "the judge speaks JSON"
        );
        assert!(
            !subrequest.headers.contains_key(http::header::AUTHORIZATION),
            "no token configured means no Authorization header"
        );
        assert_eq!(
            subrequest.body,
            Bytes::from_static(b"{}"),
            "the encoded body is passed through"
        );
    }

    #[test]
    fn builds_authenticated_judge_post() {
        let endpoint = JudgeEndpoint::parse("https://judge.example.com/v1/chat/completions").expect("URL parses");
        let subrequest = endpoint
            .build_request(Bytes::from_static(b"{}"), Some("sk-test"))
            .expect("request builds");
        assert_eq!(
            subrequest
                .headers
                .get(http::header::AUTHORIZATION)
                .map(|value| value.to_str().expect("ASCII")),
            Some("Bearer sk-test"),
            "the configured token becomes a bearer credential"
        );
    }

    #[test]
    fn rejects_token_that_cannot_be_a_header() {
        let endpoint = JudgeEndpoint::parse("https://judge.example.com/v1").expect("URL parses");
        let err = endpoint
            .build_request(Bytes::new(), Some("bad\nvalue"))
            .expect_err("a newline cannot go in a header");
        assert!(
            err.to_string().contains("invalid auth header"),
            "the error must point at the auth header, got {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Peer construction and DNS
    // -----------------------------------------------------------------------

    #[test]
    fn builds_cleartext_peer_without_tls_options() {
        let endpoint = JudgeEndpoint::parse("http://judge.internal:8000/v1").expect("URL parses");
        let addr: SocketAddr = "127.0.0.1:8000".parse().expect("socket address parses");
        let peer = build_judge_peer(addr, &endpoint, true);
        assert!(!peer.is_tls(), "an http:// judge must be dialled in cleartext");
    }

    #[test]
    fn builds_verifying_tls_peer_by_default() {
        let endpoint = JudgeEndpoint::parse("https://judge.example.com/v1").expect("URL parses");
        let addr: SocketAddr = "127.0.0.1:8443".parse().expect("socket address parses");
        let peer = build_judge_peer(addr, &endpoint, true);
        assert!(peer.is_tls(), "an https:// judge must be dialled over TLS");
        assert!(
            peer.options.verify_cert,
            "verify_tls: true must keep certificate checks on"
        );
        assert!(
            peer.options.verify_hostname,
            "verify_tls: true must keep hostname checks on"
        );
    }

    #[test]
    fn disables_tls_verification_when_configured() {
        let endpoint = JudgeEndpoint::parse("https://judge.example.com/v1").expect("URL parses");
        let addr: SocketAddr = "127.0.0.1:8443".parse().expect("socket address parses");
        let peer = build_judge_peer(addr, &endpoint, false);
        assert!(
            !peer.options.verify_cert,
            "verify_tls: false must skip certificate checks"
        );
        assert!(
            !peer.options.verify_hostname,
            "verify_tls: false must skip hostname checks"
        );
    }

    #[tokio::test]
    async fn resolves_loopback_judge_addresses() {
        let addrs = resolve_judge_addrs("127.0.0.1", 8000).await.expect("loopback resolves");
        assert_eq!(
            addrs,
            vec!["127.0.0.1:8000".parse::<SocketAddr>().expect("socket address parses")],
            "a literal address resolves to itself"
        );
    }

    #[tokio::test]
    async fn reports_dns_failures() {
        let err = resolve_judge_addrs("judge.invalid.", 8000)
            .await
            .expect_err("the reserved .invalid TLD never resolves");
        assert!(
            err.to_string().contains("DNS resolution failed for judge.invalid."),
            "the error must name the host, got {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Body helpers
    // -----------------------------------------------------------------------

    #[test]
    fn rejects_a_missing_body() {
        let err = parse_body(None).expect_err("no body means no routing decision");
        assert_eq!(
            err.to_string(),
            "request body missing",
            "a missing body is reported as such"
        );
    }

    #[test]
    fn rejects_an_empty_body() {
        let empty = Bytes::new();
        let err = parse_body(Some(&empty)).expect_err("an empty body means no routing decision");
        assert_eq!(
            err.to_string(),
            "request body empty",
            "an empty body is reported as such"
        );
    }

    #[test]
    fn rejects_non_json_body() {
        let raw = Bytes::from_static(b"not json");
        let err = parse_body(Some(&raw)).expect_err("the body must be JSON");
        assert!(
            err.to_string().starts_with("invalid JSON:"),
            "the parse error must be surfaced, got {err}"
        );
    }

    #[test]
    fn parses_json_body() {
        let raw = chat_body("hello");
        let value = parse_body(Some(&raw)).expect("a chat body is JSON");
        assert_eq!(
            value["model"], "client-model",
            "the parsed value keeps the client's fields"
        );
    }

    #[test]
    fn rewrites_the_model_field() {
        let value = serde_json::json!({"model": "client-model", "messages": []});
        let rewritten = rewrite_model(value, "weak-model").expect("an object can be rewritten");
        let parsed: serde_json::Value = serde_json::from_slice(&rewritten).expect("output is JSON");
        assert_eq!(parsed["model"], "weak-model", "the tier's model replaces the client's");
        assert!(
            parsed.get("messages").is_some(),
            "other fields must survive the rewrite"
        );
    }

    #[test]
    fn rejects_rewriting_a_non_object_body() {
        let err =
            rewrite_model(serde_json::json!([1, 2, 3]), "weak-model").expect_err("a JSON array has no model field");
        assert_eq!(
            err.to_string(),
            "request body not an object",
            "the error must say the body is not an object"
        );
    }

    #[test]
    fn previews_truncate_long_bodies() {
        let body = Bytes::from("x".repeat(500));
        let preview = body_preview(&body);
        assert_eq!(preview.chars().count(), 200, "previews are capped at 200 characters");
    }

    #[test]
    fn previews_survive_invalid_utf8() {
        let body = Bytes::from_static(&[0xFF, 0xFE, b'o', b'k']);
        let preview = body_preview(&body);
        assert!(
            preview.ends_with("ok"),
            "lossy decoding must keep the readable tail, got {preview}"
        );
    }

    #[test]
    fn decodes_a_chat_body_for_the_judge() {
        let value = serde_json::json!({
            "model": "client-model",
            "messages": [{"role": "user", "content": "hello"}],
        });
        let decoded = decode_for_judge(&value).expect("a chat body decodes into Switchyard IR");
        assert_eq!(
            decoded.model.as_deref(),
            Some("client-model"),
            "the client's model survives into the IR"
        );
    }

    #[test]
    fn rejects_a_body_the_translator_cannot_decode() {
        let err =
            decode_for_judge(&serde_json::json!("not an object")).expect_err("a bare string is not a chat request");
        assert!(
            err.to_string().starts_with("translation failed:"),
            "translation errors must be surfaced, got {err}"
        );
    }

    #[test]
    fn logging_a_verdict_tolerates_malformed_payloads() {
        // Each of these hits a different early return; none may panic.
        log_judge_verdict(b"not json");
        log_judge_verdict(br#"{"choices": []}"#);
        log_judge_verdict(&serde_json::to_vec(&judge_body("not json")).expect("serializes"));
        log_judge_verdict(judge_body(&verdict(0.95, "SUP-1", "supported")).as_bytes());
        log_judge_verdict(judge_body("{}").as_bytes());
    }

    // -----------------------------------------------------------------------
    // Judge response decoding
    // -----------------------------------------------------------------------

    #[test]
    fn decodes_an_aggregated_judge_response() {
        let addr: SocketAddr = "127.0.0.1:8000".parse().expect("socket address parses");
        let body = Bytes::from(judge_body(&verdict(0.95, "SUP-1", "supported")));
        assert!(
            decode_judge_aggregated(addr, &body).is_ok(),
            "a well-formed chat completion must decode"
        );
    }

    #[test]
    fn retries_when_the_judge_returns_non_json() {
        let addr: SocketAddr = "127.0.0.1:8000".parse().expect("socket address parses");
        let body = Bytes::from_static(b"<html>gateway error</html>");
        match decode_judge_aggregated(addr, &body) {
            Err(JudgeAttemptError::Retryable(message)) => {
                assert!(
                    message.contains("judge_non_json from 127.0.0.1:8000"),
                    "the message must name the address, got {message}"
                );
                assert!(
                    message.contains("<html>"),
                    "the message must preview the body, got {message}"
                );
            },
            Err(JudgeAttemptError::Fatal(err)) => panic!("a non-JSON body is retryable, got fatal {err}"),
            Ok(_) => panic!("HTML is not a judge response"),
        }
    }

    #[test]
    fn fails_fatally_when_translation_rejects_the_judge_response() {
        let addr: SocketAddr = "127.0.0.1:8000".parse().expect("socket address parses");
        let body = Bytes::from_static(b"[1, 2, 3]");
        match decode_judge_aggregated(addr, &body) {
            Err(JudgeAttemptError::Fatal(err)) => assert!(
                err.to_string().contains("response translation failed"),
                "the error must name the translation step, got {err}"
            ),
            Err(JudgeAttemptError::Retryable(message)) => {
                panic!("valid JSON that is not a completion is fatal, got retryable {message}")
            },
            Ok(_) => panic!("a JSON array is not a judge response"),
        }
    }

    // -----------------------------------------------------------------------
    // Error rendering
    // -----------------------------------------------------------------------

    #[test]
    fn route_errors_render_their_cause() {
        let cases = [
            (RouteError::Body("missing"), "request body missing"),
            (
                RouteError::Json("expected value".to_owned()),
                "invalid JSON: expected value",
            ),
            (RouteError::UnsupportedPath, "unsupported path (only /chat/completions)"),
            (
                RouteError::Translation("no messages".to_owned()),
                "translation failed: no messages",
            ),
            (RouteError::Serialize("io".to_owned()), "serialize failed: io"),
            (RouteError::MissingSubrequestClient, "subrequest client unavailable"),
            (RouteError::Judge("timeout".to_owned()), "judge callout failed: timeout"),
            (RouteError::Run("stream".to_owned()), "switchyard run failed: stream"),
            (RouteError::NoDecision, "no routing decision"),
            (RouteError::UnknownTier("medium".to_owned()), "unknown tier 'medium'"),
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_string(), expected, "RouteError must render its cause");
        }
    }

    // -----------------------------------------------------------------------
    // Filter hooks
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn ignores_body_chunks_before_end_of_stream() {
        let filter = make_filter("http://127.0.0.1:1/v1/chat/completions", "closed");
        let request = make_request("/v1/chat/completions");
        let mut ctx = make_ctx(&request, None);
        let mut body = Some(chat_body("hello"));

        let action = filter
            .on_request_body(&mut ctx, &mut body, false)
            .await
            .expect("partial chunks are never an error");

        assert!(
            matches!(action, FilterAction::Continue),
            "a partial chunk must pass through untouched"
        );
        assert!(
            ctx.get_metadata(METADATA_CLUSTER).is_none(),
            "no decision may be taken before the body is complete"
        );
    }

    #[tokio::test]
    async fn fails_open_when_routing_fails() {
        // No subrequest client in the context, so the judge can never be called.
        let filter = make_filter("http://127.0.0.1:1/v1/chat/completions", "open");
        let request = make_request("/v1/chat/completions");
        let mut ctx = make_ctx(&request, None);
        let mut body = Some(chat_body("hello"));

        let action = filter
            .on_request_body(&mut ctx, &mut body, true)
            .await
            .expect("fail-open never returns an error");

        assert!(
            matches!(action, FilterAction::Continue),
            "on_failure: open must let the request through"
        );
        assert_eq!(
            ctx.get_metadata("switchyard_route.error"),
            Some("subrequest client unavailable"),
            "the failure reason must be recorded for access logs"
        );
    }

    #[tokio::test]
    async fn fails_closed_when_routing_fails() {
        let filter = make_filter("http://127.0.0.1:1/v1/chat/completions", "closed");
        let request = make_request("/v1/chat/completions");
        let mut ctx = make_ctx(&request, None);
        let mut body = Some(chat_body("hello"));

        let action = filter
            .on_request_body(&mut ctx, &mut body, true)
            .await
            .expect("fail-closed rejects rather than erroring");

        let FilterAction::Reject(rejection) = action else {
            panic!("on_failure: closed must reject the request");
        };
        assert_eq!(
            rejection.status, 503,
            "a failed routing decision must surface as 503 Service Unavailable"
        );
    }

    #[tokio::test]
    async fn rejects_non_chat_completion_paths() {
        let filter = make_filter("http://127.0.0.1:1/v1/chat/completions", "closed");
        let request = make_request("/v1/embeddings");
        let mut ctx = make_ctx(&request, None);
        let mut body = Some(chat_body("hello"));

        let action = filter
            .on_request_body(&mut ctx, &mut body, true)
            .await
            .expect("an unsupported path is a routing failure, not an error");

        assert!(
            matches!(action, FilterAction::Reject(_)),
            "only /chat/completions can be routed"
        );
        assert_eq!(
            ctx.get_metadata("switchyard_route.error"),
            Some("unsupported path (only /chat/completions)"),
            "the recorded reason must name the path problem"
        );
    }

    #[tokio::test]
    async fn on_request_promotes_the_stashed_cluster() {
        let filter = make_filter("http://127.0.0.1:1/v1/chat/completions", "open");
        let request = make_request("/v1/chat/completions");
        let mut ctx = make_ctx(&request, None);
        ctx.set_metadata(METADATA_CLUSTER, "strong-cluster");

        let action = filter.on_request(&mut ctx).await.expect("on_request never fails");

        assert!(matches!(action, FilterAction::Continue), "on_request always continues");
        assert_eq!(
            ctx.cluster_name(),
            Some("strong-cluster"),
            "the body-phase decision must select the cluster"
        );
    }

    #[tokio::test]
    async fn on_request_leaves_the_cluster_alone_without_a_decision() {
        let filter = make_filter("http://127.0.0.1:1/v1/chat/completions", "open");
        let request = make_request("/v1/chat/completions");
        let mut ctx = make_ctx(&request, None);

        drop(filter.on_request(&mut ctx).await.expect("on_request never fails"));

        assert!(
            ctx.cluster_name().is_none(),
            "without a decision the router's own cluster choice must stand"
        );
    }

    // -----------------------------------------------------------------------
    // End-to-end routing against a mock judge
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn routes_to_the_weak_tier_when_the_judge_reports_supported() {
        let addr = spawn_judge("HTTP/1.1 200 OK", judge_body(&verdict(0.95, "SUP-1", "supported"))).await;
        let filter = make_filter(&format!("http://{addr}/v1/chat/completions"), "closed");
        let client = make_client();
        let request = make_request("/v1/chat/completions");
        let mut ctx = make_ctx(&request, Some(&client));
        let mut body = Some(chat_body("what is 2 + 2?"));

        let action = filter
            .on_request_body(&mut ctx, &mut body, true)
            .await
            .expect("routing succeeds");

        assert!(
            matches!(action, FilterAction::Continue),
            "a successful decision lets the request continue"
        );
        assert_eq!(
            ctx.get_metadata(METADATA_CLUSTER),
            Some("weak-cluster"),
            "a supported prompt must route to the weak tier"
        );
        assert_eq!(
            ctx.get_metadata(METADATA_DECISION),
            Some("routed"),
            "a live judge verdict must be recorded as a real route, not a fallback"
        );
        let routed: serde_json::Value =
            serde_json::from_slice(&body.expect("the body is rewritten in place")).expect("the body is JSON");
        assert_eq!(
            routed["model"], "weak-model",
            "the upstream must be asked for the weak tier's model"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn routes_to_the_strong_tier_when_the_judge_reports_unsupported() {
        let addr = spawn_judge("HTTP/1.1 200 OK", judge_body(&verdict(0.0, "LIM-2", "unsupported"))).await;
        let filter = make_filter(&format!("http://{addr}/v1/chat/completions"), "closed");
        let client = make_client();
        let request = make_request("/v1/chat/completions");
        let mut ctx = make_ctx(&request, Some(&client));
        let mut body = Some(chat_body("reverse-engineer this undocumented format"));

        drop(
            filter
                .on_request_body(&mut ctx, &mut body, true)
                .await
                .expect("routing succeeds"),
        );

        assert_eq!(
            ctx.get_metadata(METADATA_CLUSTER),
            Some("strong-cluster"),
            "an unsupported prompt must route to the strong tier"
        );
        let routed: serde_json::Value =
            serde_json::from_slice(&body.expect("the body is rewritten in place")).expect("the body is JSON");
        assert_eq!(
            routed["model"], "strong-model",
            "the upstream must be asked for the strong tier's model"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn judge_http_errors_are_reported_as_callout_failures() {
        let addr = spawn_judge("HTTP/1.1 500 Internal Server Error", "upstream exploded".to_owned()).await;
        let filter = make_filter(&format!("http://{addr}/v1/chat/completions"), "open");
        let client = make_client();
        let request = make_request("/v1/chat/completions");
        let mut ctx = make_ctx(&request, Some(&client));
        let mut body = Some(chat_body("hello"));

        let action = filter
            .on_request_body(&mut ctx, &mut body, true)
            .await
            .expect("fail-open never returns an error");

        assert!(
            matches!(action, FilterAction::Continue),
            "on_failure: open must let the request through"
        );
        let recorded = ctx
            .get_metadata("switchyard_route.error")
            .expect("the failure reason is recorded");
        assert!(
            recorded.contains("HTTP 500"),
            "the recorded reason must name the judge status, got {recorded}"
        );
        assert_eq!(
            ctx.get_metadata(METADATA_DECISION),
            Some("default_strong"),
            "with nothing remembered, a judge outage must fall back to the strong tier"
        );
        assert_eq!(
            ctx.get_metadata(METADATA_CLUSTER),
            Some("strong-cluster"),
            "the fallback must select the strong cluster"
        );
        let routed: serde_json::Value =
            serde_json::from_slice(&body.expect("the body is rewritten in place")).expect("the body is JSON");
        assert_eq!(
            routed["model"], "strong-model",
            "the fallback must rewrite the model, not just the cluster"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unreachable_judges_are_reported_as_callout_failures() {
        // Bind and immediately drop, so the port is almost certainly closed.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds an ephemeral port");
        let addr = listener.local_addr().expect("reports its address");
        drop(listener);

        let filter = make_filter(&format!("http://{addr}/v1/chat/completions"), "open");
        let client = make_client();
        let request = make_request("/v1/chat/completions");
        let mut ctx = make_ctx(&request, Some(&client));
        let mut body = Some(chat_body("hello"));

        drop(
            filter
                .on_request_body(&mut ctx, &mut body, true)
                .await
                .expect("fail-open never returns an error"),
        );

        let recorded = ctx
            .get_metadata("switchyard_route.error")
            .expect("the failure reason is recorded");
        assert!(
            recorded.starts_with("judge callout failed:"),
            "a refused connection must surface as a judge callout failure, got {recorded}"
        );
    }


    #[test]
    fn judge_failures_may_apply_a_fallback_tier() {
        assert!(
            RouteError::Judge("down".into()).may_apply_failure_tier(),
            "judge HTTP errors are the mid-session failure path"
        );
        assert!(
            RouteError::NoDecision.may_apply_failure_tier(),
            "a missing verdict is still a judge-path failure"
        );
        assert!(
            RouteError::MissingSubrequestClient.may_apply_failure_tier(),
            "no callout client is treated like an unreachable judge"
        );
    }

    #[test]
    fn bad_bodies_and_wrong_paths_stay_unrouted() {
        assert!(
            !RouteError::UnsupportedPath.may_apply_failure_tier(),
            "non-chat paths must not be rewritten to Strong"
        );
        assert!(
            !RouteError::Json("nope".into()).may_apply_failure_tier(),
            "invalid JSON cannot be rewritten"
        );
        assert!(
            !RouteError::Body("empty").may_apply_failure_tier(),
            "missing bodies cannot be rewritten"
        );
    }

    // -----------------------------------------------------------------------
    // Failure policy: reuse, default Strong, unrouted
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reuses_the_remembered_tier_when_the_judge_goes_down() {
        // Turn one: the judge answers, the weak tier is remembered for the session.
        // Turn two: the judge is down, so that remembered tier is served instead.
        let addr = spawn_judge_sequence(vec![
            ("HTTP/1.1 200 OK", judge_body(&verdict(0.95, "SUP-1", "supported"))),
            ("HTTP/1.1 500 Internal Server Error", "judge down".to_owned()),
        ])
        .await;
        let filter = make_filter(&format!("http://{addr}/v1/chat/completions"), "open");
        let client = make_client();
        let request = make_request_with_session("/v1/chat/completions", "session-alpha");

        let mut first_ctx = make_ctx(&request, Some(&client));
        let mut first_body = Some(chat_body("what is 2 + 2?"));
        drop(
            filter
                .on_request_body(&mut first_ctx, &mut first_body, true)
                .await
                .expect("the first turn routes normally"),
        );
        assert_eq!(
            first_ctx.get_metadata(METADATA_DECISION),
            Some("routed"),
            "the first turn must be a live judge decision"
        );
        assert_eq!(
            first_ctx.get_metadata(METADATA_CLUSTER),
            Some("weak-cluster"),
            "the first turn must route weak so there is a weak verdict to reuse"
        );

        let mut second_ctx = make_ctx(&request, Some(&client));
        let mut second_body = Some(chat_body("what is 3 + 3?"));
        let action = filter
            .on_request_body(&mut second_ctx, &mut second_body, true)
            .await
            .expect("fail-open never returns an error");

        assert!(
            matches!(action, FilterAction::Continue),
            "a reused tier must let the request through"
        );
        assert_eq!(
            second_ctx.get_metadata(METADATA_DECISION),
            Some("reuse"),
            "a judge outage on a known session must reuse the last success, not default to strong"
        );
        assert_eq!(
            second_ctx.get_metadata(METADATA_CLUSTER),
            Some("weak-cluster"),
            "the reused tier must select the same cluster the judge chose"
        );
        let routed: serde_json::Value =
            serde_json::from_slice(&second_body.expect("the body is rewritten in place")).expect("the body is JSON");
        assert_eq!(
            routed["model"], "weak-model",
            "reuse must rewrite the model to the remembered tier's target"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn does_not_reuse_across_different_sessions() {
        let addr = spawn_judge_sequence(vec![
            ("HTTP/1.1 200 OK", judge_body(&verdict(0.95, "SUP-1", "supported"))),
            ("HTTP/1.1 500 Internal Server Error", "judge down".to_owned()),
        ])
        .await;
        let filter = make_filter(&format!("http://{addr}/v1/chat/completions"), "open");
        let client = make_client();

        let known = make_request_with_session("/v1/chat/completions", "session-alpha");
        let mut first_ctx = make_ctx(&known, Some(&client));
        let mut first_body = Some(chat_body("what is 2 + 2?"));
        drop(
            filter
                .on_request_body(&mut first_ctx, &mut first_body, true)
                .await
                .expect("the first turn routes normally"),
        );

        let stranger = make_request_with_session("/v1/chat/completions", "session-beta");
        let mut second_ctx = make_ctx(&stranger, Some(&client));
        let mut second_body = Some(chat_body("something else entirely"));
        drop(
            filter
                .on_request_body(&mut second_ctx, &mut second_body, true)
                .await
                .expect("fail-open never returns an error"),
        );

        assert_eq!(
            second_ctx.get_metadata(METADATA_DECISION),
            Some("default_strong"),
            "one session's remembered tier must not leak into another"
        );
    }

    #[tokio::test]
    async fn unroutable_failures_pass_through_untouched() {
        // An unsupported path is not a rewritable chat request, so `open` must
        // pass it upstream exactly as it arrived rather than pinning a tier.
        let filter = make_filter("http://127.0.0.1:1/v1/chat/completions", "open");
        let request = make_request("/v1/embeddings");
        let mut ctx = make_ctx(&request, None);
        let original = chat_body("hello");
        let mut body = Some(original.clone());

        let action = filter
            .on_request_body(&mut ctx, &mut body, true)
            .await
            .expect("fail-open never returns an error");

        assert!(
            matches!(action, FilterAction::Continue),
            "on_failure: open must let an unroutable request through"
        );
        assert_eq!(
            ctx.get_metadata(METADATA_DECISION),
            Some("unrouted"),
            "a request that cannot be rewritten must be recorded as unrouted"
        );
        assert!(
            ctx.get_metadata(METADATA_CLUSTER).is_none(),
            "an unrouted request must not select a Switchyard cluster"
        );
        assert_eq!(
            body,
            Some(original),
            "an unrouted request must reach the upstream byte-for-byte unchanged"
        );
    }

    #[tokio::test]
    async fn fallback_rewrite_failure_passes_through_when_open() {
        // A JSON array parses, so routing gets as far as the judge path and fails
        // on translation -- which is may-apply -- but the fallback rewrite then
        // fails too, because an array has no `model` field to replace.
        let filter = make_filter("http://127.0.0.1:1/v1/chat/completions", "open");
        let request = make_request("/v1/chat/completions");
        let mut ctx = make_ctx(&request, None);
        let mut body = Some(Bytes::from_static(b"[1, 2, 3]"));

        let action = filter
            .on_request_body(&mut ctx, &mut body, true)
            .await
            .expect("fail-open never returns an error");

        assert!(
            matches!(action, FilterAction::Continue),
            "a failed fallback rewrite must still fail open"
        );
        assert_eq!(
            ctx.get_metadata(METADATA_DECISION),
            Some("unrouted"),
            "a body the fallback cannot rewrite must end up unrouted, not pinned to a tier"
        );
        assert_eq!(
            body,
            Some(Bytes::from_static(b"[1, 2, 3]")),
            "a failed rewrite must leave the body alone"
        );
    }

    #[tokio::test]
    async fn rewritable_failures_reject_when_closed() {
        let filter = make_filter("http://127.0.0.1:1/v1/chat/completions", "closed");
        let request = make_request("/v1/chat/completions");
        let mut ctx = make_ctx(&request, None);
        let mut body = Some(chat_body("hello"));

        let action = filter
            .on_request_body(&mut ctx, &mut body, true)
            .await
            .expect("fail-closed rejects rather than erroring");

        let FilterAction::Reject(rejection) = action else {
            panic!("on_failure: closed must reject even when a tier could be applied");
        };
        assert_eq!(rejection.status, 503, "a rejected request must surface as 503");
        assert_eq!(
            ctx.get_metadata(METADATA_DECISION),
            Some("rejected"),
            "a rejection must be recorded as such"
        );
    }

    // -----------------------------------------------------------------------
    // Session floor: skip judge when floor is at max
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn floor_skip_when_session_already_at_strong() {
        // Turn one: routes to Strong via the judge (unsupported prompt).
        // Turn two: with the same session, floor is Strong, so judge is skipped.
        let addr = spawn_judge_sequence(vec![
            ("HTTP/1.1 200 OK", judge_body(&verdict(0.0, "LIM-2", "unsupported"))),
            // Second response should never be reached due to floor_skip
            ("HTTP/1.1 500 Internal Server Error", "should not be called".to_owned()),
        ])
        .await;
        let filter = make_filter(&format!("http://{addr}/v1/chat/completions"), "open");
        let client = make_client();
        let request = make_request_with_session("/v1/chat/completions", "session-floor-test");

        // First turn: judge returns Strong
        let mut first_ctx = make_ctx(&request, Some(&client));
        let mut first_body = Some(chat_body("complex question"));
        drop(
            filter
                .on_request_body(&mut first_ctx, &mut first_body, true)
                .await
                .expect("first turn routes normally"),
        );
        assert_eq!(
            first_ctx.get_metadata(METADATA_DECISION),
            Some("routed"),
            "first turn must be a live judge decision"
        );
        assert_eq!(
            first_ctx.get_metadata(METADATA_CLUSTER),
            Some("strong-cluster"),
            "first turn must route to strong"
        );

        // Second turn: floor should skip the judge
        let mut second_ctx = make_ctx(&request, Some(&client));
        let mut second_body = Some(chat_body("simple follow-up"));
        let action = filter
            .on_request_body(&mut second_ctx, &mut second_body, true)
            .await
            .expect("second turn succeeds via floor");

        assert!(
            matches!(action, FilterAction::Continue),
            "floor_skip must let the request through"
        );
        assert_eq!(
            second_ctx.get_metadata(METADATA_DECISION),
            Some("floor_skip"),
            "second turn must skip the judge due to floor"
        );
        assert_eq!(
            second_ctx.get_metadata(METADATA_CLUSTER),
            Some("strong-cluster"),
            "floor_skip must route to the floor tier (Strong)"
        );
        let routed: serde_json::Value =
            serde_json::from_slice(&second_body.expect("body is rewritten")).expect("body is JSON");
        assert_eq!(
            routed["model"], "strong-model",
            "floor_skip must rewrite the model to the floor tier's target"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn floor_skip_rewrite_failure_follows_on_failure() {
        // A JSON array parses and keeps the session header, so floor-skip runs,
        // then rewrite_for_tier fails (not an object). Missing `model` still
        // rewrites; only a non-object hits this path.
        let addr = spawn_judge_sequence(vec![
            ("HTTP/1.1 200 OK", judge_body(&verdict(0.0, "LIM-2", "unsupported"))),
            ("HTTP/1.1 200 OK", judge_body(&verdict(0.0, "LIM-2", "unsupported"))),
            ("HTTP/1.1 500 Internal Server Error", "should not be called".to_owned()),
        ])
        .await;
        let endpoint = format!("http://{addr}/v1/chat/completions");
        let client = make_client();
        let unrewritable = Bytes::from_static(b"[1, 2, 3]");

        let open = make_filter(&endpoint, "open");
        let open_request = make_request_with_session("/v1/chat/completions", "session-floor-rewrite-open");
        seed_strong_floor(open.as_ref(), &open_request, &client).await;
        let mut open_ctx = make_ctx(&open_request, Some(&client));
        let mut open_body = Some(unrewritable.clone());
        let open_action = open
            .on_request_body(&mut open_ctx, &mut open_body, true)
            .await
            .expect("open floor-skip rewrite failure must not error");
        assert!(
            matches!(open_action, FilterAction::Continue),
            "on_failure: open must continue after a failed floor-skip rewrite"
        );
        assert_eq!(
            open_ctx.get_metadata(METADATA_DECISION),
            Some("unrouted"),
            "a failed floor-skip rewrite must fail open unrouted, not reuse Strong"
        );
        assert!(
            open_ctx.get_metadata(METADATA_CLUSTER).is_none(),
            "an unrouted floor-skip rewrite must not select a cluster"
        );
        assert_eq!(
            open_body,
            Some(unrewritable.clone()),
            "a failed rewrite must leave the body alone"
        );

        let closed = make_filter(&endpoint, "closed");
        let closed_request = make_request_with_session("/v1/chat/completions", "session-floor-rewrite-closed");
        seed_strong_floor(closed.as_ref(), &closed_request, &client).await;
        let mut closed_ctx = make_ctx(&closed_request, Some(&client));
        let mut closed_body = Some(unrewritable);
        let closed_action = closed
            .on_request_body(&mut closed_ctx, &mut closed_body, true)
            .await
            .expect("closed floor-skip rewrite failure must not error");
        let FilterAction::Reject(rejection) = closed_action else {
            panic!("on_failure: closed must reject when floor-skip rewrite fails");
        };
        assert_eq!(rejection.status, 503, "a rejected request must surface as 503");
        assert_eq!(
            closed_ctx.get_metadata(METADATA_DECISION),
            Some("rejected"),
            "a rejection must be recorded as such"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn floor_disabled_calls_judge_every_turn() {
        // With session_floor: disabled, every turn calls the judge even after Strong.
        let addr = spawn_judge_sequence(vec![
            ("HTTP/1.1 200 OK", judge_body(&verdict(0.0, "LIM-2", "unsupported"))),
            ("HTTP/1.1 200 OK", judge_body(&verdict(0.95, "SUP-1", "supported"))),
        ])
        .await;
        let filter = make_filter_with_floor(&format!("http://{addr}/v1/chat/completions"), "open", "disabled");
        let client = make_client();
        let request = make_request_with_session("/v1/chat/completions", "session-no-floor");

        // First turn: judge returns Strong
        let mut first_ctx = make_ctx(&request, Some(&client));
        let mut first_body = Some(chat_body("complex question"));
        drop(
            filter
                .on_request_body(&mut first_ctx, &mut first_body, true)
                .await
                .expect("first turn routes normally"),
        );
        assert_eq!(
            first_ctx.get_metadata(METADATA_CLUSTER),
            Some("strong-cluster"),
            "first turn routes to strong"
        );

        // Second turn: with floor disabled, judge is called and returns Weak
        let mut second_ctx = make_ctx(&request, Some(&client));
        let mut second_body = Some(chat_body("simple question"));
        drop(
            filter
                .on_request_body(&mut second_ctx, &mut second_body, true)
                .await
                .expect("second turn routes normally"),
        );

        assert_eq!(
            second_ctx.get_metadata(METADATA_DECISION),
            Some("routed"),
            "with floor disabled, judge is called every turn"
        );
        assert_eq!(
            second_ctx.get_metadata(METADATA_CLUSTER),
            Some("weak-cluster"),
            "with floor disabled, tier can downgrade to Weak"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn floor_disabled_reuse_follows_last_verdict() {
        // Disabled floor must not keep Strong in the map after a later Weak success.
        let addr = spawn_judge_sequence(vec![
            ("HTTP/1.1 200 OK", judge_body(&verdict(0.0, "LIM-2", "unsupported"))),
            ("HTTP/1.1 200 OK", judge_body(&verdict(0.95, "SUP-1", "supported"))),
            ("HTTP/1.1 500 Internal Server Error", "judge down".to_owned()),
        ])
        .await;
        let filter = make_filter_with_floor(&format!("http://{addr}/v1/chat/completions"), "open", "disabled");
        let client = make_client();
        let request = make_request_with_session("/v1/chat/completions", "session-disabled-reuse");

        let mut first_ctx = make_ctx(&request, Some(&client));
        let mut first_body = Some(chat_body("complex question"));
        drop(
            filter
                .on_request_body(&mut first_ctx, &mut first_body, true)
                .await
                .expect("first turn routes to Strong"),
        );
        let mut second_ctx = make_ctx(&request, Some(&client));
        let mut second_body = Some(chat_body("simple question"));
        drop(
            filter
                .on_request_body(&mut second_ctx, &mut second_body, true)
                .await
                .expect("second turn routes to Weak"),
        );

        let mut third_ctx = make_ctx(&request, Some(&client));
        let mut third_body = Some(chat_body("follow-up"));
        drop(
            filter
                .on_request_body(&mut third_ctx, &mut third_body, true)
                .await
                .expect("third turn fail-open reuses the last live verdict"),
        );
        assert_eq!(
            third_ctx.get_metadata(METADATA_DECISION),
            Some("reuse"),
            "judge failure must reuse the stored tier"
        );
        assert_eq!(
            third_ctx.get_metadata(METADATA_CLUSTER),
            Some("weak-cluster"),
            "disabled floor must reuse Weak after a Weak live success"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn floor_does_not_skip_judge_when_at_weak() {
        // When floor is at Weak, judge is still called (might escalate to Strong).
        let addr = spawn_judge_sequence(vec![
            ("HTTP/1.1 200 OK", judge_body(&verdict(0.95, "SUP-1", "supported"))),
            ("HTTP/1.1 200 OK", judge_body(&verdict(0.0, "LIM-2", "unsupported"))),
        ])
        .await;
        let filter = make_filter(&format!("http://{addr}/v1/chat/completions"), "open");
        let client = make_client();
        let request = make_request_with_session("/v1/chat/completions", "session-weak-then-strong");

        // First turn: judge returns Weak
        let mut first_ctx = make_ctx(&request, Some(&client));
        let mut first_body = Some(chat_body("simple question"));
        drop(
            filter
                .on_request_body(&mut first_ctx, &mut first_body, true)
                .await
                .expect("first turn routes normally"),
        );
        assert_eq!(
            first_ctx.get_metadata(METADATA_CLUSTER),
            Some("weak-cluster"),
            "first turn routes to weak"
        );

        // Second turn: floor is Weak, so judge is called and escalates to Strong
        let mut second_ctx = make_ctx(&request, Some(&client));
        let mut second_body = Some(chat_body("complex question"));
        drop(
            filter
                .on_request_body(&mut second_ctx, &mut second_body, true)
                .await
                .expect("second turn routes normally"),
        );

        assert_eq!(
            second_ctx.get_metadata(METADATA_DECISION),
            Some("routed"),
            "floor at Weak must still call the judge"
        );
        assert_eq!(
            second_ctx.get_metadata(METADATA_CLUSTER),
            Some("strong-cluster"),
            "session must escalate from Weak to Strong"
        );
    }
}

//! `switchyard_route`: Mixture-of-Models routing via NVIDIA `NeMo` Switchyard
//! (Capability mode). Decision-only: judge → weak/strong tier → cluster+model.
//!
//! Demo greps `judge verdict` / `routed` / `reuse` / `default_strong` /
//! `routing failed` / `fail-open` in `demos/switchyard-route/run-demo.sh`.

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

use self::config::{FailureMode, RouteConfig, Tier};

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
    async fn route(&self, ctx: &mut HttpFilterContext<'_>, body: &mut Option<Bytes>) -> Result<Tier, RouteError> {
        let value = parse_body(body.as_ref())?;

        // Detect wire format from path
        let path = ctx.request.uri.path();
        if !path.ends_with("/chat/completions") {
            return Err(RouteError::UnsupportedPath);
        }

        let session_key = session::session_key_from_request(&ctx.request.headers, &value);
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
            self.lock_sessions().remember(&key, tier, Instant::now());
        }
        Ok(tier)
    }

    /// Recovers from a poisoned mutex so one panicked request cannot stick the filter.
    fn lock_sessions(&self) -> MutexGuard<'_, session::SessionStore> {
        self.sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Looks up the last real judge success for this request's session key.
    fn lookup_remembered(&self, ctx: &HttpFilterContext<'_>, body: Option<&Bytes>) -> Option<Tier> {
        let value = parse_body(body).ok()?;
        let key = session::session_key_from_request(&ctx.request.headers, &value)?;
        self.lock_sessions().last_success(&key, Instant::now())
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
        let remembered = may_apply.then(|| self.lookup_remembered(ctx, body.as_ref())).flatten();
        self.dispatch_failure(ctx, body, may_apply, remembered)
    }

    /// Maps a failure disposition onto HTTP continue / 503 / rewrite.
    fn dispatch_failure(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        may_apply: bool,
        remembered: Option<Tier>,
    ) -> FilterAction {
        match failure::failure_action(self.config.on_failure, may_apply, remembered) {
            failure::FailureAction::Reject => reject_closed(ctx),
            failure::FailureAction::Unrouted => fail_open_unrouted(ctx),
            failure::FailureAction::Apply(apply) => self.apply_failure_tier(ctx, body, apply),
        }
    }

    /// Rewrites `model` and cluster metadata for an `open` failure apply.
    fn apply_failure_tier(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        apply: failure::FailureApply,
    ) -> FilterAction {
        let Ok(value) = parse_body(body.as_ref()) else {
            return fail_open_or_reject(ctx, self.config.on_failure);
        };
        match rewrite_for_tier(&self.config, body, value, apply.tier) {
            Ok(cluster) => {
                ctx.set_metadata(METADATA_CLUSTER, cluster);
                ctx.set_metadata(METADATA_DECISION, apply.kind.metadata());
                log_failure_apply(apply);
                FilterAction::Continue
            },
            Err(err) => {
                debug!(error = %err, "switchyard_route: fallback rewrite failed");
                fail_open_or_reject(ctx, self.config.on_failure)
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
                debug!(tier = %tier.tag(), "switchyard_route: routed");
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

/// When rewrite itself fails, fall back to plain open/closed.
fn fail_open_or_reject(ctx: &mut HttpFilterContext<'_>, mode: FailureMode) -> FilterAction {
    match mode {
        FailureMode::Closed => reject_closed(ctx),
        FailureMode::Open => fail_open_unrouted(ctx),
    }
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
mod tests {
    use praxis_filter::FilterRegistry;

    use super::RouteError;

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
}

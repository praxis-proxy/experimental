//! The `switchyard_route` filter: Capability-mode Mixture-of-Models routing.
//!
//! Embeds NVIDIA NeMo Switchyard's `LlmTaskClassifier` (Capability mode) in
//! the Praxis request path as a **decision-only** router: one cheap judge
//! callout classifies the request, Switchyard names an abstract tier tag
//! (`weak` / `strong`), and the filter resolves that tag through its own
//! config table into a real `(cluster, model)` pair — rewriting the body's
//! `model` field *and* selecting the cluster. The answer is served once, by
//! the real upstream; the filter never serves it.
//!
//! # The two-phase pipeline
//!
//! With `BodyMode::StreamBuffer`, `on_request_body` runs **before**
//! `on_request`. The decision and the body rewrite happen in the body phase;
//! the chosen cluster is stashed in filter metadata and applied to
//! `ctx.cluster` in `on_request`.
//!
//! # The no-downgrade guarantee
//!
//! Once a session routes to `strong` it must never silently drop to `weak`.
//! Switchyard v0.2.0 cannot provide this (its affinity is a
//! first-decision-wins latch and its state is neither durable nor seedable),
//! so the host owns it, in two layers:
//!
//! 1. **Don't overwrite on failure:** on *any* failure the request passes through with the client's own `model`
//!    untouched and no cluster set — the filter can never cause a downgrade by clobbering a good model.
//! 2. **Session floor** (`floor.rs`): an in-process `session → tier` ratchet; every decision is clamped to `max(floor,
//!    decision)`. Optionally the below-floor tier is also excluded inside Switchyard.
//!
//! A durable floor store (Redis/KV) is a planned follow-up; until then the
//! strict ratchet holds per replica and layer 1 covers state loss.
//!
//! # YAML
//!
//! ```yaml
//! filter: switchyard_route
//! judge:
//!   endpoint: "http://judge.internal:8000/v1/chat/completions"
//!   model: qwen3-judge
//!   timeout_ms: 2000
//!   max_response_bytes: 65536
//!   # Optional credential for a hosted judge. The secret lives only in the
//!   # environment; config names the variable holding it. Omit for a keyless
//!   # (e.g. local vLLM/Ollama) judge.
//!   auth:
//!     value_env: OPENAI_API_KEY   # env var holding the token
//!     header: authorization       # default: authorization
//!     scheme: Bearer              # default: Bearer ("" sends the raw value)
//! threshold: 0.5
//! targets:
//!   weak:
//!     cluster: local-vllm
//!     model: qwen2.5-7b
//!   strong:
//!     cluster: openai-frontier
//!     model: gpt-4o
//! session_floor:
//!   enabled: true
//!   ttl_secs: 3600
//!   exclude_below: true
//! on_failure: open
//! max_body_bytes: 1048576
//! session_header: x-switchyard-session-id
//! ```

mod algorithm;
mod config;
mod error;
mod floor;
mod judge;
mod peer;
mod steps;
mod translate;

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test-module suppressions")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::inconsistent_struct_constructor,
    reason = "unwrap/panic and long literals are acceptable in tests"
)]
mod tests;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytes::Bytes;
use praxis_filter::{BodyAccess, BodyMode, FilterAction, FilterError, HttpFilter, HttpFilterContext, Rejection};
use switchyard_libsy::Algorithm;
use switchyard_protocol::{Context, Metadata, Request, WireFormat};
use tracing::{debug, warn};

use self::{
    config::{FailureMode, RouteConfig, Tier},
    error::RouteError,
    floor::{InMemorySessionFloor, SessionFloorStore as _},
    peer::{JudgeEndpoint, SubRequestJudge},
};

/// Metadata key carrying the chosen cluster from the body phase to `on_request`.
const METADATA_CLUSTER: &str = "switchyard_route.cluster";
/// Metadata key recording the chosen tier tag.
const METADATA_TIER: &str = "switchyard_route.tier";
/// Metadata key recording the model written into the body.
const METADATA_MODEL: &str = "switchyard_route.model";
/// Metadata key recording why routing was skipped.
const METADATA_ERROR: &str = "switchyard_route.error";
/// Header that marks a session's final turn and evicts its floor.
const SESSION_FINAL_HEADER: &str = "x-switchyard-session-final";
/// Byte cap for the error metadata value (praxis drops larger values).
const METADATA_VALUE_LIMIT: usize = 256;

/// The `switchyard_route` HTTP filter.
pub(crate) struct SwitchyardRouteFilter {
    /// Validated filter configuration.
    config: RouteConfig,
    /// The Capability-mode classifier, built once at config time.
    algorithm: Arc<dyn Algorithm>,
    /// The judge endpoint, parsed once at config time.
    judge_endpoint: JudgeEndpoint,
    /// Host-owned no-downgrade ratchet; `None` when disabled.
    floor: Option<InMemorySessionFloor>,
}

impl SwitchyardRouteFilter {
    /// Creates the filter from parsed YAML config.
    ///
    /// # Errors
    ///
    /// Returns a [`FilterError`] when the YAML is invalid or Switchyard
    /// rejects the classifier configuration.
    pub(crate) fn from_config(config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let parsed = config::parse_config(config)?;
        let algorithm = algorithm::build_algorithm(&parsed)?;
        let judge_endpoint = JudgeEndpoint::from_config(&parsed.judge)?;
        let floor = parsed
            .session_floor
            .enabled
            .then(|| InMemorySessionFloor::new(Duration::from_secs(parsed.session_floor.ttl_secs)));
        Ok(Box::new(Self {
            config: parsed,
            algorithm,
            judge_endpoint,
            floor,
        }))
    }

    /// Runs the full routing pass: parse, decide, clamp, rewrite, stash.
    async fn route(&self, ctx: &mut HttpFilterContext<'_>, body: &mut Option<Bytes>) -> Result<(), RouteError> {
        let mut value = parse_body(body.as_ref())?;
        let format = translate::detect_format(ctx.request.uri.path(), &value)?;
        let session = self.session_id(ctx);
        let now = Instant::now();
        let floor = self.session_floor(session.as_deref(), now);
        let tag = self.decide(ctx, format, &value, floor).await?;
        let tier = Tier::from_tag(&tag).ok_or(RouteError::UnknownTier(tag))?;
        let chosen = floor.map_or(tier, |held| held.max(tier));
        self.commit_floor(ctx, session.as_deref(), chosen, now);
        self.apply(ctx, body, &mut value, chosen)
    }

    /// Runs Switchyard's step stream to a routed decision tag.
    async fn decide(
        &self,
        ctx: &HttpFilterContext<'_>,
        format: WireFormat,
        value: &serde_json::Value,
        floor: Option<Tier>,
    ) -> Result<String, RouteError> {
        let client = ctx.subrequest_client.ok_or(RouteError::MissingSubrequestClient)?;
        let llm_request = translate::decode_for_judge(format, value)?;
        let request = Request {
            llm_request,
            raw_request: None,
            metadata: Some(Metadata::from_headers(&ctx.request.headers)),
        };
        let mut switchyard_context = Context::default();
        if floor == Some(Tier::Strong) && self.config.session_floor.exclude_below {
            switchyard_context.exclude_target(Tier::Weak.tag());
        }
        let transport = SubRequestJudge::new(client, &self.judge_endpoint, &self.config.judge);
        let stream = Arc::clone(&self.algorithm).run_stream(switchyard_context, request, None);
        steps::decide(stream, &self.config.judge.model, &transport).await
    }

    /// Applies a clamped decision: rewrites the body model and stashes the
    /// cluster (plus observability keys) in filter metadata.
    fn apply(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        value: &mut serde_json::Value,
        chosen: Tier,
    ) -> Result<(), RouteError> {
        let target = self.config.targets.tier(chosen);
        let bytes = translate::rewrite_model(value, &target.model)?;
        *body = Some(Bytes::from(bytes));
        ctx.set_metadata(METADATA_CLUSTER, target.cluster.clone());
        ctx.set_metadata(METADATA_TIER, chosen.tag());
        ctx.set_metadata(METADATA_MODEL, target.model.clone());
        debug!(
            tier = chosen.tag(),
            cluster = %target.cluster,
            model = %target.model,
            "switchyard_route: routed"
        );
        Ok(())
    }

    /// Reads the session id from the configured header, if present.
    fn session_id(&self, ctx: &HttpFilterContext<'_>) -> Option<String> {
        let value = ctx.request.headers.get(self.config.session_header.as_str())?;
        let text = value.to_str().ok()?.trim();
        (!text.is_empty()).then(|| text.to_owned())
    }

    /// The live floor for this session, when the ratchet is enabled.
    fn session_floor(&self, session: Option<&str>, now: Instant) -> Option<Tier> {
        self.floor.as_ref()?.floor(session?, now)
    }

    /// Ratchets the floor after a successful decision, or evicts it on the
    /// session's final turn.
    fn commit_floor(&self, ctx: &HttpFilterContext<'_>, session: Option<&str>, chosen: Tier, now: Instant) {
        let (Some(store), Some(session)) = (self.floor.as_ref(), session) else {
            return;
        };
        if session_final(&ctx.request.headers) {
            store.evict(session);
        } else {
            store.commit(session, chosen, now);
        }
    }

    /// The single failure path: pass through unmodified (open) or 503 (closed).
    fn fail(&self, ctx: &mut HttpFilterContext<'_>, route_error: &RouteError) -> FilterAction {
        warn!(error = %route_error, "switchyard_route: routing unavailable");
        match self.config.on_failure {
            FailureMode::Open => {
                ctx.set_metadata(METADATA_ERROR, truncate(&route_error.to_string(), METADATA_VALUE_LIMIT));
                FilterAction::Continue
            },
            FailureMode::Closed => {
                FilterAction::Reject(Rejection::status(503).with_body("switchyard_route: routing unavailable"))
            },
        }
    }
}

#[async_trait]
impl HttpFilter for SwitchyardRouteFilter {
    fn name(&self) -> &'static str {
        "switchyard_route"
    }

    /// This filter satisfies the pipeline's "cluster-selecting filter before
    /// `load_balancer`" requirement.
    fn selects_cluster(&self) -> bool {
        true
    }

    fn selected_clusters(&self) -> Vec<String> {
        let weak = self.config.targets.weak.cluster.clone();
        let strong = self.config.targets.strong.cluster.clone();
        if weak == strong { vec![weak] } else { vec![weak, strong] }
    }

    fn needs_request_context(&self) -> bool {
        true
    }

    fn request_body_access(&self) -> BodyAccess {
        BodyAccess::ReadWrite
    }

    fn request_body_mode(&self) -> BodyMode {
        BodyMode::StreamBuffer {
            max_bytes: Some(self.config.max_body_bytes),
        }
    }

    async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        if ctx.cluster.is_some() {
            debug!("switchyard_route: cluster already set; preserving");
            return Ok(FilterAction::Continue);
        }
        let cluster: Option<Arc<str>> = ctx.get_metadata(METADATA_CLUSTER).map(Arc::from);
        if let Some(cluster) = cluster {
            ctx.cluster = Some(cluster);
        }
        Ok(FilterAction::Continue)
    }

    async fn on_request_body(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        if !end_of_stream {
            return Ok(FilterAction::Continue);
        }
        match self.route(ctx, body).await {
            Ok(()) => Ok(FilterAction::Continue),
            Err(route_error) => Ok(self.fail(ctx, &route_error)),
        }
    }
}

/// Parses the buffered body into a JSON object.
fn parse_body(body: Option<&Bytes>) -> Result<serde_json::Value, RouteError> {
    let raw = body
        .filter(|bytes| !bytes.is_empty())
        .ok_or(RouteError::Body("is missing or empty"))?;
    let value: serde_json::Value = serde_json::from_slice(raw).map_err(|err| RouteError::Json(err.to_string()))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(RouteError::Body("is not a JSON object"))
    }
}

/// Whether this turn is marked as the session's last.
fn session_final(headers: &http::HeaderMap) -> bool {
    headers
        .get(SESSION_FINAL_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|text| matches!(text.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// Truncates to at most `max_bytes` bytes on a character boundary.
fn truncate(text: &str, max_bytes: usize) -> String {
    let mut out = String::new();
    for character in text.chars() {
        let Some(total) = out.len().checked_add(character.len_utf8()) else {
            break;
        };
        if total > max_bytes {
            break;
        }
        out.push(character);
    }
    out
}

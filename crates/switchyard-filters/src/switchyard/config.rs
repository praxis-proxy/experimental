//! Configuration surface for the `switchyard_route` filter.
//!
//! The tag→(cluster, model) table is the heart of the filter: Switchyard only
//! ever sees the abstract tier tags (`weak` / `strong` / `judge`), and this
//! module owns their resolution to a real Praxis cluster and upstream model
//! name. Swapping what `strong` means is a config-only change.

use std::time::Duration;

use praxis_filter::FilterError;
use serde::Deserialize;

/// Longest permitted session-floor TTL (7 days), mirroring the bounded-TTL
/// convention of the stock `intelligent_route` session affinity.
const MAX_TTL_SECS: u64 = 604_800;

/// Longest permitted judge callout deadline (10 minutes).
const MAX_TIMEOUT_MS: u64 = 600_000;

/// The abstract routing tier ladder, ordered weakest first.
///
/// The `Ord` derive is load-bearing: the session-floor clamp
/// (`max(floor, decision)`) relies on `Weak < Strong`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Tier {
    /// The efficient (cheaper) tier, fed to Switchyard as `efficient_target`.
    Weak,
    /// The capable (stronger) tier, fed to Switchyard as `capable_target`.
    Strong,
}

impl Tier {
    /// The Switchyard `semantic_name` tag for this tier.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Weak => "weak",
            Self::Strong => "strong",
        }
    }

    /// Parses a Switchyard `semantic_name` tag back into a tier.
    pub(crate) fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "weak" => Some(Self::Weak),
            "strong" => Some(Self::Strong),
            _ => None,
        }
    }
}

/// Judge callout authentication.
///
/// A hosted OpenAI-compatible judge (OpenAI, Together, Fireworks, …) requires
/// a bearer token. The secret itself is **never** written into YAML: config
/// names the environment variable holding it (`value_env`), and the value is
/// resolved once at filter construction time so a missing secret fails fast
/// rather than silently sending an unauthenticated callout.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JudgeAuthConfig {
    /// Environment variable holding the credential (e.g. `OPENAI_API_KEY`).
    pub(crate) value_env: String,
    /// Header the credential is sent in.
    #[serde(default = "default_auth_header")]
    pub(crate) header: String,
    /// Prefix prepended to the resolved value (e.g. `Bearer`). An empty string
    /// sends the raw credential; a trailing space is added automatically.
    #[serde(default = "default_auth_scheme")]
    pub(crate) scheme: String,
}

/// Judge (classifier) callout configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JudgeConfig {
    /// Absolute URL of the judge's OpenAI-compatible chat-completions endpoint.
    pub(crate) endpoint: String,
    /// Model name substituted into the classifier request sent to the judge.
    pub(crate) model: String,
    /// Optional credential for a hosted judge; absent for keyless endpoints.
    #[serde(default)]
    pub(crate) auth: Option<JudgeAuthConfig>,
    /// Deadline in milliseconds covering DNS resolution and the HTTP exchange.
    #[serde(default = "default_judge_timeout_ms")]
    pub(crate) timeout_ms: u64,
    /// Maximum accepted judge response size in bytes.
    #[serde(default = "default_judge_max_response_bytes")]
    pub(crate) max_response_bytes: usize,
}

impl JudgeConfig {
    /// The callout deadline as a [`Duration`].
    pub(crate) fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
}

/// A single routing tier's resolution: which Praxis cluster to dial and which
/// real model name to write into the forwarded request body.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetConfig {
    /// Praxis cluster name assigned to `ctx.cluster`.
    pub(crate) cluster: String,
    /// Upstream model name written into the request body's `model` field.
    pub(crate) model: String,
}

/// The tag→(cluster, model) table. Exactly two tiers: Capability mode is a
/// binary efficient/capable split by construction.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetsConfig {
    /// Resolution for the `weak` (efficient) tier.
    pub(crate) weak: TargetConfig,
    /// Resolution for the `strong` (capable) tier.
    pub(crate) strong: TargetConfig,
}

impl TargetsConfig {
    /// Resolves a tier to its configured target.
    pub(crate) fn tier(&self, tier: Tier) -> &TargetConfig {
        match tier {
            Tier::Weak => &self.weak,
            Tier::Strong => &self.strong,
        }
    }
}

/// The host-owned no-downgrade ratchet (session floor) configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionFloorConfig {
    /// Whether the in-process session floor is maintained at all.
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    /// Inactivity eviction window in seconds (aligns with Switchyard's 1h).
    #[serde(default = "default_floor_ttl_secs")]
    pub(crate) ttl_secs: u64,
    /// Also bar the below-floor tier inside Switchyard via
    /// `Context::exclude_target` (belt-and-suspenders with the post-hoc clamp).
    #[serde(default = "default_true")]
    pub(crate) exclude_below: bool,
}

impl Default for SessionFloorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl_secs: default_floor_ttl_secs(),
            exclude_below: true,
        }
    }
}

/// What to do with the request when a routing decision cannot be produced.
///
/// Configured via the `on_failure` key — NOT `failure_mode`, which is a
/// structural key of Praxis's pipeline `FilterEntry` wrapper and is stripped
/// by `parse_filter_config` before the filter ever sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FailureMode {
    /// Pass the request through **unmodified** — never force a tier, so a
    /// broken optimizer can neither 5xx the request nor downgrade a session.
    Open,
    /// Reject with 503 for deployments that would rather fail than route
    /// un-optimized.
    Closed,
}

/// Parsed and validated `switchyard_route` configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RouteConfig {
    /// The classifier callout — required; the judge is the whole point.
    pub(crate) judge: JudgeConfig,
    /// Capability classifier decision threshold (`base_threshold`).
    #[serde(default = "default_threshold")]
    pub(crate) threshold: f64,
    /// The tag→(cluster, model) resolution table.
    pub(crate) targets: TargetsConfig,
    /// Host-owned no-downgrade ratchet settings.
    #[serde(default)]
    pub(crate) session_floor: SessionFloorConfig,
    /// Behaviour when no routing decision can be produced (see [`FailureMode`]
    /// for why the key is `on_failure`).
    #[serde(default = "default_failure_mode")]
    pub(crate) on_failure: FailureMode,
    /// `StreamBuffer` cap for the buffered request body, in bytes.
    #[serde(default = "default_max_body_bytes")]
    pub(crate) max_body_bytes: usize,
    /// Header carrying the session id the floor is keyed by.
    #[serde(default = "default_session_header")]
    pub(crate) session_header: String,
}

/// Serde default: `true`.
fn default_true() -> bool {
    true
}

/// Serde default for [`JudgeAuthConfig::header`]: `authorization`.
fn default_auth_header() -> String {
    "authorization".to_owned()
}

/// Serde default for [`JudgeAuthConfig::scheme`]: `Bearer`.
fn default_auth_scheme() -> String {
    "Bearer".to_owned()
}

/// Serde default for [`JudgeConfig::timeout_ms`]: 2 seconds.
fn default_judge_timeout_ms() -> u64 {
    2_000
}

/// Serde default for [`JudgeConfig::max_response_bytes`]: 64 KiB.
fn default_judge_max_response_bytes() -> usize {
    65_536
}

/// Serde default for [`RouteConfig::threshold`].
fn default_threshold() -> f64 {
    0.5
}

/// Serde default for [`SessionFloorConfig::ttl_secs`]: 1 hour.
fn default_floor_ttl_secs() -> u64 {
    3_600
}

/// Serde default for [`RouteConfig::on_failure`]: fail open.
fn default_failure_mode() -> FailureMode {
    FailureMode::Open
}

/// Serde default for [`RouteConfig::max_body_bytes`]: 1 MiB.
fn default_max_body_bytes() -> usize {
    1_048_576
}

/// Serde default for [`RouteConfig::session_header`]: Switchyard's native
/// session id header.
fn default_session_header() -> String {
    "x-switchyard-session-id".to_owned()
}

/// Builds a config-shaped [`FilterError`].
fn config_error(message: &str) -> FilterError {
    format!("switchyard_route: {message}").into()
}

/// Validates the judge callout settings.
///
/// # Errors
///
/// Returns a [`FilterError`] when the endpoint is not an absolute http(s) URL,
/// the model is empty, or a limit is out of range.
fn validate_judge(judge: &JudgeConfig) -> Result<(), FilterError> {
    let uri: http::Uri = judge
        .endpoint
        .parse()
        .map_err(|parse_error| config_error(&format!("judge.endpoint is not a valid URL: {parse_error}")))?;
    match uri.scheme_str() {
        Some("http" | "https") => {},
        Some(other) => {
            return Err(config_error(&format!(
                "judge.endpoint has unsupported scheme '{other}'"
            )));
        },
        None => return Err(config_error("judge.endpoint must be an absolute http(s) URL")),
    }
    if uri.authority().is_none() {
        return Err(config_error("judge.endpoint is missing a host"));
    }
    if judge.model.trim().is_empty() {
        return Err(config_error("judge.model must not be empty"));
    }
    if judge.timeout_ms == 0 || judge.timeout_ms > MAX_TIMEOUT_MS {
        return Err(config_error(&format!("judge.timeout_ms must be 1-{MAX_TIMEOUT_MS}")));
    }
    if judge.max_response_bytes == 0 {
        return Err(config_error("judge.max_response_bytes must be at least 1"));
    }
    if let Some(auth) = judge.auth.as_ref() {
        validate_auth(auth)?;
    }
    Ok(())
}

/// Validates the judge authentication block.
///
/// # Errors
///
/// Returns a [`FilterError`] when the env var name is empty or the header name
/// is not a valid HTTP header. The credential value is resolved (and its
/// presence checked) later, at filter construction time.
fn validate_auth(auth: &JudgeAuthConfig) -> Result<(), FilterError> {
    if auth.value_env.trim().is_empty() {
        return Err(config_error("judge.auth.value_env must not be empty"));
    }
    if http::header::HeaderName::from_bytes(auth.header.as_bytes()).is_err() {
        return Err(config_error("judge.auth.header is not a valid header name"));
    }
    Ok(())
}

/// Validates one tier's target table entry.
///
/// # Errors
///
/// Returns a [`FilterError`] when the cluster or model is empty.
fn validate_target(tier: Tier, target: &TargetConfig) -> Result<(), FilterError> {
    if target.cluster.trim().is_empty() {
        return Err(config_error(&format!(
            "targets.{}.cluster must not be empty",
            tier.tag()
        )));
    }
    if target.model.trim().is_empty() {
        return Err(config_error(&format!("targets.{}.model must not be empty", tier.tag())));
    }
    Ok(())
}

/// Validates the remaining scalar settings.
///
/// # Errors
///
/// Returns a [`FilterError`] when the threshold, floor TTL, body cap, or
/// session header is out of range or malformed.
fn validate_scalars(config: &RouteConfig) -> Result<(), FilterError> {
    if !(0.0..=1.0).contains(&config.threshold) {
        return Err(config_error("threshold must be within [0, 1]"));
    }
    if config.session_floor.enabled
        && (config.session_floor.ttl_secs == 0 || config.session_floor.ttl_secs > MAX_TTL_SECS)
    {
        return Err(config_error(&format!(
            "session_floor.ttl_secs must be 1-{MAX_TTL_SECS}"
        )));
    }
    if config.max_body_bytes == 0 {
        return Err(config_error("max_body_bytes must be at least 1"));
    }
    if http::header::HeaderName::from_bytes(config.session_header.as_bytes()).is_err() {
        return Err(config_error("session_header is not a valid header name"));
    }
    Ok(())
}

/// Parses and validates the filter's YAML configuration.
///
/// # Errors
///
/// Returns a [`FilterError`] when the YAML does not deserialize into
/// [`RouteConfig`] or any field fails validation.
pub(crate) fn parse_config(config: &serde_yaml::Value) -> Result<RouteConfig, FilterError> {
    let parsed: RouteConfig = praxis_filter::parse_filter_config("switchyard_route", config)?;
    validate_judge(&parsed.judge)?;
    validate_target(Tier::Weak, &parsed.targets.weak)?;
    validate_target(Tier::Strong, &parsed.targets.strong)?;
    validate_scalars(&parsed)?;
    Ok(parsed)
}

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test-module suppressions")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "unwrap/panic are acceptable in tests"
)]
mod tests {
    use super::{FailureMode, RouteConfig, Tier, parse_config};

    /// A minimal valid YAML config exercising the serde defaults.
    const MINIMAL: &str = "
judge:
  endpoint: http://judge.internal:8000/v1/chat/completions
  model: qwen3-judge
targets:
  weak:
    cluster: local-vllm
    model: qwen2.5-7b
  strong:
    cluster: openai-frontier
    model: gpt-4o
";

    /// Parses YAML into a config, panicking on failure.
    fn parse(yaml: &str) -> RouteConfig {
        let value: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        parse_config(&value).unwrap()
    }

    /// Parses YAML expecting a validation failure, returning the message.
    fn parse_err(yaml: &str) -> String {
        let value: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        parse_config(&value).unwrap_err().to_string()
    }

    #[test]
    fn minimal_config_applies_defaults() {
        let config = parse(MINIMAL);
        assert!(
            (config.threshold - 0.5).abs() < f64::EPSILON,
            "default threshold is 0.5"
        );
        assert_eq!(config.on_failure, FailureMode::Open, "default failure mode is open");
        assert_eq!(config.max_body_bytes, 1_048_576, "default body cap is 1 MiB");
        assert_eq!(
            config.session_header, "x-switchyard-session-id",
            "default session header"
        );
        assert!(config.session_floor.enabled, "session floor defaults on");
        assert_eq!(config.session_floor.ttl_secs, 3_600, "default floor TTL is 1h");
        assert!(config.session_floor.exclude_below, "exclude_below defaults on");
        assert_eq!(config.judge.timeout_ms, 2_000, "default judge timeout");
        assert_eq!(config.judge.max_response_bytes, 65_536, "default judge response cap");
    }

    #[test]
    fn full_config_round_trips() {
        let yaml = "
judge:
  endpoint: https://judge.internal/v1/chat/completions
  model: judge-model
  timeout_ms: 500
  max_response_bytes: 1024
threshold: 0.75
targets:
  weak: { cluster: c-weak, model: m-weak }
  strong: { cluster: c-strong, model: m-strong }
session_floor:
  enabled: true
  ttl_secs: 60
  exclude_below: false
on_failure: closed
max_body_bytes: 2048
session_header: x-my-session
";
        let config = parse(yaml);
        assert_eq!(config.on_failure, FailureMode::Closed, "failure mode parsed");
        assert_eq!(config.targets.tier(Tier::Weak).cluster, "c-weak", "weak cluster parsed");
        assert_eq!(
            config.targets.tier(Tier::Strong).model,
            "m-strong",
            "strong model parsed"
        );
        assert!(!config.session_floor.exclude_below, "exclude_below parsed");
        assert_eq!(config.session_header, "x-my-session", "session header parsed");
    }

    #[test]
    fn missing_judge_is_rejected() {
        let yaml = "
targets:
  weak: { cluster: a, model: b }
  strong: { cluster: c, model: d }
";
        let message = parse_err(yaml);
        assert!(
            message.contains("judge"),
            "error should name the missing field: {message}"
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let yaml = format!("{MINIMAL}\nunknown_key: true\n");
        let message = parse_err(&yaml);
        assert!(message.contains("unknown"), "unknown keys must be rejected: {message}");
    }

    #[test]
    fn out_of_range_threshold_is_rejected() {
        let yaml = format!("{MINIMAL}\nthreshold: 1.5\n");
        let message = parse_err(&yaml);
        assert!(message.contains("threshold"), "threshold must be validated: {message}");
    }

    #[test]
    fn bad_judge_endpoint_is_rejected() {
        let yaml = MINIMAL.replace("http://judge.internal:8000/v1/chat/completions", "judge.internal");
        let message = parse_err(&yaml);
        assert!(
            message.contains("endpoint"),
            "relative endpoint must be rejected: {message}"
        );
    }

    #[test]
    fn empty_target_model_is_rejected() {
        let yaml = MINIMAL.replace("model: gpt-4o", "model: \"\"");
        let message = parse_err(&yaml);
        assert!(
            message.contains("targets.strong.model"),
            "empty target model must be rejected: {message}"
        );
    }

    #[test]
    fn zero_floor_ttl_is_rejected() {
        let yaml = format!("{MINIMAL}\nsession_floor:\n  ttl_secs: 0\n");
        let message = parse_err(&yaml);
        assert!(message.contains("ttl_secs"), "zero TTL must be rejected: {message}");
    }

    /// A valid config whose judge carries an auth block; `{AUTH}` is the auth
    /// body, indented under `judge:`.
    fn with_auth(auth_body: &str) -> String {
        format!(
            "judge:\n  endpoint: https://api.openai.com/v1/chat/completions\n  model: gpt-4o-mini\n  auth:\n{auth_body}targets:\n  weak: {{ cluster: a, model: b }}\n  strong: {{ cluster: c, model: d }}\n"
        )
    }

    #[test]
    fn judge_auth_round_trips_with_defaults() {
        let config = parse(&with_auth("    value_env: OPENAI_API_KEY\n"));
        let auth = config.judge.auth.expect("auth block parsed");
        assert_eq!(auth.value_env, "OPENAI_API_KEY", "env var name parsed");
        assert_eq!(auth.header, "authorization", "header defaults to authorization");
        assert_eq!(auth.scheme, "Bearer", "scheme defaults to Bearer");
    }

    #[test]
    fn empty_auth_env_is_rejected() {
        let message = parse_err(&with_auth("    value_env: \"\"\n"));
        assert!(
            message.contains("value_env"),
            "empty credential env var must be rejected: {message}"
        );
    }

    #[test]
    fn invalid_auth_header_is_rejected() {
        let message = parse_err(&with_auth(
            "    value_env: OPENAI_API_KEY\n    header: \"bad header\"\n",
        ));
        assert!(
            message.contains("auth.header"),
            "invalid auth header name must be rejected: {message}"
        );
    }

    #[test]
    fn invalid_session_header_is_rejected() {
        let yaml = format!("{MINIMAL}\nsession_header: \"bad header\"\n");
        let message = parse_err(&yaml);
        assert!(
            message.contains("session_header"),
            "invalid header must be rejected: {message}"
        );
    }

    #[test]
    fn tier_order_and_tags_round_trip() {
        assert!(Tier::Weak < Tier::Strong, "the floor clamp relies on Weak < Strong");
        assert_eq!(Tier::from_tag("weak"), Some(Tier::Weak), "weak tag round-trips");
        assert_eq!(Tier::from_tag("strong"), Some(Tier::Strong), "strong tag round-trips");
        assert_eq!(Tier::from_tag("judge"), None, "non-tier tags map to None");
        assert_eq!(
            Tier::Weak.max(Tier::Strong),
            Tier::Strong,
            "max picks the stronger tier"
        );
    }
}

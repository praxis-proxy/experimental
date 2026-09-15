//! Configuration for the `switchyard_route` filter.

// Under `cfg(test)` this lint may not fire, which would leave a module-level
// `expect` unfulfilled; only suppress when compiling the library normally.
#![cfg_attr(
    not(test),
    expect(
        clippy::missing_docs_in_private_items,
        reason = "config types are straightforward data containers"
    )
)]

use praxis_filter::FilterError;
use serde::Deserialize;

/// Validated filter configuration.
#[derive(Debug, Clone)]
pub(crate) struct RouteConfig {
    pub(crate) judge: JudgeConfig,
    pub(crate) weak: TargetConfig,
    pub(crate) strong: TargetConfig,
    pub(crate) threshold: f64,
    pub(crate) on_failure: FailureMode,
    pub(crate) session_floor: SessionFloor,
}

impl RouteConfig {
    pub(crate) fn target(&self, tier: Tier) -> &TargetConfig {
        match tier {
            Tier::Weak => &self.weak,
            Tier::Strong => &self.strong,
        }
    }
}

/// Judge (classifier LLM) callout settings.
#[derive(Clone)]
pub(crate) struct JudgeConfig {
    pub(crate) endpoint: String,
    pub(crate) model: String,
    pub(crate) timeout_ms: u64,
    pub(crate) verify_tls: bool,
    /// Bearer token from `auth.value_env` at startup, if configured.
    pub(crate) auth_token: Option<String>,
}

impl std::fmt::Debug for JudgeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JudgeConfig")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("timeout_ms", &self.timeout_ms)
            .field("verify_tls", &self.verify_tls)
            .field("auth_token", &self.auth_token.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// Target cluster + model for a tier.
#[derive(Debug, Clone)]
pub(crate) struct TargetConfig {
    pub(crate) cluster: String,
    pub(crate) model: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FailureMode {
    /// Pass through unchanged on failure.
    #[default]
    Open,
    /// Reject with 503 on failure.
    Closed,
}

/// Whether to enforce a session floor that prevents tier downgrades.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SessionFloor {
    /// Once a session reaches Strong, it stays Strong (skip judge if already maxed).
    #[default]
    Enabled,
    /// Each turn gets a fresh judge decision; no floor enforced.
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Tier {
    Weak,
    Strong,
}

impl Tier {
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Weak => "weak",
            Self::Strong => "strong",
        }
    }

    pub(crate) fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "weak" => Some(Self::Weak),
            "strong" => Some(Self::Strong),
            _ => None,
        }
    }

    /// Returns `true` if this is the highest tier (no point calling the judge).
    pub(crate) fn is_max(self) -> bool {
        self == Self::Strong
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    judge: RawJudge,
    targets: RawTargets,
    #[serde(default = "default_threshold")]
    threshold: f64,
    #[serde(default)]
    on_failure: FailureMode,
    #[serde(default)]
    session_floor: SessionFloor,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawJudge {
    endpoint: String,
    model: String,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
    #[serde(default = "default_verify_tls")]
    verify_tls: bool,
    #[serde(default)]
    auth: Option<RawJudgeAuth>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawJudgeAuth {
    value_env: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTargets {
    weak: RawTarget,
    strong: RawTarget,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTarget {
    cluster: String,
    model: String,
}

fn default_threshold() -> f64 {
    0.5
}

fn default_timeout() -> u64 {
    5000
}

fn default_verify_tls() -> bool {
    true
}

/// Parses and validates YAML config.
pub(crate) fn parse(yaml: &serde_yaml::Value) -> Result<RouteConfig, FilterError> {
    let raw: RawConfig = serde_yaml::from_value(yaml.clone()).map_err(|err| FilterError::from(err.to_string()))?;

    if !(0.0..=1.0).contains(&raw.threshold) {
        return Err(FilterError::from("threshold must be between 0.0 and 1.0"));
    }
    if raw.judge.endpoint.parse::<http::Uri>().is_err() {
        return Err(FilterError::from("judge.endpoint is not a valid URL"));
    }

    let auth_token = match raw.judge.auth {
        None => None,
        Some(auth) => {
            if auth.value_env.is_empty() {
                return Err(FilterError::from("judge.auth.value_env must not be empty"));
            }
            let value = std::env::var(&auth.value_env).map_err(|_err| {
                FilterError::from(format!("judge.auth.value_env '{}' is unset or empty", auth.value_env))
            })?;
            if value.is_empty() {
                return Err(FilterError::from(format!(
                    "judge.auth.value_env '{}' is unset or empty",
                    auth.value_env
                )));
            }
            Some(value)
        },
    };

    Ok(RouteConfig {
        judge: JudgeConfig {
            endpoint: raw.judge.endpoint,
            model: raw.judge.model,
            timeout_ms: raw.judge.timeout_ms,
            verify_tls: raw.judge.verify_tls,
            auth_token,
        },
        weak: TargetConfig {
            cluster: raw.targets.weak.cluster,
            model: raw.targets.weak.model,
        },
        strong: TargetConfig {
            cluster: raw.targets.strong.cluster,
            model: raw.targets.strong.model,
        },
        threshold: raw.threshold,
        on_failure: raw.on_failure,
        session_floor: raw.session_floor,
    })
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::assertions_on_result_states,
    reason = "tests"
)]
mod tests {
    use super::*;

    fn valid_yaml() -> serde_yaml::Value {
        serde_yaml::from_str(
            r#"
judge:
  endpoint: "http://localhost:8000/v1/chat/completions"
  model: "judge-model"
targets:
  weak:
    cluster: "weak-cluster"
    model: "weak-model"
  strong:
    cluster: "strong-cluster"
    model: "strong-model"
"#,
        )
        .unwrap()
    }

    #[test]
    fn parses_minimal_config() {
        let config = parse(&valid_yaml()).expect("should parse");
        assert_eq!(config.judge.model, "judge-model");
        assert_eq!(config.weak.cluster, "weak-cluster");
        assert!((config.threshold - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn applies_judge_defaults() {
        let config = parse(&valid_yaml()).expect("should parse");
        assert_eq!(config.judge.timeout_ms, 5000, "the judge timeout defaults to 5s");
        assert!(config.judge.verify_tls, "TLS verification is on unless disabled");
        assert!(config.judge.auth_token.is_none(), "no auth block means no token");
        assert!(
            matches!(config.on_failure, FailureMode::Open),
            "routing failures default to passing traffic through"
        );
        assert!(
            matches!(config.session_floor, SessionFloor::Enabled),
            "session floor defaults to enabled"
        );
    }

    #[test]
    fn honours_explicit_judge_settings() {
        let mut yaml = valid_yaml();
        yaml["judge"]["timeout_ms"] = serde_yaml::Value::Number(serde_yaml::Number::from(250));
        yaml["judge"]["verify_tls"] = serde_yaml::Value::Bool(false);
        yaml["on_failure"] = serde_yaml::Value::String("closed".to_owned());
        yaml["session_floor"] = serde_yaml::Value::String("disabled".to_owned());
        let config = parse(&yaml).expect("should parse");
        assert_eq!(
            config.judge.timeout_ms, 250,
            "an explicit timeout overrides the default"
        );
        assert!(!config.judge.verify_tls, "verify_tls: false must be honoured");
        assert!(
            matches!(config.on_failure, FailureMode::Closed),
            "on_failure: closed must be honoured"
        );
        assert!(
            matches!(config.session_floor, SessionFloor::Disabled),
            "session_floor: disabled must be honoured"
        );
    }

    #[test]
    fn rejects_invalid_threshold() {
        let mut yaml = valid_yaml();
        yaml["threshold"] = serde_yaml::Value::Number(serde_yaml::Number::from(1.5));
        assert!(parse(&yaml).is_err());
    }

    #[test]
    fn rejects_negative_threshold() {
        let mut yaml = valid_yaml();
        yaml["threshold"] = serde_yaml::Value::Number(serde_yaml::Number::from(-0.1));
        let err = parse(&yaml).expect_err("a negative probability is meaningless");
        assert!(
            err.to_string().contains("threshold must be between 0.0 and 1.0"),
            "the error must state the valid range, got {err}"
        );
    }

    #[test]
    fn accepts_threshold_bounds() {
        for bound in [0.0_f64, 1.0_f64] {
            let mut yaml = valid_yaml();
            yaml["threshold"] = serde_yaml::Value::Number(serde_yaml::Number::from(bound));
            let config = parse(&yaml).expect("the range is inclusive at both ends");
            assert!(
                (config.threshold - bound).abs() < f64::EPSILON,
                "the configured threshold must survive validation"
            );
        }
    }

    #[test]
    fn rejects_a_non_url_endpoint() {
        let mut yaml = valid_yaml();
        yaml["judge"]["endpoint"] = serde_yaml::Value::String("http://ju dge/v1".to_owned());
        let err = parse(&yaml).expect_err("the endpoint must be a URL");
        assert!(
            err.to_string().contains("judge.endpoint is not a valid URL"),
            "the error must name the endpoint field, got {err}"
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        let mut yaml = valid_yaml();
        yaml["typo"] = serde_yaml::Value::Bool(true);
        assert!(
            parse(&yaml).is_err(),
            "unknown keys are typos, not options, and must be rejected"
        );
    }

    #[test]
    fn rejects_missing_targets() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
judge:
  endpoint: "http://localhost:8000/v1/chat/completions"
  model: "judge-model"
"#,
        )
        .unwrap();
        assert!(parse(&yaml).is_err(), "both tiers are required");
    }

    #[test]
    fn rejects_an_empty_auth_env_name() {
        let mut yaml = valid_yaml();
        yaml["judge"]["auth"] = serde_yaml::from_str(r#"{"value_env": ""}"#).unwrap();
        let err = parse(&yaml).expect_err("an empty variable name cannot be read");
        assert!(
            err.to_string().contains("judge.auth.value_env must not be empty"),
            "the error must name the auth field, got {err}"
        );
    }

    #[test]
    fn rejects_an_unset_auth_env_var() {
        let mut yaml = valid_yaml();
        yaml["judge"]["auth"] = serde_yaml::from_str(r#"{"value_env": "PRAXIS_TEST_JUDGE_TOKEN_UNSET"}"#).unwrap();
        let err = parse(&yaml).expect_err("an unset variable is a config error");
        assert!(
            err.to_string()
                .contains("judge.auth.value_env 'PRAXIS_TEST_JUDGE_TOKEN_UNSET' is unset or empty"),
            "the error must name the variable, got {err}"
        );
    }

    #[test]
    fn reads_the_auth_token_from_the_environment() {
        // `unsafe_code` is forbidden workspace-wide, so the environment cannot be
        // mutated here. Borrow a variable the test process already carries instead.
        let (name, value) = std::env::vars()
            .find(|(name, value)| !value.is_empty() && name.chars().all(|ch| ch.is_ascii_uppercase() || ch == '_'))
            .expect("the test process always has at least one plain, non-empty variable");
        let mut yaml = valid_yaml();
        yaml["judge"]["auth"] = serde_yaml::from_str(&format!(r#"{{"value_env": "{name}"}}"#)).unwrap();
        let config = parse(&yaml).expect("a set variable yields a token");
        assert_eq!(
            config.judge.auth_token.as_deref(),
            Some(value.as_str()),
            "the bearer token comes from the environment, never the config file"
        );
    }

    #[test]
    fn debug_redacts_the_auth_token() {
        let judge = JudgeConfig {
            endpoint: "http://localhost:8000/v1/chat/completions".to_owned(),
            model: "judge-model".to_owned(),
            timeout_ms: 5000,
            verify_tls: true,
            auth_token: Some("sk-secret".to_owned()),
        };
        let rendered = format!("{judge:?}");
        assert!(
            !rendered.contains("sk-secret"),
            "the token must never reach a log line, got {rendered}"
        );
        assert!(
            rendered.contains("[redacted]"),
            "the redaction marker keeps the field visible, got {rendered}"
        );
    }

    #[test]
    fn debug_shows_absent_tokens_as_none() {
        let judge = JudgeConfig {
            endpoint: "http://localhost:8000/v1/chat/completions".to_owned(),
            model: "judge-model".to_owned(),
            timeout_ms: 5000,
            verify_tls: false,
            auth_token: None,
        };
        let rendered = format!("{judge:?}");
        assert!(
            rendered.contains("auth_token: None"),
            "an absent token renders as None, got {rendered}"
        );
    }

    #[test]
    fn target_selects_the_configured_tier() {
        let config = parse(&valid_yaml()).expect("should parse");
        assert_eq!(
            config.target(Tier::Weak).cluster,
            "weak-cluster",
            "the weak tier maps to the weak target"
        );
        assert_eq!(
            config.target(Tier::Strong).model,
            "strong-model",
            "the strong tier maps to the strong target"
        );
    }

    #[test]
    fn tier_ordering() {
        assert!(Tier::Weak < Tier::Strong);
    }

    #[test]
    fn tier_is_max() {
        assert!(!Tier::Weak.is_max(), "Weak is not the maximum tier");
        assert!(Tier::Strong.is_max(), "Strong is the maximum tier");
    }

    #[test]
    fn tier_tags_round_trip() {
        for tier in [Tier::Weak, Tier::Strong] {
            assert_eq!(
                Tier::from_tag(tier.tag()),
                Some(tier),
                "every tier's tag must parse back to itself"
            );
        }
        assert_eq!(
            Tier::Weak.tag(),
            "weak",
            "the weak tag is part of the Switchyard contract"
        );
        assert_eq!(
            Tier::Strong.tag(),
            "strong",
            "the strong tag is part of the Switchyard contract"
        );
        assert_eq!(
            Tier::from_tag("medium"),
            None,
            "an unrecognised decision tag must not resolve to a tier"
        );
    }
}

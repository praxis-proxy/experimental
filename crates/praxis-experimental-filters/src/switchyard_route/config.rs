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
    fn rejects_invalid_threshold() {
        let mut yaml = valid_yaml();
        yaml["threshold"] = serde_yaml::Value::Number(serde_yaml::Number::from(1.5));
        assert!(parse(&yaml).is_err());
    }

    #[test]
    fn tier_ordering() {
        assert!(Tier::Weak < Tier::Strong);
    }
}

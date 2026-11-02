//! Selects the separately configured Switchyard failure target.

use async_trait::async_trait;
use bytes::Bytes;
use praxis_filter::{
    BodyAccess, BodyMode, FilterAction, FilterError, HttpFilter, HttpFilterContext, body::MAX_JSON_BODY_BYTES,
};
use serde::Deserialize;

/// The metadata key consumed by the load balancer after the body phase.
const METADATA_CLUSTER: &str = "switchyard_route.cluster";

/// An experimental filter that applies one configured failure target.
pub(crate) struct SwitchyardFailureTargetFilter {
    /// Cluster selected by the fallback step.
    cluster: String,
    /// Model written into the replayed request body.
    model: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// YAML configuration for the fallback target.
struct RawConfig {
    /// Cluster selected by the fallback step.
    cluster: String,
    /// Model written into the replayed request body.
    model: String,
}

impl SwitchyardFailureTargetFilter {
    /// Creates the filter from its validated YAML target.
    pub(crate) fn from_config(yaml: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let config: RawConfig =
            serde_yaml::from_value(yaml.clone()).map_err(|err| FilterError::from(err.to_string()))?;
        validate_non_empty("cluster", &config.cluster)?;
        validate_non_empty("model", &config.model)?;
        Ok(Box::new(Self {
            cluster: config.cluster,
            model: config.model,
        }))
    }
}

#[async_trait]
impl HttpFilter for SwitchyardFailureTargetFilter {
    fn name(&self) -> &'static str {
        "switchyard_failure_target"
    }

    fn request_body_access(&self) -> BodyAccess {
        BodyAccess::ReadWrite
    }

    fn request_body_mode(&self) -> BodyMode {
        BodyMode::StreamBuffer {
            // The primary route may admit any valid JSON body up to this ceiling.
            // Keep replay safe by accepting every body it can pass to IRR.
            max_bytes: Some(MAX_JSON_BODY_BYTES),
        }
    }

    fn selects_cluster(&self) -> bool {
        true
    }

    fn selected_clusters(&self) -> Vec<String> {
        vec![self.cluster.clone()]
    }

    async fn on_request_body(
        &self,
        _ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        if !end_of_stream {
            return Ok(FilterAction::Continue);
        }
        let Some(raw) = body.as_ref() else {
            return Err(FilterError::from("switchyard_failure_target: request body missing"));
        };
        *body = Some(rewrite_body(raw, &self.model)?);
        Ok(FilterAction::Continue)
    }

    async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        ctx.set_metadata(METADATA_CLUSTER, self.cluster.clone());
        ctx.cluster = Some(self.cluster.clone().into());
        Ok(FilterAction::Continue)
    }
}

/// Reject empty target values early instead of producing an unresolvable route.
fn validate_non_empty(field: &str, value: &str) -> Result<(), FilterError> {
    if value.trim().is_empty() {
        return Err(FilterError::from(format!("{field} must not be empty")));
    }
    Ok(())
}

/// Rewrites only the model field while preserving the rest of the request.
fn rewrite_body(raw: &[u8], model: &str) -> Result<Bytes, FilterError> {
    let mut value: serde_json::Value = serde_json::from_slice(raw)
        .map_err(|err| FilterError::from(format!("switchyard_failure_target: invalid JSON: {err}")))?;
    value
        .as_object_mut()
        .ok_or_else(|| FilterError::from("switchyard_failure_target: request body is not an object"))?
        .insert("model".to_owned(), serde_json::Value::String(model.to_owned()));
    serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(|err| FilterError::from(format!("switchyard_failure_target: serialize failed: {err}")))
}

#[cfg(test)]
#[expect(
    clippy::allow_attributes,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "small configuration tests"
)]
#[allow(clippy::panic, reason = "assertion failures should identify invalid fixtures")]
mod tests {
    use praxis_filter::{BodyMode, body::MAX_JSON_BODY_BYTES};

    use super::SwitchyardFailureTargetFilter;

    #[test]
    fn accepts_a_configured_target() {
        let yaml = serde_yaml::from_str("cluster: failure-cluster\nmodel: failure-model\n").unwrap();
        let filter = SwitchyardFailureTargetFilter::from_config(&yaml).expect("target should parse");
        assert_eq!(filter.name(), "switchyard_failure_target");
    }

    #[test]
    fn buffers_every_json_body_the_primary_route_can_admit() {
        let yaml = serde_yaml::from_str("cluster: failure-cluster\nmodel: failure-model\n").unwrap();
        let filter = SwitchyardFailureTargetFilter::from_config(&yaml).expect("target should parse");
        assert_eq!(
            filter.request_body_mode(),
            BodyMode::StreamBuffer {
                max_bytes: Some(MAX_JSON_BODY_BYTES)
            }
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        let yaml = serde_yaml::from_str("cluster: failure-cluster\nmodel: failure-model\nextra: true\n").unwrap();
        assert!(SwitchyardFailureTargetFilter::from_config(&yaml).is_err());
    }

    #[test]
    fn rejects_empty_target_values() {
        let yaml = serde_yaml::from_str("cluster: '  '\nmodel: failure-model\n").unwrap();
        assert!(SwitchyardFailureTargetFilter::from_config(&yaml).is_err());
    }

    #[test]
    fn rewrites_only_the_model() {
        let body = super::rewrite_body(
            br#"{"model":"primary-model","messages":[{"role":"user","content":"hello"}]}"#,
            "failure-model",
        )
        .expect("valid request body should rewrite");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("rewritten body should remain JSON");
        assert_eq!(value.get("model"), Some(&serde_json::json!("failure-model")));
        let content = value
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .and_then(|messages| messages.first())
            .and_then(|message| message.get("content"));
        assert_eq!(content, Some(&serde_json::json!("hello")));
    }

    #[test]
    fn rewrites_a_body_larger_than_the_former_one_mib_limit() {
        let content = "x".repeat(1_048_576);
        let expected_content = content.clone();
        let raw = serde_json::to_vec(&serde_json::json!({
            "model": "primary-model",
            "messages": [{"role": "user", "content": content}],
        }))
        .expect("large body should serialize");
        let rewritten = super::rewrite_body(&raw, "failure-model").expect("large body should rewrite");
        let value: serde_json::Value = serde_json::from_slice(&rewritten).expect("rewritten body should remain JSON");

        assert!(rewritten.len() > 1_048_576);
        assert_eq!(value.get("model"), Some(&serde_json::json!("failure-model")));
        let rewritten_content = value
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .and_then(|messages| messages.first())
            .and_then(|message| message.get("content"));
        assert_eq!(rewritten_content, Some(&serde_json::json!(expected_content)));
    }
}

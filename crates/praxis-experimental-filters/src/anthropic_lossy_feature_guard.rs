//! Rejects Anthropic Messages features that the stock Chat translator drops.

use async_trait::async_trait;
use bytes::Bytes;
use praxis_filter::{BodyAccess, BodyMode, FilterAction, FilterError, HttpFilter, HttpFilterContext, Rejection};
use tracing::warn;

/// Top-level Anthropic fields with no Chat Completions equivalent.
const UNSUPPORTED_TOP_LEVEL_FIELDS: [&str; 2] = ["thinking", "context_management"];

/// Stops a request before the Anthropic translator could silently discard data.
#[derive(Debug)]
pub(crate) struct AnthropicLossyFeatureGuard;

impl AnthropicLossyFeatureGuard {
    /// Builds the guard, which has no configuration options.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the export_filters! factory signature requires Result"
    )]
    pub(crate) fn from_config(_config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        Ok(Box::new(Self))
    }
}

#[async_trait]
impl HttpFilter for AnthropicLossyFeatureGuard {
    fn name(&self) -> &'static str {
        "anthropic_lossy_feature_guard"
    }

    fn request_body_access(&self) -> BodyAccess {
        BodyAccess::ReadOnly
    }

    fn request_body_mode(&self) -> BodyMode {
        BodyMode::StreamBuffer { max_bytes: None }
    }

    async fn on_request(&self, _ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        Ok(FilterAction::Continue)
    }

    async fn on_request_body(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        if !end_of_stream || !is_anthropic_messages_path(ctx.request.uri.path()) {
            return Ok(FilterAction::Continue);
        }
        let feature = body
            .as_deref()
            .and_then(|raw| serde_json::from_slice(raw).ok())
            .and_then(|request| unsupported_feature(&request));
        Ok(feature.map_or(FilterAction::Continue, reject_unsupported_feature))
    }
}

/// Returns whether a path is the public Anthropic Messages endpoint.
fn is_anthropic_messages_path(path: &str) -> bool {
    path.strip_suffix('/').unwrap_or(path) == "/v1/messages"
}

/// Returns the first field or block that the stock translator would drop.
fn unsupported_feature(request: &serde_json::Value) -> Option<&'static str> {
    let object = request.as_object()?;
    UNSUPPORTED_TOP_LEVEL_FIELDS
        .iter()
        .copied()
        .find(|field| object.contains_key(*field))
        .or_else(|| contains_thinking_block(object).then_some("thinking content block"))
        .or_else(|| contains_typed_tool(object).then_some("typed tool"))
}

/// Returns whether any message content block is thinking or redacted thinking.
fn contains_thinking_block(request: &serde_json::Map<String, serde_json::Value>) -> bool {
    request
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                message
                    .get("content")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|blocks| {
                        blocks.iter().any(|block| {
                            matches!(
                                block.get("type").and_then(serde_json::Value::as_str),
                                Some("thinking" | "redacted_thinking")
                            )
                        })
                    })
            })
        })
}

/// Returns whether any tool is not an untyped or `custom` client tool.
fn contains_typed_tool(request: &serde_json::Map<String, serde_json::Value>) -> bool {
    request
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool.get("type").is_some_and(|kind| kind.as_str() != Some("custom")))
        })
}

/// Returns an Anthropic-compatible invalid-request error for a lost feature.
fn reject_unsupported_feature(feature: &'static str) -> FilterAction {
    warn!(
        feature,
        "rejecting Anthropic Messages feature that cannot be translated"
    );
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": format!("unsupported Anthropic Messages feature: {feature}"),
        }
    });
    FilterAction::Reject(
        Rejection::status(400)
            .with_header("content-type", "application/json")
            .with_body(body.to_string())
            .preserving_keepalive(),
    )
}

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "tests use unwrap and expect for clear fixtures")]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "test fixtures are static"
)]
mod tests {
    use praxis_filter::FilterRegistry;

    use super::*;

    /// Parses a static Anthropic request fixture.
    fn request(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("fixture is valid JSON")
    }

    #[test]
    fn accepts_the_translatable_subset() {
        let body = request(
            r#"{"model":"claude","messages":[{"role":"user","content":"hello"}],"tools":[{"name":"lookup","input_schema":{"type":"object"}},{"type":"custom","name":"clock","input_schema":{"type":"object"}}]}"#,
        );
        assert_eq!(
            unsupported_feature(&body),
            None,
            "client tools and text are translatable"
        );
    }

    #[test]
    fn identifies_every_known_lossy_feature() {
        for (body, expected) in [
            (r#"{"thinking":{"type":"enabled"}}"#, "thinking"),
            (r#"{"context_management":{}}"#, "context_management"),
            (
                r#"{"messages":[{"content":[{"type":"thinking","thinking":"secret"}]}]}"#,
                "thinking content block",
            ),
            (r#"{"tools":[{"type":"web_search_20250305"}]}"#, "typed tool"),
        ] {
            assert_eq!(
                unsupported_feature(&request(body)),
                Some(expected),
                "must reject {expected}"
            );
        }
    }

    #[test]
    fn recognizes_only_the_public_anthropic_path() {
        assert!(is_anthropic_messages_path("/v1/messages"));
        assert!(is_anthropic_messages_path("/v1/messages/"));
        assert!(!is_anthropic_messages_path("/openai/v1/messages"));
    }

    #[test]
    fn filter_is_registered() {
        let mut registry = FilterRegistry::with_builtins();
        crate::register_filters(&mut registry);
        assert!(registry.available_filters().contains(&"anthropic_lossy_feature_guard"));
    }
}

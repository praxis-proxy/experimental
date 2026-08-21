//! Wire-format detection, judge-side decoding, and the model rewrite.
//!
//! Translation is used **only to feed the judge**: the inbound body is decoded
//! into the Switchyard IR so the classifier sees a normalized request
//! regardless of source format. The forwarded request is never round-tripped
//! through the IR — both supported formats carry `model` at the JSON top
//! level, so the rewrite mutates the original parsed body in place (mirroring
//! the stock `ModelRewriteFilter`), preserving every provider-specific field.

use serde_json::Value;
use switchyard_protocol::{LlmRequest, WireFormat};

use super::error::RouteError;

/// Detects the request's wire format, primarily from the request path.
///
/// Path suffixes are authoritative (`/chat/completions` ⇒ OpenAI chat,
/// `/messages` ⇒ Anthropic messages); otherwise the body shape decides.
///
/// # Errors
///
/// Returns [`RouteError::UnknownFormat`] (or a body-shape error) when neither
/// signal identifies a supported format; the caller takes the failure path.
pub(crate) fn detect_format(path: &str, body: &Value) -> Result<WireFormat, RouteError> {
    if path.ends_with("/chat/completions") {
        return Ok(WireFormat::OpenAiChat);
    }
    if path.ends_with("/messages") {
        return Ok(WireFormat::AnthropicMessages);
    }
    detect_format_from_shape(body)
}

/// Body-shape fallback for non-canonical paths.
///
/// Anthropic messages bodies always carry a top-level `max_tokens` (and often
/// `system`); OpenAI chat bodies usually carry neither. An OpenAI chat request
/// using the legacy `max_tokens` off the canonical path would be misread —
/// the path check above is the primary, precise signal.
fn detect_format_from_shape(body: &Value) -> Result<WireFormat, RouteError> {
    let Some(object) = body.as_object() else {
        return Err(RouteError::Body("is not a JSON object"));
    };
    if !object.contains_key("messages") {
        return Err(RouteError::UnknownFormat);
    }
    if object.contains_key("system") || object.contains_key("max_tokens") {
        return Ok(WireFormat::AnthropicMessages);
    }
    Ok(WireFormat::OpenAiChat)
}

/// Decodes the inbound body into the Switchyard IR for the judge.
///
/// # Errors
///
/// Returns [`RouteError::Translation`] when the codec rejects the body.
pub(crate) fn decode_for_judge(format: WireFormat, body: &Value) -> Result<LlmRequest, RouteError> {
    switchyard_translation::decode_request(format, body).map_err(|err| RouteError::Translation(err.to_string()))
}

/// Rewrites the top-level `model` field in place and re-serializes.
///
/// # Errors
///
/// Returns [`RouteError::Body`] when the body is not a JSON object, or
/// [`RouteError::Serialize`] when re-serialization fails.
pub(crate) fn rewrite_model(body: &mut Value, model: &str) -> Result<Vec<u8>, RouteError> {
    let Some(object) = body.as_object_mut() else {
        return Err(RouteError::Body("is not a JSON object"));
    };
    object.insert("model".to_owned(), Value::String(model.to_owned()));
    serde_json::to_vec(body).map_err(|err| RouteError::Serialize(err.to_string()))
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
    use serde_json::{Value, json};
    use switchyard_protocol::WireFormat;

    use super::{decode_for_judge, detect_format, rewrite_model};

    /// A minimal OpenAI chat-completions body.
    fn openai_body() -> Value {
        json!({
            "model": "agent-default",
            "messages": [{"role": "user", "content": "hello"}],
        })
    }

    /// A minimal Anthropic messages body.
    fn anthropic_body() -> Value {
        json!({
            "model": "agent-default",
            "max_tokens": 128,
            "system": "be terse",
            "messages": [{"role": "user", "content": "hello"}],
        })
    }

    #[test]
    fn path_detection_is_authoritative() {
        let chat_format = detect_format("/v1/chat/completions", &anthropic_body()).unwrap();
        assert_eq!(chat_format, WireFormat::OpenAiChat, "path beats body shape");
        let messages_format = detect_format("/v1/messages", &openai_body()).unwrap();
        assert_eq!(
            messages_format,
            WireFormat::AnthropicMessages,
            "messages path maps to Anthropic"
        );
    }

    #[test]
    fn shape_fallback_detects_both_formats() {
        let openai_format = detect_format("/proxy", &openai_body()).unwrap();
        assert_eq!(
            openai_format,
            WireFormat::OpenAiChat,
            "plain messages body is OpenAI chat"
        );
        let anthropic_format = detect_format("/proxy", &anthropic_body()).unwrap();
        assert_eq!(
            anthropic_format,
            WireFormat::AnthropicMessages,
            "system+max_tokens body is Anthropic"
        );
    }

    #[test]
    fn undetectable_bodies_are_errors() {
        let formatless_error = detect_format("/proxy", &json!({"no": "messages"})).unwrap_err();
        assert!(
            formatless_error.to_string().contains("unrecognized"),
            "message-less body has no format: {formatless_error}"
        );
        let shapeless_error = detect_format("/proxy", &json!(["not", "an", "object"])).unwrap_err();
        assert!(
            shapeless_error.to_string().contains("object"),
            "non-object body is an error: {shapeless_error}"
        );
    }

    #[test]
    fn openai_bodies_decode_for_the_judge() {
        let request = decode_for_judge(WireFormat::OpenAiChat, &openai_body()).unwrap();
        assert_eq!(request.model.as_deref(), Some("agent-default"), "model survives decode");
        assert_eq!(request.messages.len(), 1, "message survives decode");
    }

    #[test]
    fn anthropic_bodies_decode_for_the_judge() {
        let request = decode_for_judge(WireFormat::AnthropicMessages, &anthropic_body()).unwrap();
        assert_eq!(request.model.as_deref(), Some("agent-default"), "model survives decode");
        assert_eq!(request.messages.len(), 1, "message survives decode");
    }

    #[test]
    fn decode_is_lenient_about_malformed_fields() {
        // Switchyard's default TranslationPolicy is permissive: a malformed
        // field is preserved/diagnosed rather than fatal, so a shapeless body
        // decodes to an empty-message request (the judge still runs; failure
        // ownership stays with the host).
        let request = decode_for_judge(WireFormat::OpenAiChat, &json!({"messages": "nope"})).unwrap();
        assert!(request.messages.is_empty(), "junk messages decode to none");
    }

    #[test]
    fn non_object_bodies_fail_translation() {
        let error = decode_for_judge(WireFormat::OpenAiChat, &json!(["not", "an", "object"])).unwrap_err();
        assert!(
            error.to_string().contains("translation"),
            "codec rejection surfaces as a translation error: {error}"
        );
    }

    #[test]
    fn rewrite_replaces_only_the_model() {
        let mut body = anthropic_body();
        let bytes = rewrite_model(&mut body, "claude-strong").unwrap();
        let round_trip: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(round_trip["model"], "claude-strong", "model is rewritten");
        assert_eq!(round_trip["max_tokens"], 128, "other fields are preserved");
        assert_eq!(round_trip["system"], "be terse", "provider-specific fields survive");
    }

    #[test]
    fn rewrite_rejects_non_objects() {
        let mut body = json!("just a string");
        let error = rewrite_model(&mut body, "claude-strong").unwrap_err();
        assert!(
            error.to_string().contains("object"),
            "non-object rewrite errors: {error}"
        );
    }
}

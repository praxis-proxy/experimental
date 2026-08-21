//! Serving the judge (classifier) callout.
//!
//! Switchyard fully prepares the judge request — packaged classifier system
//! prompt, trimmed messages, JSON-schema `response_format`, and output token
//! cap. This module only transports it: encode onto the OpenAI chat wire with
//! the configured judge model, POST it through the [`JudgeTransport`] seam,
//! decode the reply, and deliver it back into the run.

use async_trait::async_trait;
use bytes::Bytes;
use switchyard_libsy::CallLlmRequest;
use switchyard_protocol::{LlmResponse, Response, WireFormat};

use super::error::RouteError;

/// Transport seam for the judge HTTP exchange, so the step loop is testable
/// without a network.
#[async_trait]
pub(crate) trait JudgeTransport: Send + Sync {
    /// POSTs an encoded judge request body and returns the raw response body.
    async fn execute(&self, body: Bytes) -> Result<Bytes, RouteError>;
}

/// Serves one judge `CallLlm` step through `transport`.
///
/// # Errors
///
/// Returns [`RouteError::Judge`] on any encode, transport, decode, or respond
/// failure. The caller treats that as the filter's single failure path rather
/// than responding `Err` into the run: Switchyard would fold a judge error
/// into an ambiguous verdict and default a tier itself, and the host — not
/// the library — owns the failure outcome (pass through unmodified).
pub(crate) async fn serve_judge(
    call: Box<CallLlmRequest>,
    judge_model: &str,
    transport: &dyn JudgeTransport,
) -> Result<(), RouteError> {
    let request_body = encode_judge_request(&call, judge_model)?;
    let response_body = transport.execute(request_body).await?;
    let response = decode_judge_response(&response_body)?;
    call.respond(Ok(response))
        .map_err(|err| RouteError::Judge(format!("failed to deliver judge response: {err}")))
}

/// Encodes the prepared judge request onto the OpenAI chat wire, overriding
/// the model (the prepared request inherits the *inbound* model name) and
/// forcing a non-streaming exchange.
fn encode_judge_request(call: &CallLlmRequest, judge_model: &str) -> Result<Bytes, RouteError> {
    let mut llm_request = call.get_request().llm_request.clone();
    llm_request.model = Some(judge_model.to_owned());
    llm_request.stream = false;
    let wire = switchyard_translation::encode_request(&llm_request, WireFormat::OpenAiChat)
        .map_err(|err| RouteError::Judge(format!("request encoding failed: {err}")))?;
    let bytes =
        serde_json::to_vec(&wire).map_err(|err| RouteError::Judge(format!("request serialization failed: {err}")))?;
    Ok(Bytes::from(bytes))
}

/// Decodes an OpenAI chat completion reply into a Switchyard [`Response`].
fn decode_judge_response(body: &Bytes) -> Result<Response, RouteError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|err| RouteError::Judge(format!("response is not JSON: {err}")))?;
    let aggregated = switchyard_translation::decode_aggregated_response(&value, WireFormat::OpenAiChat)
        .map_err(|err| RouteError::Judge(format!("response translation failed: {err}")))?;
    Ok(Response {
        llm_response: LlmResponse::Agg(aggregated),
        metadata: None,
    })
}

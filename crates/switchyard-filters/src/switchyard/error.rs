//! Failure vocabulary for the `switchyard_route` filter.
//!
//! Every variant funnels into the filter's single failure path: under
//! `failure_mode: open` the request passes through **unmodified** (the
//! no-downgrade safety net — see the module docs on `switchyard.rs`); under
//! `failure_mode: closed` it is rejected with 503.

use thiserror::Error;

/// Everything that can prevent a routing decision.
#[derive(Debug, Error)]
pub(crate) enum RouteError {
    /// The buffered request body was missing, empty, or the wrong shape.
    #[error("request body {0}")]
    Body(&'static str),
    /// The request body was not valid JSON.
    #[error("request body is not valid JSON: {0}")]
    Json(String),
    /// Neither the path nor the body shape identified a supported wire format.
    #[error("unrecognized wire format")]
    UnknownFormat,
    /// The body could not be decoded into the Switchyard IR for the judge.
    #[error("request translation failed: {0}")]
    Translation(String),
    /// The rewritten body could not be re-serialized.
    #[error("body re-serialization failed: {0}")]
    Serialize(String),
    /// The server did not provide a `SubRequestClient` for the judge callout.
    #[error("subrequest client unavailable")]
    MissingSubrequestClient,
    /// The judge callout failed (encode, transport, decode, or respond).
    #[error("judge callout failed: {0}")]
    Judge(String),
    /// The Switchyard run surfaced an error step.
    #[error("switchyard run failed: {0}")]
    Run(String),
    /// The step stream ended without a routed decision.
    #[error("switchyard produced no routing decision")]
    NoDecision,
    /// The routed decision named a tag outside the configured tier table.
    #[error("switchyard selected unknown tier '{0}'")]
    UnknownTier(String),
}

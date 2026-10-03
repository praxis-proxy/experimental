//! Manual JWT authentication and private dashboard credential validation.
mod config;
#[cfg(test)]
mod tests;
mod verifier;

use async_trait::async_trait;
use bytes::Bytes;
use config::{Config, Mode};
use http::{HeaderName, HeaderValue, Method};
use praxis_filter::{BodyAccess, BodyMode, FilterAction, FilterError, HttpFilter, HttpFilterContext, Rejection};
use serde::Deserialize;
use verifier::{AuthError, Verifier};

/// Credential input accepted by the private `MaaS`-compatible endpoint.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Callback {
    /// Caller JWT submitted by the metering login handler.
    key: String,
}

/// Non-expiring caller JWTs with explicit registry revocation.
pub(crate) struct ManualJwtFilter {
    /// Listener-specific mode and trust boundaries.
    config: Config,
    /// Common authentication logic for login and inference.
    verifier: Verifier,
}

impl ManualJwtFilter {
    /// Build a manually administered authentication boundary.
    ///
    /// # Errors
    /// Fails on malformed configuration, empty trust values or invalid public key.
    pub(crate) fn from_config(value: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let config: Config = praxis_filter::parse_filter_config("manual_jwt", value)?;
        if config.mode == Mode::Dashboard
            && (config.allowed_origins.is_empty()
                || config
                    .allowed_origins
                    .iter()
                    .any(|origin| !origin.starts_with("https://") || origin.ends_with('/')))
        {
            return Err("manual_jwt: dashboard requires explicit HTTPS origins without trailing slash".into());
        }
        let verifier = Verifier::new(&config)?;
        Ok(Box::new(Self { config, verifier }))
    }

    /// Assert only verified identity and consume all caller authentication material.
    async fn inference(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        let mut values = ctx.request.headers.get_all("authorization").iter();
        let token = values
            .next()
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, _token)| scheme.eq_ignore_ascii_case("Bearer"))
            .map(|(_scheme, token)| token);
        let Some(token) = token.filter(|_token| values.next().is_none()) else {
            return Ok(denied(401));
        };
        match self.verifier.authenticate(token).await {
            Ok(subject) => {
                strip_identity(ctx);
                ctx.request_headers_to_remove.push(HeaderName::from_static("cookie"));
                ctx.request_headers_to_set.push((
                    HeaderName::from_static("x-tenant-username"),
                    HeaderValue::from_str(&subject)?,
                ));
                Ok(FilterAction::Continue)
            },
            Err(error) => Ok(auth_failure(&error)),
        }
    }

    /// Return the existing `MaaS` login-validation contract on a private listener.
    async fn callback(&self, ctx: &HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        if ctx.request.uri.path() != "/validate" {
            return Ok(denied(404));
        }
        if ctx.request.method != Method::POST {
            return Ok(denied(405));
        }
        let Some(body) = ctx.buffered_request_body.as_ref() else {
            return Ok(denied(400));
        };
        if body.len() > 16_384 {
            return Ok(denied(413));
        }
        let Ok(input) = serde_json::from_slice::<Callback>(body) else {
            return Ok(denied(400));
        };
        match self.verifier.authenticate(&input.key).await {
            Ok(subject) => {
                let payload = serde_json::to_vec(&serde_json::json!({"valid":true,"username":subject,"groups":[]}))?;
                Ok(FilterAction::Reject(
                    Rejection::status(200)
                        .with_header("Content-Type", "application/json")
                        .with_header("Cache-Control", "no-store")
                        .with_body(Bytes::from(payload)),
                ))
            },
            Err(error) => Ok(auth_failure(&error)),
        }
    }

    /// Retain `PriceTag`'s cookie authentication and reject cross-origin writes.
    fn dashboard(&self, ctx: &mut HttpFilterContext<'_>) -> FilterAction {
        strip_identity(ctx);
        if !matches!(ctx.request.method, Method::GET | Method::HEAD | Method::OPTIONS) {
            let origin = ctx.request.headers.get("origin").and_then(|value| value.to_str().ok());
            if !origin.is_some_and(|value| self.config.allowed_origins.iter().any(|allowed| value == allowed)) {
                return denied(403);
            }
        }
        FilterAction::Continue
    }
}

/// Strip every identity source the metering service could consume.
fn strip_identity(ctx: &mut HttpFilterContext<'_>) {
    for name in [
        "authorization",
        "x-api-key",
        "x-tenant-username",
        "x-tenant-group",
        "x-tenant-subscription",
        "x-forwarded-user",
        "x-forwarded-groups",
        "x-forwarded-real-user",
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
        "x-real-ip",
        "proxy-authorization",
    ] {
        ctx.request_headers_to_remove.push(HeaderName::from_static(name));
    }
}

/// Map registry unavailability separately from a rejected caller credential.
fn auth_failure(error: &AuthError) -> FilterAction {
    match error {
        AuthError::Invalid => denied(401),
        AuthError::Unavailable => denied(503),
    }
}

/// A generic non-cacheable failure that never echoes caller credentials.
fn denied(status: u16) -> FilterAction {
    FilterAction::Reject(Rejection::status(status).with_header("Cache-Control", "no-store"))
}

#[async_trait]
impl HttpFilter for ManualJwtFilter {
    fn name(&self) -> &'static str {
        "manual_jwt"
    }

    fn request_body_access(&self) -> BodyAccess {
        if self.config.mode == Mode::Callback {
            BodyAccess::ReadOnly
        } else {
            BodyAccess::None
        }
    }

    fn request_body_mode(&self) -> BodyMode {
        if self.config.mode == Mode::Callback {
            BodyMode::StreamBuffer {
                max_bytes: Some(16_384),
            }
        } else {
            BodyMode::Stream
        }
    }

    async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        match self.config.mode {
            Mode::Inference => self.inference(ctx).await,
            Mode::Callback => self.callback(ctx).await,
            Mode::Dashboard => Ok(self.dashboard(ctx)),
        }
    }
}

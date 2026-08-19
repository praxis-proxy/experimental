//! Judge endpoint resolution and the production [`JudgeTransport`].
//!
//! Filters cannot address a Praxis cluster from a sub-request — callouts
//! target explicit URLs. This is a minimal reimplementation of praxis-ai's
//! (crate-private) `execute_url`: parse the URL once at config time, resolve
//! every address at call time, try each until one connects, preserve the URL
//! authority as `Host`, and bound the whole exchange with one deadline.

use std::{net::SocketAddr, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use pingora_core::upstreams::peer::HttpPeer;
use praxis_core::subrequest::{SubRequest, SubRequestClient, SubRequestError, SubResponse};
use praxis_filter::FilterError;

use super::{
    config::{JudgeAuthConfig, JudgeConfig},
    error::RouteError,
    judge::JudgeTransport,
};

/// A judge endpoint parsed once at configuration time.
#[derive(Debug, Clone)]
pub(crate) struct JudgeEndpoint {
    /// Whether to establish a TLS connection.
    tls: bool,
    /// DNS hostname or literal address.
    host: String,
    /// Destination TCP port.
    port: u16,
    /// TLS server name; empty for cleartext HTTP.
    sni: String,
    /// Original URL authority, preserved as the `Host` header.
    authority: http::HeaderValue,
    /// Path and query sent to the judge.
    uri: http::Uri,
    /// Resolved `(header, value)` credential; `None` for a keyless judge.
    auth: Option<(http::HeaderName, http::HeaderValue)>,
}

impl JudgeEndpoint {
    /// Parses the judge URL and resolves its optional credential from the
    /// environment, so both are ready before the first request.
    ///
    /// # Errors
    ///
    /// Returns a [`FilterError`] when the URL is relative, uses a scheme other
    /// than http/https, carries a malformed authority or path, or when the
    /// configured credential environment variable is unset or malformed.
    pub(crate) fn from_config(judge: &JudgeConfig) -> Result<Self, FilterError> {
        let mut endpoint = Self::parse_url(&judge.endpoint)?;
        endpoint.auth = judge.auth.as_ref().map(resolve_auth).transpose()?;
        Ok(endpoint)
    }

    /// Parses an absolute http(s) URL into its connection components.
    fn parse_url(endpoint: &str) -> Result<Self, FilterError> {
        let parsed: http::Uri = endpoint
            .parse()
            .map_err(|err| endpoint_error(&format!("not a valid URL: {err}")))?;
        let tls = parse_tls(&parsed)?;
        let authority = parsed.authority().ok_or_else(|| endpoint_error("missing host"))?;
        let host = authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let port = authority.port_u16().unwrap_or(if tls { 443 } else { 80 });
        let sni = if tls { host.clone() } else { String::new() };
        let authority = http::HeaderValue::from_str(authority.as_str())
            .map_err(|err| endpoint_error(&format!("invalid authority: {err}")))?;
        let uri: http::Uri = parsed
            .path_and_query()
            .map_or("/", http::uri::PathAndQuery::as_str)
            .parse()
            .map_err(|err| endpoint_error(&format!("bad path: {err}")))?;
        Ok(Self {
            tls,
            host,
            port,
            sni,
            authority,
            uri,
            auth: None,
        })
    }
}

/// Resolves a credential from the environment into a ready `(header, value)`.
///
/// # Errors
///
/// Returns a [`FilterError`] when the environment variable is unset or empty,
/// the header name is invalid, or the assembled value is not a legal header
/// value (which would leak nothing, since the message never echoes the secret).
fn resolve_auth(auth: &JudgeAuthConfig) -> Result<(http::HeaderName, http::HeaderValue), FilterError> {
    let secret = std::env::var(&auth.value_env)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| auth_error(&format!("credential env var '{}' is unset or empty", auth.value_env)))?;
    assemble_auth(auth, &secret)
}

/// Assembles a `(header, value)` credential from an already-resolved secret.
///
/// Split from the environment lookup so the header/scheme assembly is testable
/// without mutating process-global state (workspace lints forbid `unsafe`, and
/// `std::env::set_var` is `unsafe`).
///
/// # Errors
///
/// Returns a [`FilterError`] when the header name is invalid or the assembled
/// value is not a legal header value; the message never echoes the secret.
fn assemble_auth(auth: &JudgeAuthConfig, secret: &str) -> Result<(http::HeaderName, http::HeaderValue), FilterError> {
    let header = http::HeaderName::from_bytes(auth.header.as_bytes())
        .map_err(|err| auth_error(&format!("invalid header name: {err}")))?;
    let rendered = if auth.scheme.is_empty() {
        secret.to_owned()
    } else {
        format!("{} {secret}", auth.scheme)
    };
    let mut value = http::HeaderValue::from_str(&rendered)
        .map_err(|_err| auth_error("resolved credential is not a valid header value"))?;
    value.set_sensitive(true);
    Ok((header, value))
}

/// Maps the URL scheme to a TLS decision.
fn parse_tls(parsed: &http::Uri) -> Result<bool, FilterError> {
    match parsed.scheme_str() {
        Some("https") => Ok(true),
        Some("http") => Ok(false),
        Some(other) => Err(endpoint_error(&format!("unsupported scheme '{other}'"))),
        None => Err(endpoint_error("must be an absolute http(s) URL")),
    }
}

/// Builds an endpoint-shaped [`FilterError`].
fn endpoint_error(message: &str) -> FilterError {
    format!("switchyard_route: judge.endpoint {message}").into()
}

/// Builds an auth-shaped [`FilterError`]; never echoes the credential value.
fn auth_error(message: &str) -> FilterError {
    format!("switchyard_route: judge.auth {message}").into()
}

/// The production [`JudgeTransport`]: resolves the parsed endpoint and
/// executes the callout through the server-shared per-request
/// [`SubRequestClient`].
pub(crate) struct SubRequestJudge<'req> {
    /// The per-request sub-request client borrowed from the filter context.
    client: &'req SubRequestClient,
    /// The parsed judge endpoint.
    endpoint: &'req JudgeEndpoint,
    /// Deadline covering DNS resolution and the complete HTTP exchange.
    timeout: Duration,
    /// Response size cap forwarded to the client.
    max_response_bytes: usize,
}

impl<'req> SubRequestJudge<'req> {
    /// Assembles the transport from per-request and config-time parts.
    pub(crate) fn new(client: &'req SubRequestClient, endpoint: &'req JudgeEndpoint, judge: &JudgeConfig) -> Self {
        Self {
            client,
            endpoint,
            timeout: judge.timeout(),
            max_response_bytes: judge.max_response_bytes,
        }
    }

    /// Builds the POST sub-request carrying the encoded judge body.
    fn build_request(&self, body: Bytes) -> SubRequest {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::HOST, self.endpoint.authority.clone());
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        if let Some((name, value)) = self.endpoint.auth.as_ref() {
            headers.insert(name.clone(), value.clone());
        }
        SubRequest {
            method: http::Method::POST,
            uri: self.endpoint.uri.clone(),
            headers,
            body,
        }
    }

    /// Resolves the endpoint and tries each address until one connects.
    ///
    /// (`SubRequestError` is `#[non_exhaustive]`, so the catch-all arm is
    /// required and lint-exempt; only `Connect` drives address fallback.)
    async fn execute_resolved(&self, request: &SubRequest, addrs: &[SocketAddr]) -> Result<Bytes, RouteError> {
        let mut last_connect_error: Option<String> = None;
        for addr in addrs {
            let peer = HttpPeer::new(*addr, self.endpoint.tls, self.endpoint.sni.clone());
            match self
                .client
                .execute(&peer, request, self.max_response_bytes, self.timeout, None)
                .await
            {
                Ok(response) => return check_status(response),
                Err(SubRequestError::Connect(message)) => last_connect_error = Some(message),
                Err(other) => return Err(RouteError::Judge(other.to_string())),
            }
        }
        let host = &self.endpoint.host;
        Err(RouteError::Judge(last_connect_error.map_or_else(
            || format!("no addresses resolved for {host}"),
            |message| format!("all resolved addresses for {host} failed: {message}"),
        )))
    }

    /// Resolves DNS and executes; the caller enforces the overall deadline.
    async fn execute_inner(&self, body: Bytes) -> Result<Bytes, RouteError> {
        let addrs = resolve(&self.endpoint.host, self.endpoint.port).await?;
        let request = self.build_request(body);
        self.execute_resolved(&request, &addrs).await
    }
}

#[async_trait]
impl JudgeTransport for SubRequestJudge<'_> {
    async fn execute(&self, body: Bytes) -> Result<Bytes, RouteError> {
        tokio::time::timeout(self.timeout, self.execute_inner(body))
            .await
            .map_err(|_elapsed| RouteError::Judge("deadline exceeded".to_owned()))?
    }
}

/// Resolves every address for the host so connects can fall back.
async fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>, RouteError> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|err| RouteError::Judge(format!("DNS resolution failed for {host}: {err}")))?
        .collect();
    if addrs.is_empty() {
        return Err(RouteError::Judge(format!("no addresses resolved for {host}")));
    }
    Ok(addrs)
}

/// Accepts 2xx judge replies and rejects everything else.
fn check_status(response: SubResponse) -> Result<Bytes, RouteError> {
    let status = response.status;
    if (200..=299).contains(&status) {
        Ok(response.body)
    } else {
        Err(RouteError::Judge(format!("judge returned HTTP {status}")))
    }
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
    use super::{JudgeEndpoint, assemble_auth, resolve_auth};
    use crate::switchyard::config::JudgeAuthConfig;

    /// An auth config with the given header and scheme (env var name unused by
    /// the pure-assembly tests).
    fn auth(header: &str, scheme: &str) -> JudgeAuthConfig {
        JudgeAuthConfig {
            value_env: "UNUSED".to_owned(),
            header: header.to_owned(),
            scheme: scheme.to_owned(),
        }
    }

    #[test]
    fn https_endpoints_parse_with_tls_and_default_port() {
        let endpoint = JudgeEndpoint::parse_url("https://judge.internal/v1/chat/completions").unwrap();
        assert!(endpoint.tls, "https enables TLS");
        assert_eq!(endpoint.port, 443, "https defaults to 443");
        assert_eq!(endpoint.sni, "judge.internal", "SNI mirrors the host");
        assert_eq!(endpoint.uri.path(), "/v1/chat/completions", "path preserved");
        assert!(endpoint.auth.is_none(), "URL parse leaves auth unresolved");
    }

    #[test]
    fn http_endpoints_parse_with_explicit_port() {
        let endpoint = JudgeEndpoint::parse_url("http://127.0.0.1:8000/v1/chat/completions?tag=x").unwrap();
        assert!(!endpoint.tls, "http stays cleartext");
        assert_eq!(endpoint.port, 8_000, "explicit port preserved");
        assert!(endpoint.sni.is_empty(), "no SNI for cleartext");
        assert_eq!(endpoint.authority.to_str().unwrap(), "127.0.0.1:8000", "authority kept");
        assert_eq!(endpoint.uri.query(), Some("tag=x"), "query preserved");
    }

    #[test]
    fn bad_endpoints_are_rejected() {
        assert!(
            JudgeEndpoint::parse_url("ftp://judge/v1").is_err(),
            "non-http scheme rejected"
        );
        assert!(
            JudgeEndpoint::parse_url("/v1/chat/completions").is_err(),
            "relative URL rejected"
        );
        assert!(JudgeEndpoint::parse_url("http://").is_err(), "missing host rejected");
    }

    #[test]
    fn bearer_credential_prefixes_the_value_and_is_sensitive() {
        let (name, value) = assemble_auth(&auth("authorization", "Bearer"), "sk-secret").unwrap();
        assert_eq!(name.as_str(), "authorization", "header name honoured");
        assert_eq!(value.to_str().unwrap(), "Bearer sk-secret", "scheme prefixes the value");
        assert!(value.is_sensitive(), "credential is marked sensitive");
    }

    #[test]
    fn an_empty_scheme_sends_the_raw_credential() {
        let (name, value) = assemble_auth(&auth("x-api-key", ""), "raw-token").unwrap();
        assert_eq!(name.as_str(), "x-api-key", "custom header honoured");
        assert_eq!(value.to_str().unwrap(), "raw-token", "no prefix with an empty scheme");
    }

    #[test]
    fn an_unset_credential_is_rejected() {
        // A name unlikely to exist in any environment this test runs in.
        let mut missing = auth("authorization", "Bearer");
        missing.value_env = "SWITCHYARD_TEST_JUDGE_KEY_DEFINITELY_ABSENT_9f3c".to_owned();
        let error = resolve_auth(&missing).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("unset or empty"),
            "a missing secret must fail fast: {message}"
        );
        assert!(
            message.contains("judge.auth"),
            "auth failures name the judge.auth key, not judge.endpoint: {message}"
        );
    }
}

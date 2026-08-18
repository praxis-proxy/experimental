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

use super::{config::JudgeConfig, error::RouteError, judge::JudgeTransport};

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
}

impl JudgeEndpoint {
    /// Parses an absolute http(s) URL into its connection components.
    ///
    /// # Errors
    ///
    /// Returns a [`FilterError`] when the URL is relative, uses a scheme other
    /// than http/https, or carries a malformed authority or path.
    pub(crate) fn parse(endpoint: &str) -> Result<Self, FilterError> {
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
        })
    }
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
    use super::JudgeEndpoint;

    #[test]
    fn https_endpoints_parse_with_tls_and_default_port() {
        let endpoint = JudgeEndpoint::parse("https://judge.internal/v1/chat/completions").unwrap();
        assert!(endpoint.tls, "https enables TLS");
        assert_eq!(endpoint.port, 443, "https defaults to 443");
        assert_eq!(endpoint.sni, "judge.internal", "SNI mirrors the host");
        assert_eq!(endpoint.uri.path(), "/v1/chat/completions", "path preserved");
    }

    #[test]
    fn http_endpoints_parse_with_explicit_port() {
        let endpoint = JudgeEndpoint::parse("http://127.0.0.1:8000/v1/chat/completions?tag=x").unwrap();
        assert!(!endpoint.tls, "http stays cleartext");
        assert_eq!(endpoint.port, 8_000, "explicit port preserved");
        assert!(endpoint.sni.is_empty(), "no SNI for cleartext");
        assert_eq!(endpoint.authority.to_str().unwrap(), "127.0.0.1:8000", "authority kept");
        assert_eq!(endpoint.uri.query(), Some("tag=x"), "query preserved");
    }

    #[test]
    fn bad_endpoints_are_rejected() {
        assert!(
            JudgeEndpoint::parse("ftp://judge/v1").is_err(),
            "non-http scheme rejected"
        );
        assert!(
            JudgeEndpoint::parse("/v1/chat/completions").is_err(),
            "relative URL rejected"
        );
        assert!(JudgeEndpoint::parse("http://").is_err(), "missing host rejected");
    }
}

//! Filter-level tests for `switchyard_route`.
//!
//! These drive the real `HttpFilter` hooks against hand-built
//! [`HttpFilterContext`]s (every field is public), with the judge served by a
//! scripted loopback HTTP stub through a real [`SubRequestClient`] — so the
//! full decision path (translate → Switchyard run → judge callout → clamp →
//! rewrite → stash) runs hermetically inside `make test`.

use std::{
    collections::HashMap,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

use bytes::Bytes;
use praxis_core::{
    id::IdGenerator,
    subrequest::{SubRequestClient, SubRequestConnector},
};
use praxis_filter::{
    BodyAccess, BodyMode, FilterAction, FilterRegistry, HttpFilter, HttpFilterContext, Request, RequestExtensions,
};
use serde_json::{Value, json};

use super::{METADATA_CLUSTER, METADATA_ERROR, METADATA_TIER, SwitchyardRouteFilter};

// ---------------------------------------------------------------------------
// Judge stub
// ---------------------------------------------------------------------------

/// A running loopback judge stub: endpoint URL, captured request bodies, and
/// the serving thread's handle.
struct JudgeStub {
    /// Full URL of the stub's chat-completions endpoint.
    endpoint: String,
    /// Raw HTTP requests captured in arrival order.
    captured: Arc<Mutex<Vec<String>>>,
    /// The serving thread; joined on drop-by-test-end via [`JudgeStub::join`].
    handle: JoinHandle<()>,
}

impl JudgeStub {
    /// Serves the scripted `(status, body)` responses, one per connection.
    fn spawn(script: Vec<(u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_in_thread = Arc::clone(&captured);
        let handle = std::thread::spawn(move || {
            for (status, body) in script {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                captured_in_thread.lock().unwrap().push(request);
                write_http_response(&mut stream, status, &body);
            }
        });
        Self {
            endpoint: format!("http://127.0.0.1:{port}/v1/chat/completions"),
            captured,
            handle,
        }
    }

    /// Waits for the whole script to have been served.
    fn join(self) -> Vec<String> {
        self.handle.join().unwrap();
        Arc::try_unwrap(self.captured).unwrap().into_inner().unwrap()
    }
}

/// Reads one HTTP/1.1 request (headers plus `Content-Length` body).
fn read_http_request(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4_096];
    loop {
        let count = stream.read(&mut chunk).unwrap();
        if count == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..count]);
        if let Some(text) = complete_request(&buffer) {
            return text;
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// Returns the request text once the `Content-Length` body has fully arrived.
fn complete_request(buffer: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(buffer);
    let (head, rest) = text.split_once("\r\n\r\n")?;
    let content_length: usize = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::to_owned)
        })?
        .trim()
        .parse()
        .ok()?;
    (rest.len() >= content_length).then(|| text.into_owned())
}

/// Writes one HTTP/1.1 response and closes the connection.
fn write_http_response(stream: &mut TcpStream, status: u16, body: &str) {
    let length = body.len();
    let response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{body}"
    );
    stream.write_all(response.as_bytes()).unwrap();
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// An OpenAI chat-completion reply whose content is a Capability verdict.
fn verdict_response(rule: &str, boundary: &str, p_solve: f64) -> String {
    let verdict = json!({
        "crux": "test crux",
        "primary_rule": rule,
        "capability_boundary": boundary,
        "p_solve": p_solve,
    })
    .to_string();
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 1,
        "model": "judge-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": verdict},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    })
    .to_string()
}

/// A verdict that classifies the task as easy (routes `weak`).
fn weak_verdict() -> (u16, String) {
    (200, verdict_response("SUP-1", "supported", 0.95))
}

/// A verdict that classifies the task as hard (routes `strong`).
fn strong_verdict() -> (u16, String) {
    (200, verdict_response("LIM-1", "unsupported", 0.05))
}

/// Filter YAML pointing the judge at `endpoint`.
fn filter_yaml(endpoint: &str, failure_mode: &str) -> String {
    format!(
        "
judge:
  endpoint: {endpoint}
  model: judge-model
  timeout_ms: 5000
targets:
  weak:
    cluster: cluster-weak
    model: model-weak
  strong:
    cluster: cluster-strong
    model: model-strong
on_failure: {failure_mode}
"
    )
}

/// Filter YAML like [`filter_yaml`], with the escalation ratchet enabled so a
/// strong-floored session skips the judge entirely.
fn ratchet_yaml(endpoint: &str) -> String {
    format!(
        "{}session_floor:\n  escalation_ratchet: true\n",
        filter_yaml(endpoint, "open")
    )
}

/// Builds the filter from YAML text.
fn build_filter(yaml: &str) -> Box<dyn HttpFilter> {
    let value: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
    SwitchyardRouteFilter::from_config(&value).unwrap()
}

/// A minimal OpenAI chat request body naming the client's current model.
fn openai_body() -> Bytes {
    Bytes::from(
        json!({
            "model": "agent-default",
            "messages": [{"role": "user", "content": "hello"}],
        })
        .to_string(),
    )
}

/// Owned parts a context borrows from.
struct CtxParts {
    /// The immutable request view.
    request: Request,
    /// The context's id generator.
    id_generator: IdGenerator,
}

impl CtxParts {
    /// Builds parts for a POST to `path` with the given extra headers.
    fn new(path: &str, headers: &[(&str, &str)]) -> Self {
        let mut header_map = http::HeaderMap::new();
        for (name, value) in headers {
            header_map.insert(
                http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        Self {
            request: Request {
                headers: header_map,
                method: http::Method::POST,
                uri: path.parse().unwrap(),
            },
            id_generator: IdGenerator::with_seed(7),
        }
    }
}

/// Builds a filter context over `parts`, mirroring praxis-filter's own
/// in-crate test constructor field for field.
fn make_ctx<'ctx>(parts: &'ctx CtxParts, client: Option<&'ctx SubRequestClient>) -> HttpFilterContext<'ctx> {
    HttpFilterContext {
        buffered_request_body: None,
        body_done_indices: Vec::new(),
        branch_iterations: HashMap::new(),
        client_addr: None,
        cluster: None,
        current_filter_id: None,
        downstream_tls: false,
        extensions: RequestExtensions::default(),
        executed_filter_indices: Vec::new(),
        extra_request_headers: Vec::new(),
        request_headers_to_remove: Vec::new(),
        request_headers_to_set: Vec::new(),
        filter_metadata: HashMap::new(),
        pre_read_mutations: Vec::new(),
        structured_metadata: HashMap::new(),
        filter_results: HashMap::new(),
        filter_state: HashMap::new(),
        health_registry: None,
        id_generator: &parts.id_generator,
        kv_stores: None,
        metrics_route: None,
        peer_identity: None,
        subrequest_client: client,
        request: &parts.request,
        request_body_bytes: 0,
        request_body_mode: BodyMode::Stream,
        request_start: std::time::Instant::now(),
        response_body_bytes: 0,
        response_body_mode: BodyMode::Stream,
        response_header: None,
        response_headers_modified: false,
        rewritten_path: None,
        selected_endpoint_index: None,
        time_source: &praxis_core::time::SystemTimeSource,
        upstream: None,
    }
}

/// A real loopback-capable sub-request client.
fn subrequest_client() -> SubRequestClient {
    SubRequestClient::new(SubRequestConnector::new(4, None))
}

/// Runs one buffered body pass and returns `(action, body, metadata)`.
async fn run_body_phase(
    filter: &dyn HttpFilter,
    client: &SubRequestClient,
    path: &str,
    headers: &[(&str, &str)],
    body_bytes: Bytes,
) -> (FilterAction, Option<Bytes>, HashMap<String, String>) {
    let parts = CtxParts::new(path, headers);
    let mut ctx = make_ctx(&parts, Some(client));
    let mut body = Some(body_bytes);
    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();
    (action, body, ctx.filter_metadata)
}

/// The `model` field of a JSON body.
fn body_model(body: Option<&Bytes>) -> String {
    let value: Value = serde_json::from_slice(body.unwrap()).unwrap();
    value["model"].as_str().unwrap().to_owned()
}

// ---------------------------------------------------------------------------
// Registration and trait surface
// ---------------------------------------------------------------------------

#[test]
fn switchyard_route_is_registered() {
    let mut registry = FilterRegistry::with_builtins();
    crate::register_filters(&mut registry);
    let names = registry.available_filters();
    assert!(
        names.contains(&"switchyard_route"),
        "expected switchyard_route to be registered, got: {names:?}"
    );
}

#[test]
fn trait_surface_matches_the_pipeline_contract() {
    let filter = build_filter(&filter_yaml("http://127.0.0.1:1/v1/chat/completions", "open"));
    assert_eq!(filter.name(), "switchyard_route", "name matches registration");
    assert!(
        filter.selects_cluster(),
        "must satisfy pipeline cluster-selector validation"
    );
    assert!(filter.needs_request_context(), "body phase reads path and headers");
    let clusters = filter.selected_clusters();
    assert!(
        clusters.contains(&"cluster-weak".to_owned()) && clusters.contains(&"cluster-strong".to_owned()),
        "both candidate clusters must be declared, got: {clusters:?}"
    );
    assert_eq!(
        filter.request_body_access(),
        BodyAccess::ReadWrite,
        "the body is mutated"
    );
    assert!(
        matches!(
            filter.request_body_mode(),
            BodyMode::StreamBuffer {
                max_bytes: Some(1_048_576)
            }
        ),
        "default 1 MiB StreamBuffer"
    );
}

#[test]
fn shared_cluster_is_declared_once() {
    let yaml = filter_yaml("http://127.0.0.1:1/v1/chat/completions", "open").replace("cluster-strong", "cluster-weak");
    let filter = build_filter(&yaml);
    assert_eq!(
        filter.selected_clusters(),
        vec!["cluster-weak".to_owned()],
        "deduplicated"
    );
}

// ---------------------------------------------------------------------------
// Phase 2: on_request
// ---------------------------------------------------------------------------

#[tokio::test]
async fn on_request_applies_the_stashed_cluster() {
    let filter = build_filter(&filter_yaml("http://127.0.0.1:1/v1/chat/completions", "open"));
    let parts = CtxParts::new("/v1/chat/completions", &[]);
    let mut ctx = make_ctx(&parts, None);
    ctx.set_metadata(METADATA_CLUSTER, "cluster-strong");
    let action = filter.on_request(&mut ctx).await.unwrap();
    assert!(matches!(action, FilterAction::Continue), "stash application continues");
    assert_eq!(ctx.cluster_name(), Some("cluster-strong"), "stashed cluster applied");
}

#[tokio::test]
async fn on_request_preserves_an_earlier_cluster_choice() {
    let filter = build_filter(&filter_yaml("http://127.0.0.1:1/v1/chat/completions", "open"));
    let parts = CtxParts::new("/v1/chat/completions", &[]);
    let mut ctx = make_ctx(&parts, None);
    ctx.cluster = Some(Arc::from("preset-cluster"));
    ctx.set_metadata(METADATA_CLUSTER, "cluster-strong");
    let action = filter.on_request(&mut ctx).await.unwrap();
    assert!(matches!(action, FilterAction::Continue), "preservation continues");
    assert_eq!(ctx.cluster_name(), Some("preset-cluster"), "earlier choice preserved");
}

#[tokio::test]
async fn on_request_without_a_stash_sets_nothing() {
    let filter = build_filter(&filter_yaml("http://127.0.0.1:1/v1/chat/completions", "open"));
    let parts = CtxParts::new("/v1/chat/completions", &[]);
    let mut ctx = make_ctx(&parts, None);
    let action = filter.on_request(&mut ctx).await.unwrap();
    assert!(matches!(action, FilterAction::Continue), "no stash still continues");
    assert_eq!(ctx.cluster_name(), None, "no decision, no cluster");
}

// ---------------------------------------------------------------------------
// Failure paths (no network needed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn early_chunks_pass_straight_through() {
    let filter = build_filter(&filter_yaml("http://127.0.0.1:1/v1/chat/completions", "open"));
    let parts = CtxParts::new("/v1/chat/completions", &[]);
    let mut ctx = make_ctx(&parts, None);
    let original = openai_body();
    let mut body = Some(original.clone());
    let action = filter.on_request_body(&mut ctx, &mut body, false).await.unwrap();
    assert!(matches!(action, FilterAction::Continue), "non-final chunks continue");
    assert_eq!(body.as_ref().unwrap(), &original, "body untouched before end of stream");
    assert!(ctx.filter_metadata.is_empty(), "no metadata before end of stream");
}

#[tokio::test]
async fn fail_open_passes_through_unmodified_without_a_client() {
    let filter = build_filter(&filter_yaml("http://127.0.0.1:1/v1/chat/completions", "open"));
    let parts = CtxParts::new("/v1/chat/completions", &[]);
    let mut ctx = make_ctx(&parts, None);
    let original = openai_body();
    let mut body = Some(original.clone());
    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();
    assert!(matches!(action, FilterAction::Continue), "fail-open continues");
    assert_eq!(
        body.as_ref().unwrap(),
        &original,
        "the client's model is never clobbered"
    );
    assert!(ctx.get_metadata(METADATA_CLUSTER).is_none(), "no cluster on failure");
    assert!(
        ctx.get_metadata(METADATA_ERROR).unwrap().contains("subrequest client"),
        "the reason is recorded"
    );
}

#[tokio::test]
async fn fail_closed_rejects_with_503() {
    let filter = build_filter(&filter_yaml("http://127.0.0.1:1/v1/chat/completions", "closed"));
    let parts = CtxParts::new("/v1/chat/completions", &[]);
    let mut ctx = make_ctx(&parts, None);
    let mut body = Some(openai_body());
    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();
    let FilterAction::Reject(rejection) = action else {
        panic!("expected Reject, got {action:?}");
    };
    assert_eq!(rejection.status, 503, "closed mode rejects with 503");
}

#[tokio::test]
async fn fail_open_passes_through_non_json_bodies() {
    let filter = build_filter(&filter_yaml("http://127.0.0.1:1/v1/chat/completions", "open"));
    let parts = CtxParts::new("/v1/chat/completions", &[]);
    let mut ctx = make_ctx(&parts, None);
    let original = Bytes::from_static(b"not json at all");
    let mut body = Some(original.clone());
    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();
    assert!(matches!(action, FilterAction::Continue), "fail-open continues");
    assert_eq!(body.as_ref().unwrap(), &original, "unparsable body forwarded as-is");
    assert!(ctx.get_metadata(METADATA_ERROR).is_some(), "the reason is recorded");
}

#[tokio::test]
async fn fail_open_passes_through_unknown_formats() {
    let filter = build_filter(&filter_yaml("http://127.0.0.1:1/v1/chat/completions", "open"));
    let parts = CtxParts::new("/unknown/endpoint", &[]);
    let mut ctx = make_ctx(&parts, None);
    let original = Bytes::from(json!({"model": "agent-default", "input": "hi"}).to_string());
    let mut body = Some(original.clone());
    let action = filter.on_request_body(&mut ctx, &mut body, true).await.unwrap();
    assert!(matches!(action, FilterAction::Continue), "fail-open continues");
    assert_eq!(body.as_ref().unwrap(), &original, "unroutable format forwarded as-is");
    assert!(
        ctx.get_metadata(METADATA_ERROR).unwrap().contains("wire format"),
        "the reason is recorded"
    );
}

// ---------------------------------------------------------------------------
// End-to-end decisions through the loopback judge
// ---------------------------------------------------------------------------

#[tokio::test]
async fn easy_requests_route_to_the_weak_tier() {
    let stub = JudgeStub::spawn(vec![weak_verdict()]);
    let filter = build_filter(&filter_yaml(&stub.endpoint, "open"));
    let client = subrequest_client();
    let (action, body, metadata) =
        run_body_phase(filter.as_ref(), &client, "/v1/chat/completions", &[], openai_body()).await;
    assert!(matches!(action, FilterAction::Continue), "successful routing continues");
    assert_eq!(
        body_model(body.as_ref()),
        "model-weak",
        "body model rewritten to the weak tier"
    );
    assert_eq!(metadata.get(METADATA_CLUSTER).map(String::as_str), Some("cluster-weak"));
    assert_eq!(metadata.get(METADATA_TIER).map(String::as_str), Some("weak"));
    let captured = stub.join();
    assert!(
        captured[0].contains("\"model\":\"judge-model\""),
        "the judge sees the configured judge model: {}",
        captured[0]
    );
    assert!(
        captured[0].contains("json_schema"),
        "the classifier contract's response_format reaches the wire"
    );
}

#[tokio::test]
async fn hard_requests_route_to_the_strong_tier() {
    let stub = JudgeStub::spawn(vec![strong_verdict()]);
    let filter = build_filter(&filter_yaml(&stub.endpoint, "open"));
    let client = subrequest_client();
    let (action, body, metadata) =
        run_body_phase(filter.as_ref(), &client, "/v1/chat/completions", &[], openai_body()).await;
    assert!(matches!(action, FilterAction::Continue), "successful routing continues");
    assert_eq!(
        body_model(body.as_ref()),
        "model-strong",
        "body model rewritten to the strong tier"
    );
    assert_eq!(
        metadata.get(METADATA_CLUSTER).map(String::as_str),
        Some("cluster-strong")
    );
    stub.join();
}

#[tokio::test]
async fn anthropic_requests_route_and_preserve_provider_fields() {
    let stub = JudgeStub::spawn(vec![weak_verdict()]);
    let filter = build_filter(&filter_yaml(&stub.endpoint, "open"));
    let client = subrequest_client();
    let anthropic = Bytes::from(
        json!({
            "model": "agent-default",
            "max_tokens": 256,
            "system": "be terse",
            "messages": [{"role": "user", "content": "hello"}],
        })
        .to_string(),
    );
    let (_, body, metadata) = run_body_phase(filter.as_ref(), &client, "/v1/messages", &[], anthropic).await;
    assert_eq!(
        body_model(body.as_ref()),
        "model-weak",
        "Anthropic body model rewritten"
    );
    let value: Value = serde_json::from_slice(body.as_ref().unwrap()).unwrap();
    assert_eq!(value["max_tokens"], 256, "provider-specific fields preserved");
    assert_eq!(value["system"], "be terse", "provider-specific fields preserved");
    assert_eq!(metadata.get(METADATA_CLUSTER).map(String::as_str), Some("cluster-weak"));
    stub.join();
}

// ---------------------------------------------------------------------------
// The no-downgrade guarantee
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_strong_session_never_downgrades() {
    let stub = JudgeStub::spawn(vec![strong_verdict(), weak_verdict()]);
    let filter = build_filter(&filter_yaml(&stub.endpoint, "open"));
    let client = subrequest_client();
    let session = &[("x-switchyard-session-id", "session-ratchet")];

    let (_, first_body, _) =
        run_body_phase(filter.as_ref(), &client, "/v1/chat/completions", session, openai_body()).await;
    assert_eq!(body_model(first_body.as_ref()), "model-strong", "turn 1 reaches strong");

    let (_, second_body, metadata) =
        run_body_phase(filter.as_ref(), &client, "/v1/chat/completions", session, openai_body()).await;
    assert_eq!(
        body_model(second_body.as_ref()),
        "model-strong",
        "turn 2 must hold strong even when the judge says weak"
    );
    assert_eq!(metadata.get(METADATA_TIER).map(String::as_str), Some("strong"));
    stub.join();
}

#[tokio::test]
async fn the_escalation_ratchet_skips_the_judge_once_strong() {
    // Only ONE verdict is scripted, yet TWO turns run. The stub serves exactly
    // as many connections as scripted, so a second judge call would hang the
    // turn waiting for a response that never comes. That it completes — and that
    // exactly one request is captured — proves the ratcheted turn skipped the
    // judge entirely, not merely that its verdict was discarded.
    let stub = JudgeStub::spawn(vec![strong_verdict()]);
    let filter = build_filter(&ratchet_yaml(&stub.endpoint));
    let client = subrequest_client();
    let session = &[("x-switchyard-session-id", "session-ratchet-skip")];

    let (_, first_body, _) =
        run_body_phase(filter.as_ref(), &client, "/v1/chat/completions", session, openai_body()).await;
    assert_eq!(body_model(first_body.as_ref()), "model-strong", "turn 1 reaches strong");

    let (_, second_body, metadata) =
        run_body_phase(filter.as_ref(), &client, "/v1/chat/completions", session, openai_body()).await;
    assert_eq!(
        body_model(second_body.as_ref()),
        "model-strong",
        "turn 2 stays strong with no judge call"
    );
    assert_eq!(metadata.get(METADATA_TIER).map(String::as_str), Some("strong"));

    let captured = stub.join();
    assert_eq!(
        captured.len(),
        1,
        "the judge was called exactly once, not on the ratcheted turn"
    );
}

#[tokio::test]
async fn without_the_ratchet_a_strong_session_still_judges_every_turn() {
    // The default (ratchet off) is Switchyard vanilla: the judge runs every
    // turn even on a strong-floored session. Two verdicts are scripted and both
    // must be consumed; the floor clamps the weak second verdict up to strong.
    let stub = JudgeStub::spawn(vec![strong_verdict(), weak_verdict()]);
    let filter = build_filter(&filter_yaml(&stub.endpoint, "open"));
    let client = subrequest_client();
    let session = &[("x-switchyard-session-id", "session-no-ratchet")];

    for _ in 0..2 {
        let (_action, _body, _metadata) =
            run_body_phase(filter.as_ref(), &client, "/v1/chat/completions", session, openai_body()).await;
    }

    let captured = stub.join();
    assert_eq!(captured.len(), 2, "vanilla behaviour judges both turns");
}

#[tokio::test]
async fn a_judge_failure_never_forces_a_tier() {
    let stub = JudgeStub::spawn(vec![strong_verdict(), (500, "oops".to_owned())]);
    let filter = build_filter(&filter_yaml(&stub.endpoint, "open"));
    let client = subrequest_client();
    let session = &[("x-switchyard-session-id", "session-failure")];

    let (_, first_body, _) =
        run_body_phase(filter.as_ref(), &client, "/v1/chat/completions", session, openai_body()).await;
    assert_eq!(body_model(first_body.as_ref()), "model-strong", "turn 1 reaches strong");

    let original = openai_body();
    let (action, second_body, metadata) = run_body_phase(
        filter.as_ref(),
        &client,
        "/v1/chat/completions",
        session,
        original.clone(),
    )
    .await;
    assert!(matches!(action, FilterAction::Continue), "fail-open continues");
    assert_eq!(
        second_body.as_ref().unwrap(),
        &original,
        "a judge failure leaves the client's model untouched"
    );
    assert!(!metadata.contains_key(METADATA_CLUSTER), "no cluster on failure");
    assert!(
        metadata.get(METADATA_ERROR).unwrap().contains("judge"),
        "the judge failure is recorded"
    );
    stub.join();
}

#[tokio::test]
async fn the_final_turn_evicts_the_floor() {
    let stub = JudgeStub::spawn(vec![strong_verdict(), weak_verdict(), weak_verdict()]);
    let filter = build_filter(&filter_yaml(&stub.endpoint, "open"));
    let client = subrequest_client();
    let session = &[("x-switchyard-session-id", "session-final")];
    let final_turn = &[
        ("x-switchyard-session-id", "session-final"),
        ("x-switchyard-session-final", "true"),
    ];

    let (_, first_body, _) =
        run_body_phase(filter.as_ref(), &client, "/v1/chat/completions", session, openai_body()).await;
    assert_eq!(body_model(first_body.as_ref()), "model-strong", "turn 1 reaches strong");

    let (_, final_body, _) = run_body_phase(
        filter.as_ref(),
        &client,
        "/v1/chat/completions",
        final_turn,
        openai_body(),
    )
    .await;
    assert_eq!(
        body_model(final_body.as_ref()),
        "model-strong",
        "the floor still holds on the final turn"
    );

    let (_, next_body, _) =
        run_body_phase(filter.as_ref(), &client, "/v1/chat/completions", session, openai_body()).await;
    assert_eq!(
        body_model(next_body.as_ref()),
        "model-weak",
        "a fresh session after eviction follows the judge again"
    );
    stub.join();
}

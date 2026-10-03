//! Manual credential verification regression tests.
#[expect(clippy::allow_attributes, reason = "test assertions and fixtures")]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, reason = "tests")]
mod cases {
    use std::{sync::LazyLock, time::Instant};

    use jsonwebtoken::{Algorithm, EncodingKey, Header};
    use praxis_filter::{BodyAccess, BodyMode, FilterAction, HttpFilter, HttpFilterContext, Request};
    use sha2::{Digest as _, Sha256};

    use super::super::{
        ManualJwtFilter,
        config::{Config, Mode},
        verifier::{AuthError, Verifier},
    };

    /// Deterministic ID generator for the test filter context.
    static TEST_IDS: LazyLock<praxis_core::id::IdGenerator> =
        LazyLock::new(|| praxis_core::id::IdGenerator::with_seed(0));

    /// Wall-clock source for the test filter context.
    static TEST_TIME: praxis_core::time::SystemTimeSource = praxis_core::time::SystemTimeSource;

    /// Builds a POST request for `path`.
    fn make_request(path: &str) -> Request {
        Request {
            method: http::Method::POST,
            uri: path.parse().expect("test path is a valid URI"),
            headers: http::HeaderMap::new(),
        }
    }

    #[expect(clippy::too_many_lines, reason = "complete framework context fixture")]
    /// Builds a filter context, mirroring every field of `HttpFilterContext`.
    fn make_ctx(request: &Request) -> HttpFilterContext<'_> {
        HttpFilterContext {
            buffered_request_body: None,
            body_done_indices: Vec::new(),
            branch_iterations: std::collections::HashMap::new(),
            client_addr: None,
            cluster: None,
            current_filter_id: None,
            downstream_tls: false,
            metrics_route: None,
            peer_identity: None,
            extensions: praxis_filter::RequestExtensions::default(),
            executed_filter_indices: Vec::new(),
            extra_request_headers: Vec::new(),
            request_headers_to_remove: Vec::new(),
            request_headers_to_set: Vec::new(),
            filter_metadata: std::collections::HashMap::new(),
            grpc_completion: None,
            prior_pre_read_mutations: Vec::new(),
            pre_read_mutations: Vec::new(),
            structured_metadata: std::collections::HashMap::new(),
            filter_results: std::collections::HashMap::new(),
            filter_state: std::collections::HashMap::new(),
            health_registry: None,
            id_generator: &TEST_IDS,
            kv_stores: None,
            session_stores: None,
            subrequest_client: None,
            subrequest_response_mode: praxis_filter::SubRequestResponseMode::Buffered,
            request,
            request_body_bytes: 0,
            request_body_mode: BodyMode::Stream,
            request_start: Instant::now(),
            response_body_bytes: 0,
            response_body_mode: BodyMode::Stream,
            response_header: None,
            response_headers_modified: false,
            upstream_reached: false,
            selected_endpoint_index: None,
            attempted_endpoints: Vec::new(),
            retry_policy: None,
            route_retry_policy: None,
            cluster_retry_state: None,
            cluster_retry_state_released: false,
            endpoint_reselector: None,
            pinned_endpoint_address: None,
            time_source: &TEST_TIME,
            rewritten_path: None,
            upstream: None,
        }
    }

    fn fixture() -> (tempfile::TempDir, Verifier, EncodingKey) {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("private.pem");
        assert!(
            std::process::Command::new("openssl")
                .arg("genpkey")
                .args(["-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out"])
                .arg(&key)
                .output()
                .unwrap()
                .status
                .success(),
            "generate fixture RSA key"
        );
        let public = std::process::Command::new("openssl")
            .args(["pkey", "-pubout", "-in"])
            .arg(&key)
            .output()
            .unwrap();
        std::fs::write(dir.path().join("public.pem"), public.stdout).unwrap();
        let verifier = Verifier::new(&Config {
            mode: Mode::Inference,
            public_key_file: dir.path().join("public.pem"),
            registry_file: dir.path().join("users.json"),
            issuer: "issuer".into(),
            audience: "audience".into(),
            allowed_origins: Vec::new(),
        })
        .unwrap();
        let signing = EncodingKey::from_rsa_pem(&std::fs::read(key).unwrap()).unwrap();
        (dir, verifier, signing)
    }

    fn token(signing: &EncodingKey, extra: &serde_json::Value) -> String {
        let mut claims = serde_json::json!({"iss":"issuer","aud":"audience","sub":"alice","iat":jsonwebtoken::get_current_timestamp(),"jti":"one"});
        claims
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, signing).unwrap()
    }

    fn register(dir: &tempfile::TempDir, token: &str, active: bool) {
        let registry = serde_json::json!({"version":1,"users":{"alice":{"digest":hex::encode(Sha256::digest(token.as_bytes())),"active":active}}});
        std::fs::write(dir.path().join("users.json"), registry.to_string()).unwrap();
    }

    #[tokio::test]
    async fn indefinite_rotation_and_revocation_preserve_identity() {
        let (dir, verifier, signing) = fixture();
        let original = token(&signing, &serde_json::json!({}));
        register(&dir, &original, true);
        assert_eq!(verifier.authenticate(&original).await.unwrap(), "alice");
        register(&dir, &original, false);
        assert!(
            matches!(verifier.authenticate(&original).await, Err(AuthError::Invalid)),
            "revocation must be immediate"
        );
        let replacement = token(&signing, &serde_json::json!({"jti":"two"}));
        register(&dir, &replacement, true);
        assert!(
            matches!(verifier.authenticate(&original).await, Err(AuthError::Invalid)),
            "rotation revokes old token"
        );
        assert_eq!(verifier.authenticate(&replacement).await.unwrap(), "alice");
        std::fs::write(dir.path().join("users.json"), "broken").unwrap();
        assert!(
            matches!(verifier.authenticate(&replacement).await, Err(AuthError::Unavailable)),
            "registry failure must fail closed"
        );
    }

    #[tokio::test]
    async fn rfc7519_registered_claims_are_checked_when_present() {
        let (dir, verifier, signing) = fixture();
        for extra in [
            serde_json::json!({"iss":"wrong"}),
            serde_json::json!({"aud":"wrong"}),
            serde_json::json!({"exp":1}),
            serde_json::json!({"nbf":4_000_000_000_u64}),
            serde_json::json!({"iat":4_000_000_000_u64}),
            serde_json::json!({"sub":"admin\r\nX: bad"}),
            serde_json::json!({"jti":""}),
        ] {
            let credential = token(&signing, &extra);
            register(&dir, &credential, true);
            assert!(
                matches!(verifier.authenticate(&credential).await, Err(AuthError::Invalid)),
                "registered hash cannot bypass JWT validation"
            );
        }
    }

    #[tokio::test]
    async fn unregistered_or_unsigned_tokens_are_rejected() {
        let (dir, verifier, signing) = fixture();
        let good = token(&signing, &serde_json::json!({}));
        register(&dir, &good, true);
        let different = token(&signing, &serde_json::json!({"jti":"other"}));
        assert!(
            matches!(verifier.authenticate(&different).await, Err(AuthError::Invalid)),
            "signature alone is not admission"
        );
        assert!(
            matches!(verifier.authenticate("broken").await, Err(AuthError::Invalid)),
            "malformed token"
        );
        let (_, _, other_key) = fixture();
        let forged = token(&other_key, &serde_json::json!({}));
        register(&dir, &forged, true);
        assert!(
            matches!(verifier.authenticate(&forged).await, Err(AuthError::Invalid)),
            "wrong signature"
        );
    }

    #[tokio::test]
    #[expect(clippy::too_many_lines, reason = "sequential authentication boundary assertions")]
    async fn listener_roles_and_spoofed_headers() {
        let (dir, _verifier, signing) = fixture();
        let credential = token(&signing, &serde_json::json!({}));
        register(&dir, &credential, true);
        let make = |mode: &str| {
            let config = serde_json::json!({"mode":mode,"public_key_file":dir.path().join("public.pem"),"registry_file":dir.path().join("users.json"),"issuer":"issuer","audience":"audience","allowed_origins":["https://gateway.test"]});
            ManualJwtFilter::from_config(&serde_yaml::to_value(config).unwrap()).unwrap()
        };
        let inference = make("inference");
        assert_eq!(inference.name(), "manual_jwt", "registered name");
        assert_eq!(inference.request_body_access(), BodyAccess::None);
        let mut request = make_request("/v1/chat/completions");
        assert_status(apply(inference.as_ref(), &request).await, 401);
        request
            .headers
            .insert("authorization", format!("Bearer {credential}").parse().unwrap());
        request.headers.insert("x-tenant-username", "admin".parse().unwrap());
        let mut context = make_ctx(&request);
        assert_continues(&inference.on_request(&mut context).await.unwrap());
        assert!(
            context
                .request_headers_to_set
                .iter()
                .any(|(name, value)| name == "x-tenant-username" && value == "alice"),
            "verified identity replaces caller"
        );
        assert!(
            context
                .request_headers_to_remove
                .iter()
                .any(|name| name == "authorization"),
            "credentials consumed"
        );
        request.headers.append("authorization", "Bearer other".parse().unwrap());
        assert_status(apply(inference.as_ref(), &request).await, 401);
        let callback = make("callback");
        assert_eq!(
            callback.request_body_mode(),
            BodyMode::StreamBuffer {
                max_bytes: Some(16_384)
            },
            "callback body is bounded"
        );
        let mut callback_request = make_request("/validate");
        let mut callback_context = make_ctx(&callback_request);
        assert_status(callback.on_request(&mut callback_context).await.unwrap(), 400);
        callback_context.buffered_request_body = Some(
            serde_json::to_vec(&serde_json::json!({"key":credential}))
                .unwrap()
                .into(),
        );
        let FilterAction::Reject(response) = callback.on_request(&mut callback_context).await.unwrap() else {
            panic!("callback must answer locally")
        };
        assert_eq!(response.status, 200, "valid callback");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&response.body.unwrap()).unwrap(),
            serde_json::json!({"valid":true,"username":"alice","groups":[]}),
            "MaaS-compatible response"
        );
        callback_context.buffered_request_body = Some(bytes::Bytes::from_static(b"{\"key\":\"invalid\"}"));
        assert_status(callback.on_request(&mut callback_context).await.unwrap(), 401);
        callback_context.buffered_request_body = Some(bytes::Bytes::from(vec![0; 16_385]));
        assert_status(callback.on_request(&mut callback_context).await.unwrap(), 413);
        callback_context.buffered_request_body = Some(bytes::Bytes::from_static(b"broken"));
        assert_status(callback.on_request(&mut callback_context).await.unwrap(), 400);
        callback_request.method = http::Method::GET;
        assert_status(apply(callback.as_ref(), &callback_request).await, 405);
        assert_status(apply(callback.as_ref(), &make_request("/other")).await, 404);
        let dashboard = make("dashboard");
        let mut browser_request = make_request("/login");
        assert_status(apply(dashboard.as_ref(), &browser_request).await, 403);
        browser_request
            .headers
            .insert("origin", "https://attacker.test".parse().unwrap());
        assert_status(apply(dashboard.as_ref(), &browser_request).await, 403);
        browser_request
            .headers
            .insert("origin", "https://gateway.test".parse().unwrap());
        assert_continues(&apply(dashboard.as_ref(), &browser_request).await);
        browser_request.method = http::Method::GET;
        assert_continues(&apply(dashboard.as_ref(), &browser_request).await);
    }

    #[tokio::test]
    async fn registry_errors_and_optional_expiry() {
        let (dir, verifier, signing) = fixture();
        let credential = token(&signing, &serde_json::json!({"exp":4_000_000_000_u64}));
        register(&dir, &credential, true);
        assert_eq!(verifier.authenticate(&credential).await.unwrap(), "alice");
        for invalid in [
            "{}",
            r#"{"version":2,"users":{}}"#,
            r#"{"version":1,"users":{"bad subject":{"active":true,"digest":"bad"}}}"#,
        ] {
            std::fs::write(dir.path().join("users.json"), invalid).unwrap();
            assert!(
                matches!(verifier.authenticate(&credential).await, Err(AuthError::Unavailable)),
                "invalid registry fails closed"
            );
        }
        std::fs::write(dir.path().join("users.json"), vec![b' '; 1_048_577]).unwrap();
        assert!(
            matches!(verifier.authenticate(&credential).await, Err(AuthError::Unavailable)),
            "bounded registry"
        );
        std::fs::remove_file(dir.path().join("users.json")).unwrap();
        assert!(
            matches!(verifier.authenticate(&credential).await, Err(AuthError::Unavailable)),
            "missing registry"
        );
        assert!(
            matches!(verifier.authenticate(&"x".repeat(8193)).await, Err(AuthError::Invalid)),
            "bounded credential"
        );
    }

    #[test]
    fn rejects_invalid_configuration() {
        let (dir, _verifier, _signing) = fixture();
        let config = serde_json::json!({"mode":"inference","public_key_file":dir.path().join("public.pem"),"registry_file":dir.path().join("users.json"),"issuer":"issuer","audience":"audience"});
        for changes in [
            serde_json::json!({"issuer":""}),
            serde_json::json!({"audience":""}),
            serde_json::json!({"mode":"dashboard"}),
            serde_json::json!({"mode":"dashboard","allowed_origins":["http://wrong"]}),
            serde_json::json!({"mode":"unknown"}),
            serde_json::json!({"unknown":true}),
        ] {
            let mut invalid = config.clone();
            invalid
                .as_object_mut()
                .unwrap()
                .extend(changes.as_object().unwrap().clone());
            let Err(_) = ManualJwtFilter::from_config(&serde_yaml::to_value(invalid).unwrap()) else {
                panic!("bad trust configuration was accepted");
            };
        }
    }

    /// Invoke a filter with an unbuffered request fixture.
    async fn apply(filter: &dyn HttpFilter, request: &Request) -> FilterAction {
        filter.on_request(&mut make_ctx(request)).await.unwrap()
    }

    /// Require admission without discarding an unexpected rejection.
    fn assert_continues(action: &FilterAction) {
        assert!(matches!(action, FilterAction::Continue), "expected admission");
    }

    /// Compare explicit local responses without discarding the unexpected action.
    fn assert_status(action: FilterAction, status: u16) {
        let FilterAction::Reject(response) = action else {
            panic!("expected local response")
        };
        assert_eq!(response.status, status, "expected authentication result");
    }
}

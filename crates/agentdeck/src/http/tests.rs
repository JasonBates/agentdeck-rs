use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::json;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use tower::ServiceExt;

use super::*;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

#[derive(Default)]
struct MockActions {
    calls: Mutex<Vec<String>>,
    fail: AtomicBool,
}

#[async_trait]
impl HerdrActions for MockActions {
    async fn focus_pane(&self, pane_id: &str) -> Result<(), ActionError> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(format!("pane:{pane_id}"));
        self.result()
    }

    async fn focus_workspace(&self, workspace_id: &str) -> Result<(), ActionError> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(format!("workspace:{workspace_id}"));
        self.result()
    }

    async fn create_tab(&self, workspace_id: &str) -> Result<(), ActionError> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(format!("tab:{workspace_id}"));
        self.result()
    }
}

impl MockActions {
    fn result(&self) -> Result<(), ActionError> {
        if self.fail.load(Ordering::Acquire) {
            Err(ActionError::HerdrUnavailable)
        } else {
            Ok(())
        }
    }
}

fn health_report() -> HealthReport {
    HealthReport {
        runtime_version: SafeVersion::new("0.1.0")
            .unwrap_or_else(|| panic!("fixture version must be safe")),
        status: HealthStatus::Degraded,
        herdr: AdapterHealth {
            state: HealthState::Unavailable,
            version: SafeVersion::new("0.8.2"),
            last_success_unix_seconds: Some(42),
            reason: Some(HealthReason::ConnectionFailed),
        },
        capabilities: BTreeMap::from([(
            CapabilityName::Headings,
            CapabilityHealth {
                state: HealthState::Disabled,
                backend: Some(HealthBackend::None),
                reason: None,
            },
        )]),
        adapters: BTreeMap::new(),
        degraded_reasons: vec![HealthReason::HerdrUnavailable],
    }
}

fn options() -> HttpOptions {
    HttpOptions {
        listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 4242)),
        base_path: "/".to_owned(),
        public_dir: None,
        public_host: None,
        allowed_origins: Vec::new(),
        auth_token: None,
        max_sse_clients: 4,
        liveness_interval: Duration::from_secs(5),
    }
}

fn server_with(options: HttpOptions) -> (HttpServer, Arc<StateHub>, Arc<MockActions>) {
    let states = Arc::new(
        StateHub::new(&json!({"n": 1})).unwrap_or_else(|error| panic!("state hub: {error}")),
    );
    let actions = Arc::new(MockActions::default());
    let server = HttpServer::build(
        options,
        states.clone(),
        actions.clone(),
        Arc::new(StaticHealth(health_report())),
    )
    .unwrap_or_else(|error| panic!("HTTP server: {error}"));
    (server, states, actions)
}

async fn request(server: &HttpServer, request: Request<Body>) -> Response {
    server
        .router()
        .oneshot(request)
        .await
        .unwrap_or_else(|error| match error {})
}

async fn body(response: Response) -> Vec<u8> {
    to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap_or_else(|error| panic!("body: {error}"))
        .to_vec()
}

async fn raw_http_response(build: impl FnOnce(SocketAddr) -> Vec<u8>) -> Vec<u8> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap_or_else(|error| panic!("test listener must bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("test listener address must be available: {error}"));
    let mut configured = options();
    configured.listen = address;
    let (server, _, _) = server_with(configured);
    let cancellation = CancellationToken::new();
    let serve_cancellation = cancellation.clone();
    let task =
        tokio::spawn(
            async move { serve_http(listener, server.router(), serve_cancellation).await },
        );

    let mut stream = TcpStream::connect(address)
        .await
        .unwrap_or_else(|error| panic!("test client must connect: {error}"));
    stream
        .write_all(&build(address))
        .await
        .unwrap_or_else(|error| panic!("test request must write: {error}"));
    let mut response = Vec::new();
    match timeout(Duration::from_secs(2), stream.read_to_end(&mut response)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) if !response.is_empty() => {
            let _connection_close_error = error;
        }
        Ok(Err(error)) => panic!("test response must read: {error}"),
        Err(_) => panic!("test response timed out"),
    }

    cancellation.cancel();
    timeout(Duration::from_secs(2), task)
        .await
        .unwrap_or_else(|_| panic!("test server did not stop"))
        .unwrap_or_else(|error| panic!("test server task failed: {error}"))
        .unwrap_or_else(|error| panic!("test server failed: {error}"));
    response
}

fn assert_security_headers(response: &Response) {
    assert!(response.headers().contains_key(header::CACHE_CONTROL));
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.headers()["referrer-policy"], "no-referrer");
    assert_eq!(response.headers()["x-frame-options"], "DENY");
    assert!(
        response
            .headers()
            .contains_key(header::CONTENT_SECURITY_POLICY)
    );
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .header(header::HOST, "localhost:4242")
        .body(Body::empty())
        .unwrap_or_else(|error| panic!("request: {error}"))
}

fn post(path: &str, content_type: &str, value: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::HOST, "localhost:4242")
        .body(Body::from(value))
        .unwrap_or_else(|error| panic!("request: {error}"))
}

async fn next_data(body: &mut Body) -> Option<Vec<u8>> {
    loop {
        let frame = body.frame().await?;
        let frame = frame.unwrap_or_else(|error| panic!("stream frame: {error}"));
        if let Ok(data) = frame.into_data() {
            return Some(data.to_vec());
        }
    }
}

#[tokio::test]
async fn embedded_routes_base_paths_methods_and_headers() {
    let mut configured = options();
    configured.base_path = "/deck".to_owned();
    let (server, _, _) = server_with(configured);

    for path in ["/", "/index.html", "/deck", "/deck/", "/deck/index.html"] {
        let response = request(&server, get(path)).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        assert_eq!(response.headers()["x-frame-options"], "DENY");
        assert!(
            response
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY)
        );
        assert!(body(response).await.starts_with(b"<!doctype html>"));
    }
    for path in ["/docs/setup.html", "/deck/docs/setup.html"] {
        let response = request(&server, get(path)).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert!(
            String::from_utf8_lossy(&body(response).await).contains("Recommended setup"),
            "{path} did not serve setup docs"
        );
    }
    for path in ["/api/snapshot", "/deck/api/snapshot"] {
        assert_eq!(
            request(&server, get(path)).await.status(),
            StatusCode::OK,
            "{path}"
        );
    }
    for path in ["/api/focus", "/deck/api/focus"] {
        assert_eq!(
            request(
                &server,
                post(path, "application/json", br#"{"paneId":"w1:p1"}"#.to_vec())
            )
            .await
            .status(),
            StatusCode::OK,
            "{path}"
        );
    }
    assert_eq!(
        request(&server, get("/deckevil/api/snapshot"))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    let wrong_method = request(
        &server,
        Request::builder()
            .method(Method::GET)
            .uri("/api/focus")
            .header(header::HOST, "localhost:4242")
            .body(Body::empty())
            .unwrap_or_else(|error| panic!("request: {error}")),
    )
    .await;
    assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        request(&server, get("/unknown")).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &server,
            post("/unknown", "application/json", b"{}".to_vec())
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn every_declared_route_distinguishes_supported_wrong_and_unknown_methods() {
    let (server, _, _) = server_with(options());
    let routes: [(&str, Method); 9] = [
        ("/", Method::GET),
        ("/index.html", Method::GET),
        ("/docs/setup.html", Method::GET),
        ("/api/snapshot", Method::GET),
        ("/api/health", Method::GET),
        ("/events", Method::GET),
        ("/api/focus", Method::POST),
        ("/api/workspace", Method::POST),
        ("/api/tab", Method::POST),
    ];
    for (path, supported) in routes {
        for method in [
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ] {
            let expected = if method == supported
                || (method == Method::HEAD && supported == Method::GET)
            {
                StatusCode::OK
            } else if method == Method::OPTIONS && (path.starts_with("/api/") || path == "/events")
            {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::METHOD_NOT_ALLOWED
            };
            let request_value = Request::builder()
                .method(method.clone())
                .uri(path)
                .header(header::HOST, "localhost:4242")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(if path == "/api/focus" {
                    br#"{"paneId":"w1:p1"}"#.as_slice()
                } else {
                    br#"{"workspaceId":"w1"}"#.as_slice()
                }))
                .unwrap_or_else(|error| panic!("request: {error}"));
            let response = request(&server, request_value).await;
            assert_eq!(response.status(), expected, "{method} {path}");
            assert_security_headers(&response);
            drop(response);
        }
    }
    for method in [Method::GET, Method::POST, Method::OPTIONS] {
        let response = request(
            &server,
            Request::builder()
                .method(method.clone())
                .uri("/not-a-route")
                .header(header::HOST, "localhost:4242")
                .body(Body::empty())
                .unwrap_or_else(|error| panic!("request: {error}")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method}");
        assert_security_headers(&response);
    }
}

#[tokio::test]
async fn every_http_error_class_keeps_security_and_no_store_headers() {
    let (server, _, actions) = server_with(options());
    let bad_request = request(
        &server,
        post("/api/focus", "application/json", br#"{}"#.to_vec()),
    )
    .await;
    assert_eq!(bad_request.status(), StatusCode::BAD_REQUEST);
    assert_security_headers(&bad_request);

    let forbidden = request(
        &server,
        Request::builder()
            .uri("/api/snapshot")
            .header(header::HOST, "evil.example:4242")
            .body(Body::empty())
            .unwrap_or_else(|error| panic!("request: {error}")),
    )
    .await;
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    assert_security_headers(&forbidden);

    for request_value in [
        get("/missing"),
        Request::builder()
            .method(Method::GET)
            .uri("/api/focus")
            .header(header::HOST, "localhost:4242")
            .body(Body::empty())
            .unwrap_or_else(|error| panic!("request: {error}")),
    ] {
        let response = request(&server, request_value).await;
        assert!(matches!(
            response.status(),
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
        ));
        assert_security_headers(&response);
    }

    actions.fail.store(true, Ordering::Release);
    let unavailable = request(
        &server,
        post(
            "/api/focus",
            "application/json",
            br#"{"paneId":"w1:p1"}"#.to_vec(),
        ),
    )
    .await;
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_security_headers(&unavailable);

    let mut remote = options();
    remote.listen = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 8080));
    remote.auth_token = Some(TOKEN.to_owned());
    remote.allowed_origins = vec!["https://deck.example".to_owned()];
    let (remote_server, _, _) = server_with(remote);
    let unauthorized = request(
        &remote_server,
        Request::builder()
            .uri("/api/snapshot")
            .header(header::HOST, "deck.example")
            .body(Body::empty())
            .unwrap_or_else(|error| panic!("request: {error}")),
    )
    .await;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    assert_security_headers(&unauthorized);
}

#[tokio::test]
async fn snapshot_and_health_are_json_no_store_and_redacted_by_type() {
    let (server, _, _) = server_with(options());
    let snapshot = request(&server, get("/api/snapshot")).await;
    assert_eq!(snapshot.status(), StatusCode::OK);
    assert_eq!(snapshot.headers()[header::CONTENT_TYPE], "application/json");
    assert_eq!(snapshot.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(body(snapshot).await, br#"{"n":1}"#);

    let health = request(&server, get("/api/health")).await;
    assert_eq!(health.status(), StatusCode::OK);
    let bytes = body(health).await;
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("runtimeVersion"));
    assert!(text.contains("connection_failed"));
    for forbidden in ["prompt", "transcript", "session", TOKEN] {
        assert!(
            !text.to_ascii_lowercase().contains(forbidden),
            "leaked {forbidden}"
        );
    }
}

#[tokio::test]
async fn production_listener_enforces_request_head_byte_and_field_caps() {
    let accepted = raw_http_response(|address| {
        let mut request =
            format!("GET /api/snapshot HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n");
        for index in 0..(MAX_REQUEST_HEADER_FIELDS - 2) {
            request.push_str(&format!("X-Field-{index}: a\r\n"));
        }
        request.push_str("\r\n");
        request.into_bytes()
    })
    .await;
    assert!(
        accepted.starts_with(b"HTTP/1.1 200"),
        "exact field limit was rejected: {}",
        String::from_utf8_lossy(&accepted)
    );

    let too_many_fields = raw_http_response(|address| {
        let mut request =
            format!("GET /api/snapshot HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n");
        for index in 0..(MAX_REQUEST_HEADER_FIELDS - 1) {
            request.push_str(&format!("X-Field-{index}: a\r\n"));
        }
        request.push_str("\r\n");
        request.into_bytes()
    })
    .await;
    assert!(
        too_many_fields.starts_with(b"HTTP/1.1 431"),
        "field cap response was not 431: {}",
        String::from_utf8_lossy(&too_many_fields)
    );

    let too_many_bytes = raw_http_response(|address| {
        format!(
            "GET /api/snapshot HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nX-Fill: {}\r\n\r\n",
            "a".repeat(MAX_REQUEST_HEAD_BYTES)
        )
        .into_bytes()
    })
    .await;
    assert!(
        too_many_bytes.starts_with(b"HTTP/1.1 431"),
        "request-head byte cap response was not 431: {}",
        String::from_utf8_lossy(&too_many_bytes)
    );
}

#[tokio::test]
async fn mutations_validate_json_content_type_shape_ids_and_body_limit() {
    let (server, _, actions) = server_with(options());
    for (path, body_value, expected_call) in [
        (
            "/api/focus",
            br#"{"paneId":"w1:p1"}"#.as_slice(),
            "pane:w1:p1",
        ),
        (
            "/api/workspace",
            br#"{"workspaceId":"w1"}"#.as_slice(),
            "workspace:w1",
        ),
        ("/api/tab", br#"{"workspaceId":"w1"}"#.as_slice(), "tab:w1"),
    ] {
        let response = request(
            &server,
            post(path, "application/json; charset=utf-8", body_value.to_vec()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body(response).await, br#"{"ok":true}"#);
        assert_eq!(
            actions
                .calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .last()
                .map(String::as_str),
            Some(expected_call)
        );
    }

    let invalid = [
        ("text/plain", br#"{"paneId":"p"}"#.to_vec()),
        ("application/json", b"{".to_vec()),
        ("application/json", br#"{}"#.to_vec()),
        ("application/json", br#"{"paneId":1}"#.to_vec()),
        ("application/json", br#"{"paneId":"p","extra":1}"#.to_vec()),
        ("application/json", br#"{"paneId":""}"#.to_vec()),
        ("application/json", br#"{"paneId":" p "}"#.to_vec()),
        ("application/json", br#"{"paneId":"p\u0000x"}"#.to_vec()),
        (
            "application/json",
            serde_json::to_vec(&json!({"paneId": "x".repeat(MAX_IDENTIFIER_BYTES + 1)}))
                .unwrap_or_default(),
        ),
    ];
    for (content_type, value) in invalid {
        let response = request(&server, post("/api/focus", content_type, value)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body(response).await,
            br#"{"ok":false,"error":"invalid_request"}"#
        );
    }

    for valid in [
        "w1:p1".to_owned(),
        "workspace-1".to_owned(),
        "550e8400-e29b-41d4-a716-446655440000".to_owned(),
        format!("a{}", "x".repeat(MAX_IDENTIFIER_BYTES - 1)),
    ] {
        let response = request(
            &server,
            post(
                "/api/focus",
                "application/json",
                serde_json::to_vec(&json!({"paneId": valid})).unwrap_or_default(),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    for invalid in [
        "-option".to_owned(),
        "--help".to_owned(),
        "w1/p1".to_owned(),
        "w1\\p1".to_owned(),
        "w1,p1".to_owned(),
        "w1@p1".to_owned(),
        "é".to_owned(),
        format!("a{}", "x".repeat(MAX_IDENTIFIER_BYTES)),
    ] {
        let response = request(
            &server,
            post(
                "/api/focus",
                "application/json",
                serde_json::to_vec(&json!({"paneId": invalid})).unwrap_or_default(),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    let mut exact = br#"{"paneId":"p"}"#.to_vec();
    exact.resize(BODY_LIMIT_BYTES, b' ');
    assert_eq!(
        request(&server, post("/api/focus", "application/json", exact))
            .await
            .status(),
        StatusCode::OK
    );
    let oversized = vec![b' '; BODY_LIMIT_BYTES + 1];
    assert_eq!(
        request(&server, post("/api/focus", "application/json", oversized))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );

    actions.fail.store(true, Ordering::Release);
    let unavailable = request(
        &server,
        post(
            "/api/focus",
            "application/json",
            br#"{"paneId":"p"}"#.to_vec(),
        ),
    )
    .await;
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body(unavailable).await,
        br#"{"ok":false,"error":"herdr_unavailable"}"#
    );
}

#[tokio::test]
async fn remote_auth_origin_and_preflight_policy() {
    let mut remote = options();
    remote.listen = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 8080));
    assert!(
        HttpServer::build(
            remote.clone(),
            Arc::new(StateHub::new(&json!({})).unwrap_or_else(|error| panic!("hub: {error}"))),
            Arc::new(MockActions::default()),
            Arc::new(StaticHealth(health_report())),
        )
        .is_err()
    );
    remote.auth_token = Some(TOKEN.to_owned());
    remote.allowed_origins = vec!["https://deck.example".to_owned()];
    let (server, _, _) = server_with(remote);

    assert_eq!(request(&server, get("/")).await.status(), StatusCode::OK);
    for path in ["/api/snapshot", "/api/health", "/events"] {
        let unauthorized = request(
            &server,
            Request::builder()
                .uri(path)
                .header(header::HOST, "deck.example")
                .body(Body::empty())
                .unwrap_or_else(|error| panic!("request: {error}")),
        )
        .await;
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(unauthorized.headers()[header::WWW_AUTHENTICATE], "Bearer");
    }
    for (path, body_value) in [
        ("/api/focus", br#"{"paneId":"p"}"#.as_slice()),
        ("/api/workspace", br#"{"workspaceId":"w1"}"#.as_slice()),
        ("/api/tab", br#"{"workspaceId":"w1"}"#.as_slice()),
    ] {
        let response = request(
            &server,
            Request::builder()
                .method(Method::POST)
                .uri(path)
                .header(header::HOST, "deck.example")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body_value))
                .unwrap_or_else(|error| panic!("request: {error}")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }

    let authorized = Request::builder()
        .uri("/api/snapshot")
        .header(header::HOST, "deck.example")
        .header(header::ORIGIN, "https://deck.example")
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap_or_else(|error| panic!("request: {error}"));
    let authorized = request(&server, authorized).await;
    assert_eq!(authorized.status(), StatusCode::OK);
    assert_eq!(
        authorized.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://deck.example"
    );
    for bad_token in ["short", "1123456789abcdef0123456789abcdef"] {
        let rejected = Request::builder()
            .uri("/api/snapshot")
            .header(header::HOST, "deck.example")
            .header(header::AUTHORIZATION, format!("Bearer {bad_token}"))
            .body(Body::empty())
            .unwrap_or_else(|error| panic!("request: {error}"));
        assert_eq!(
            request(&server, rejected).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    let rejected = Request::builder()
        .uri("/")
        .header(header::ORIGIN, "https://evil.example")
        .header(header::HOST, "deck.example")
        .body(Body::empty())
        .unwrap_or_else(|error| panic!("request: {error}"));
    assert_eq!(
        request(&server, rejected).await.status(),
        StatusCode::FORBIDDEN
    );

    let allowed = Request::builder()
        .uri("/")
        .header(header::ORIGIN, "https://deck.example")
        .header(header::HOST, "deck.example")
        .body(Body::empty())
        .unwrap_or_else(|error| panic!("request: {error}"));
    let allowed = request(&server, allowed).await;
    assert_eq!(allowed.status(), StatusCode::OK);
    assert_eq!(
        allowed.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://deck.example"
    );

    let preflight = Request::builder()
        .method(Method::OPTIONS)
        .uri("/api/focus")
        .header(header::ORIGIN, "https://deck.example")
        .header(header::HOST, "deck.example")
        .body(Body::empty())
        .unwrap_or_else(|error| panic!("request: {error}"));
    let preflight = request(&server, preflight).await;
    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        preflight.headers()[header::ACCESS_CONTROL_ALLOW_METHODS],
        "GET, POST, OPTIONS"
    );

    let rejected_preflight = Request::builder()
        .method(Method::OPTIONS)
        .uri("/api/focus")
        .header(header::ORIGIN, "https://evil.example")
        .header(header::HOST, "evil.example")
        .body(Body::empty())
        .unwrap_or_else(|error| panic!("request: {error}"));
    assert_eq!(
        request(&server, rejected_preflight).await.status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn known_authorities_defeat_dns_rebinding_and_allow_declared_proxy_origins() {
    let (server, _, _) = server_with(options());
    for (host, origin) in [
        ("evil.example:4242", Some("http://evil.example:4242")),
        ("evil.example:4242", None),
        ("localhost:4242", Some("http://evil.example:4242")),
    ] {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri("/api/focus")
            .header(header::HOST, host)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(origin) = origin {
            builder = builder.header(header::ORIGIN, origin);
        }
        let response = request(
            &server,
            builder
                .body(Body::from(br#"{"paneId":"w1:p1"}"#.as_slice()))
                .unwrap_or_else(|error| panic!("request: {error}")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_security_headers(&response);
    }

    for (host, origin) in [
        ("localhost:4242", "http://localhost:4242"),
        ("127.0.0.1:4242", "http://127.0.0.1:4242"),
    ] {
        let response = request(
            &server,
            Request::builder()
                .method(Method::POST)
                .uri("/api/focus")
                .header(header::HOST, host)
                .header(header::ORIGIN, origin)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(br#"{"paneId":"w1:p1"}"#.as_slice()))
                .unwrap_or_else(|error| panic!("request: {error}")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    let mut proxied = options();
    proxied.public_host = Some("deck.private.example".to_owned());
    let (proxy_server, _, _) = server_with(proxied);
    let proxy = Request::builder()
        .uri("/api/snapshot")
        .header(header::HOST, "deck.private.example")
        .header(header::ORIGIN, "https://deck.private.example")
        .body(Body::empty())
        .unwrap_or_else(|error| panic!("request: {error}"));
    let response = request(&proxy_server, proxy).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://deck.private.example"
    );

    let mut ipv6 = options();
    ipv6.listen = SocketAddr::from((Ipv6Addr::LOCALHOST, 4242));
    let (ipv6_server, _, _) = server_with(ipv6);
    let response = request(
        &ipv6_server,
        Request::builder()
            .uri("/api/snapshot")
            .header(header::HOST, "[::1]:4242")
            .header(header::ORIGIN, "http://[::1]:4242")
            .body(Body::empty())
            .unwrap_or_else(|error| panic!("request: {error}")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[test]
fn options_and_health_never_expose_free_form_secrets() {
    let mut configured = options();
    configured.auth_token = Some(TOKEN.to_owned());
    let debug = format!("{configured:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains(TOKEN));

    for unsafe_value in [
        "secret prompt text",
        "0123456789abcdef0123456789abcdef",
        "opaqueIdentifierToken",
        "vopaque.identifier",
        "/private/transcript/path",
        "v1",
    ] {
        assert!(
            SafeVersion::new(unsafe_value).is_none(),
            "accepted {unsafe_value}"
        );
        assert!(HealthStatus::from_code(unsafe_value).is_none());
        assert!(CapabilityName::from_code(unsafe_value).is_none());
        assert!(AdapterName::from_code(unsafe_value).is_none());
        assert!(HealthState::from_code(unsafe_value).is_none());
        assert!(HealthBackend::from_code(unsafe_value).is_none());
        assert!(HealthReason::from_code(unsafe_value).is_none());
    }
    assert_eq!(
        SafeVersion::new("1.2.3-rc1").map(|version| version.as_str().to_owned()),
        Some("1.2.3-rc1".to_owned())
    );
    assert!(SafeVersion::new("v1.2.3+build7").is_some());
    // All other health strings and map keys are closed enums, so secrets cannot
    // be represented in those fields in the first place.
    let bytes =
        serde_json::to_vec(&health_report()).unwrap_or_else(|error| panic!("health JSON: {error}"));
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains("secret"));
    assert!(!text.contains("session content"));
    assert!(!text.contains("private"));
    assert!(text.contains("connection_failed"));
}

#[test]
fn direct_http_options_cannot_bypass_build_time_safety() {
    let mutations: [fn(&mut HttpOptions); 6] = [
        |options: &mut HttpOptions| options.base_path = "/deck/../bad".to_owned(),
        |options: &mut HttpOptions| options.base_path = "/{deck}".to_owned(),
        |options: &mut HttpOptions| options.liveness_interval = Duration::ZERO,
        |options: &mut HttpOptions| options.max_sse_clients = 0,
        |options: &mut HttpOptions| options.max_sse_clients = MAX_SSE_CLIENTS + 1,
        |options: &mut HttpOptions| {
            options.allowed_origins = vec!["https://*.example".to_owned()];
        },
    ];
    for mutate in mutations {
        let mut configured = options();
        mutate(&mut configured);
        let build = std::panic::catch_unwind(|| {
            HttpServer::build(
                configured,
                Arc::new(StateHub::new(&json!({})).unwrap_or_else(|error| panic!("hub: {error}"))),
                Arc::new(MockActions::default()),
                Arc::new(StaticHealth(health_report())),
            )
        });
        assert!(build.is_ok(), "invalid options reached an Axum panic");
        assert!(build.is_ok_and(|result| result.is_err()));
    }

    let mut boundary = options();
    boundary.max_sse_clients = MAX_SSE_CLIENTS;
    assert!(server_with(boundary).0.active_sse_clients() == 0);

    for invalid_base in [
        "",
        "deck",
        "/deck/",
        "//deck",
        "/deck//v1",
        "/{deck}",
        "/:deck",
        "/*deck",
        "/deck?x",
        "/deck#x",
        "/deck%20",
        "/deck\\x",
        "/deck x",
        "/déck",
        "/-deck",
        "/_deck",
        "/.deck",
        "/deck\u{0}",
    ] {
        let result = std::panic::catch_unwind(|| {
            let mut configured = options();
            configured.base_path = invalid_base.to_owned();
            HttpServer::build(
                configured,
                Arc::new(StateHub::new(&json!({})).unwrap_or_else(|error| panic!("hub: {error}"))),
                Arc::new(MockActions::default()),
                Arc::new(StaticHealth(health_report())),
            )
        });
        assert!(result.is_ok(), "Axum panic for {invalid_base:?}");
        assert!(
            result.is_ok_and(|build| build.is_err()),
            "accepted {invalid_base:?}"
        );
    }

    let mut over_semaphore = options();
    over_semaphore.max_sse_clients = tokio::sync::Semaphore::MAX_PERMITS.saturating_add(1);
    assert!(
        HttpServer::build(
            over_semaphore,
            Arc::new(StateHub::new(&json!({})).unwrap_or_else(|error| panic!("hub: {error}"))),
            Arc::new(MockActions::default()),
            Arc::new(StaticHealth(health_report())),
        )
        .is_err()
    );
}

#[tokio::test]
async fn development_assets_are_confined_to_the_canonical_root() {
    let root = tempfile::Builder::new()
        .prefix(".agentdeck-http-")
        .tempdir_in(".")
        .unwrap_or_else(|error| panic!("temp dir: {error}"));
    std::fs::write(root.path().join("index.html"), "<h1>development</h1>")
        .unwrap_or_else(|error| panic!("write: {error}"));
    std::fs::write(root.path().join("style.css"), "body{}")
        .unwrap_or_else(|error| panic!("write: {error}"));
    let mut configured = options();
    configured.public_dir = Some(root.path().to_owned());
    configured.base_path = "/deck".to_owned();
    let (server, _, _) = server_with(configured);
    assert_eq!(
        body(request(&server, get("/")).await).await,
        b"<h1>development</h1>"
    );
    let css = request(&server, get("/style.css")).await;
    assert_eq!(css.status(), StatusCode::OK);
    assert_eq!(
        css.headers()[header::CONTENT_TYPE],
        "text/css; charset=utf-8"
    );
    assert_eq!(body(css).await, b"body{}");
    assert_eq!(
        request(&server, get("/deck/style.css")).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        request(&server, get("/deckevil/style.css")).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&server, get("/%2e%2e/secret")).await.status(),
        StatusCode::NOT_FOUND
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let outside =
            tempfile::NamedTempFile::new().unwrap_or_else(|error| panic!("file: {error}"));
        symlink(outside.path(), root.path().join("escape.txt"))
            .unwrap_or_else(|error| panic!("symlink: {error}"));
        assert_eq!(
            request(&server, get("/escape.txt")).await.status(),
            StatusCode::NOT_FOUND
        );

        std::fs::create_dir(root.path().join("real"))
            .unwrap_or_else(|error| panic!("directory: {error}"));
        std::fs::write(root.path().join("real/secret.txt"), "secret")
            .unwrap_or_else(|error| panic!("write: {error}"));
        symlink(root.path().join("real"), root.path().join("linked"))
            .unwrap_or_else(|error| panic!("symlink: {error}"));
        assert_eq!(
            request(&server, get("/linked/secret.txt")).await.status(),
            StatusCode::NOT_FOUND
        );

        std::fs::write(root.path().join("writable.txt"), "unsafe")
            .unwrap_or_else(|error| panic!("write: {error}"));
        std::fs::set_permissions(
            root.path().join("writable.txt"),
            std::fs::Permissions::from_mode(0o666),
        )
        .unwrap_or_else(|error| panic!("permissions: {error}"));
        assert_eq!(
            request(&server, get("/writable.txt")).await.status(),
            StatusCode::NOT_FOUND
        );

        let linked_root_parent = tempfile::Builder::new()
            .prefix(".agentdeck-linked-")
            .tempdir_in(".")
            .unwrap_or_else(|error| panic!("linked root parent: {error}"));
        let linked_root = linked_root_parent.path().join("public-link");
        symlink(root.path(), &linked_root).unwrap_or_else(|error| panic!("root symlink: {error}"));
        let mut linked_options = options();
        linked_options.public_dir = Some(linked_root);
        assert!(
            HttpServer::build(
                linked_options,
                Arc::new(StateHub::new(&json!({})).unwrap_or_else(|error| panic!("hub: {error}"))),
                Arc::new(MockActions::default()),
                Arc::new(StaticHealth(health_report())),
            )
            .is_err()
        );

        let nested_parent = tempfile::Builder::new()
            .prefix(".agentdeck-nested-")
            .tempdir_in(".")
            .unwrap_or_else(|error| panic!("nested parent: {error}"));
        std::fs::create_dir_all(nested_parent.path().join("real/served"))
            .unwrap_or_else(|error| panic!("nested root: {error}"));
        symlink(
            nested_parent.path().join("real"),
            nested_parent.path().join("linked-parent"),
        )
        .unwrap_or_else(|error| panic!("nested symlink: {error}"));
        let mut nested_options = options();
        nested_options.public_dir = Some(nested_parent.path().join("linked-parent/served"));
        assert!(
            HttpServer::build(
                nested_options,
                Arc::new(StateHub::new(&json!({})).unwrap_or_else(|error| panic!("hub: {error}")),),
                Arc::new(MockActions::default()),
                Arc::new(StaticHealth(health_report())),
            )
            .is_err()
        );
    }

    let mut remote_override = options();
    remote_override.listen = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 8080));
    remote_override.auth_token = Some(TOKEN.to_owned());
    remote_override.allowed_origins = vec!["https://deck.example".to_owned()];
    remote_override.public_dir = Some(root.path().to_owned());
    assert!(
        HttpServer::build(
            remote_override,
            Arc::new(StateHub::new(&json!({})).unwrap_or_else(|error| panic!("hub: {error}"))),
            Arc::new(MockActions::default()),
            Arc::new(StaticHealth(health_report())),
        )
        .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn sse_is_exact_latest_only_changed_and_liveness_republished() {
    let (server, states, _) = server_with(options());
    let response = request(&server, get("/events")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "no-cache, no-store"
    );
    let mut stream = response.into_body();
    assert_eq!(
        next_data(&mut stream).await,
        Some(b"data: {\"n\":1}\n\n".to_vec())
    );

    assert!(
        !states
            .publish(&json!({"n": 1}))
            .unwrap_or_else(|error| panic!("publish: {error}"))
    );
    assert!(
        states
            .publish(&json!({"n": 2}))
            .unwrap_or_else(|error| panic!("publish: {error}"))
    );
    assert!(
        states
            .publish(&json!({"n": 3}))
            .unwrap_or_else(|error| panic!("publish: {error}"))
    );
    assert_eq!(
        next_data(&mut stream).await,
        Some(b"data: {\"n\":3}\n\n".to_vec())
    );

    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(
        next_data(&mut stream).await,
        Some(b"data: {\"n\":3}\n\n".to_vec())
    );
}

#[tokio::test]
async fn sse_limit_disconnect_unpolled_drop_shutdown_and_reconnect_cleanup() {
    let mut configured = options();
    configured.max_sse_clients = 1;
    configured.liveness_interval = Duration::from_secs(60);
    let (server, _, _) = server_with(configured);

    let first = request(&server, get("/events")).await;
    assert_eq!(server.active_sse_clients(), 1);
    assert_eq!(
        request(&server, get("/events")).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    drop(first);
    assert_eq!(
        server.active_sse_clients(),
        0,
        "unpolled response must release its lease"
    );

    for _ in 0..1_000 {
        let response = request(&server, get("/events")).await;
        assert_eq!(server.active_sse_clients(), 1);
        let mut stream = response.into_body();
        assert!(next_data(&mut stream).await.is_some());
        drop(stream);
        assert_eq!(server.active_sse_clients(), 0);
    }

    let response = request(&server, get("/events")).await;
    let mut stream = response.into_body();
    assert!(next_data(&mut stream).await.is_some());
    server.shutdown();
    assert_eq!(next_data(&mut stream).await, None);
    drop(stream);
    assert_eq!(server.active_sse_clients(), 0);
    let after_shutdown = request(&server, get("/events")).await;
    assert_eq!(after_shutdown.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body(after_shutdown).await,
        br#"{"ok":false,"error":"shutting_down"}"#
    );
}

#[tokio::test]
async fn one_dropped_sse_client_does_not_affect_another() {
    let mut configured = options();
    configured.max_sse_clients = 2;
    configured.liveness_interval = Duration::from_secs(60);
    let (server, states, _) = server_with(configured);
    let mut broken = request(&server, get("/events")).await.into_body();
    let mut healthy = request(&server, get("/events")).await.into_body();
    assert!(next_data(&mut broken).await.is_some());
    assert!(next_data(&mut healthy).await.is_some());
    drop(broken);
    assert_eq!(server.active_sse_clients(), 1);
    assert!(
        states
            .publish(&json!({"n": 9}))
            .unwrap_or_else(|error| panic!("publish: {error}"))
    );
    assert_eq!(
        next_data(&mut healthy).await,
        Some(b"data: {\"n\":9}\n\n".to_vec())
    );
}

struct RacingState;

impl StatePort for RacingState {
    fn current(&self) -> Arc<[u8]> {
        Arc::from(br#"{"n":"stale"}"#.as_slice())
    }

    fn subscribe(&self) -> tokio::sync::watch::Receiver<Arc<[u8]>> {
        let (_sender, receiver) =
            tokio::sync::watch::channel(Arc::from(br#"{"n":"latest"}"#.as_slice()));
        receiver
    }
}

#[tokio::test]
async fn sse_initial_frame_is_derived_from_subscription_without_current_subscribe_race() {
    let server = HttpServer::build(
        options(),
        Arc::new(RacingState),
        Arc::new(MockActions::default()),
        Arc::new(StaticHealth(health_report())),
    )
    .unwrap_or_else(|error| panic!("server: {error}"));
    let mut stream = request(&server, get("/events")).await.into_body();
    assert_eq!(
        next_data(&mut stream).await,
        Some(b"data: {\"n\":\"latest\"}\n\n".to_vec())
    );
}

struct InvalidState;

impl StatePort for InvalidState {
    fn current(&self) -> Arc<[u8]> {
        Arc::from(
            br#"{"ok":true}
data: injected"#
                .as_slice(),
        )
    }

    fn subscribe(&self) -> tokio::sync::watch::Receiver<Arc<[u8]>> {
        let (_sender, receiver) = tokio::sync::watch::channel(self.current());
        receiver
    }
}

struct ClosedPublisher;

impl StatePort for ClosedPublisher {
    fn current(&self) -> Arc<[u8]> {
        Arc::from(br#"{"n":1}"#.as_slice())
    }

    fn subscribe(&self) -> tokio::sync::watch::Receiver<Arc<[u8]>> {
        let (sender, receiver) = tokio::sync::watch::channel(self.current());
        drop(sender);
        receiver
    }
}

#[tokio::test]
async fn publisher_closure_ends_only_its_sse_stream_after_the_immediate_frame() {
    let server = HttpServer::build(
        options(),
        Arc::new(ClosedPublisher),
        Arc::new(MockActions::default()),
        Arc::new(StaticHealth(health_report())),
    )
    .unwrap_or_else(|error| panic!("server: {error}"));
    let mut stream = request(&server, get("/events")).await.into_body();
    assert_eq!(
        next_data(&mut stream).await,
        Some(b"data: {\"n\":1}\n\n".to_vec())
    );
    assert_eq!(next_data(&mut stream).await, None);
}

#[tokio::test]
async fn arbitrary_state_ports_cannot_inject_sse_frames() {
    let server = HttpServer::build(
        options(),
        Arc::new(InvalidState),
        Arc::new(MockActions::default()),
        Arc::new(StaticHealth(health_report())),
    )
    .unwrap_or_else(|error| panic!("server: {error}"));
    for path in ["/api/snapshot", "/events"] {
        let response = request(&server, get(path)).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body(response).await,
            br#"{"ok":false,"error":"invalid_state"}"#
        );
    }
}

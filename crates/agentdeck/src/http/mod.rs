//! Testable HTTP/SSE boundary for the AgentDeck browser.
//!
//! This module deliberately builds a `Router` without binding a socket. The
//! runtime state owner can inject its watch-backed state and Herdr actions when
//! the foreground server is wired in a later phase.

mod assets;
mod health;
mod security;
mod state;

use std::{
    convert::Infallible,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::State,
    http::{HeaderMap, Method, Request, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use hyper::server::conn::http1;
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use serde::Deserialize;
use tokio::{net::TcpListener, sync::Semaphore, task::JoinSet};
use tokio_util::sync::CancellationToken;

pub use health::{
    AdapterHealth, AdapterName, CapabilityHealth, CapabilityName, HealthBackend, HealthPort,
    HealthReason, HealthReport, HealthState, HealthStatus, SafeVersion, StaticHealth,
};
pub use state::{StateHub, StatePort, StatePublishError};

use crate::config::Config;
use assets::AssetSource;
use security::{SecurityPolicy, security_middleware};

pub const BODY_LIMIT_BYTES: usize = 16 * 1024;
/// Maximum HTTP/1 request-line-plus-header bytes admitted by Hyper.
///
/// Hyper's parser enforces this before constructing an Axum request, preventing
/// oversized headers from reaching router middleware or application state.
pub const MAX_REQUEST_HEAD_BYTES: usize = 16 * 1024;
/// Maximum number of HTTP/1 header fields admitted before request construction.
pub const MAX_REQUEST_HEADER_FIELDS: usize = 64;
/// Maximum time allowed to finish an HTTP/1 request head.
pub const REQUEST_HEAD_READ_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_IDENTIFIER_BYTES: usize = 256;
pub const DEFAULT_MAX_SSE_CLIENTS: usize = 64;
pub const MAX_SSE_CLIENTS: usize = 1024;
pub const DEFAULT_LIVENESS_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum HttpBuildError {
    #[error("invalid HTTP configuration: {0}")]
    InvalidConfig(#[from] anyhow::Error),
    #[error("server.listen is not a socket address: {0}")]
    InvalidListen(#[from] std::net::AddrParseError),
    #[error("development public directory is invalid: {0}")]
    InvalidPublicDirectory(String),
    #[error("invalid HTTP option: {0}")]
    InvalidOption(String),
    #[error("max SSE clients must be greater than zero")]
    ZeroClientLimit,
}

#[derive(Clone)]
pub struct HttpOptions {
    pub listen: SocketAddr,
    pub base_path: String,
    pub public_dir: Option<PathBuf>,
    pub public_host: Option<String>,
    pub allowed_origins: Vec<String>,
    pub auth_token: Option<String>,
    pub max_sse_clients: usize,
    pub liveness_interval: Duration,
}

impl std::fmt::Debug for HttpOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpOptions")
            .field("listen", &self.listen)
            .field("base_path", &self.base_path)
            .field("public_dir", &self.public_dir)
            .field("public_host", &self.public_host)
            .field("allowed_origins", &self.allowed_origins)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "<redacted>"),
            )
            .field("max_sse_clients", &self.max_sse_clients)
            .field("liveness_interval", &self.liveness_interval)
            .finish()
    }
}

impl HttpOptions {
    pub fn from_config(config: &Config) -> Result<Self, HttpBuildError> {
        config.validate()?;
        Ok(Self {
            listen: config.server.listen.parse()?,
            base_path: config.server.base_path.clone(),
            public_dir: config.server.public_dir.as_ref().map(PathBuf::from),
            public_host: config.server.public_host.clone(),
            allowed_origins: config.security.allowed_origins.clone(),
            auth_token: config.security.auth_token.clone(),
            max_sse_clients: DEFAULT_MAX_SSE_CLIENTS,
            liveness_interval: DEFAULT_LIVENESS_INTERVAL,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionError {
    HerdrUnavailable,
}

#[async_trait]
pub trait HerdrActions: Send + Sync + 'static {
    async fn focus_pane(&self, pane_id: &str) -> Result<(), ActionError>;
    async fn focus_workspace(&self, workspace_id: &str) -> Result<(), ActionError>;
    async fn create_tab(&self, workspace_id: &str) -> Result<(), ActionError>;
}

#[derive(Clone)]
struct AppState {
    states: Arc<dyn StatePort>,
    actions: Arc<dyn HerdrActions>,
    health: Arc<dyn HealthPort>,
    assets: AssetSource,
    clients: Arc<Semaphore>,
    active_clients: Arc<AtomicUsize>,
    liveness_interval: Duration,
    shutdown: CancellationToken,
    base_path: String,
}

#[derive(Clone)]
pub struct HttpServer {
    router: Router,
    shutdown: CancellationToken,
    active_clients: Arc<AtomicUsize>,
}

impl HttpServer {
    pub fn build(
        options: HttpOptions,
        states: Arc<dyn StatePort>,
        actions: Arc<dyn HerdrActions>,
        health: Arc<dyn HealthPort>,
    ) -> Result<Self, HttpBuildError> {
        validate_options(&options)?;
        let assets = AssetSource::new(
            options.public_dir.as_deref(),
            options.listen.ip().is_loopback(),
        )?;
        let security = SecurityPolicy::new(
            options.listen,
            options.allowed_origins,
            options.public_host,
            options.auth_token,
        );
        let shutdown = CancellationToken::new();
        let active_clients = Arc::new(AtomicUsize::new(0));
        let state = AppState {
            states,
            actions,
            health,
            assets,
            clients: Arc::new(Semaphore::new(options.max_sse_clients)),
            active_clients: Arc::clone(&active_clients),
            liveness_interval: options.liveness_interval,
            shutdown: shutdown.clone(),
            base_path: options.base_path.clone(),
        };

        let surface = surface_router();
        let router = if options.base_path == "/" {
            surface
        } else {
            let trailing_base = format!("{}/", options.base_path);
            surface
                .clone()
                .merge(Router::new().nest(&options.base_path, surface))
                .route(&trailing_base, get(index))
        }
        .layer(middleware::from_fn_with_state(
            security,
            security_middleware,
        ))
        .with_state(state);

        Ok(Self {
            router,
            shutdown,
            active_clients,
        })
    }

    pub fn router(&self) -> Router {
        self.router.clone()
    }

    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    pub fn active_sse_clients(&self) -> usize {
        self.active_clients.load(Ordering::Acquire)
    }
}

/// Serve the production HTTP/1 boundary with parser-level request-head limits.
///
/// `axum::serve` intentionally exposes no Hyper builder controls. Owning the
/// accept loop keeps the request-line/header byte budget and field-count budget
/// at the parser boundary while preserving per-connection graceful shutdown.
pub async fn serve_http(
    listener: TcpListener,
    router: Router,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            completed = connections.join_next(), if !connections.is_empty() => {
                let _connection_task_result = completed;
            }
            accepted = listener.accept() => {
                let (stream, _peer) = accepted?;
                let router = router.clone();
                let connection_shutdown = shutdown.clone();
                connections.spawn(async move {
                    let mut builder = http1::Builder::new();
                    builder
                        .max_headers(MAX_REQUEST_HEADER_FIELDS)
                        .max_buf_size(MAX_REQUEST_HEAD_BYTES)
                        .timer(TokioTimer::new())
                        .header_read_timeout(REQUEST_HEAD_READ_TIMEOUT);
                    let connection = builder.serve_connection(
                        TokioIo::new(stream),
                        TowerToHyperService::new(router),
                    );
                    tokio::pin!(connection);
                    tokio::select! {
                        result = &mut connection => {
                            let _connection_result = result;
                        }
                        () = connection_shutdown.cancelled() => {
                            connection.as_mut().graceful_shutdown();
                            let _connection_result = connection.await;
                        }
                    }
                });
            }
        }
    }

    while let Some(result) = connections.join_next().await {
        let _connection_task_result = result;
    }
    Ok(())
}

fn validate_options(options: &HttpOptions) -> Result<(), HttpBuildError> {
    let base_ok = options.base_path == "/"
        || options.base_path.strip_prefix('/').is_some_and(|relative| {
            !relative.is_empty()
                && relative.split('/').all(|segment| {
                    segment
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_alphanumeric)
                        && segment.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                        })
                        && !matches!(segment, "." | "..")
                })
        });
    if !base_ok {
        return Err(HttpBuildError::InvalidOption(
            "base path is not a literal slash-separated ASCII path".to_owned(),
        ));
    }
    if options.liveness_interval.is_zero() {
        return Err(HttpBuildError::InvalidOption(
            "liveness interval must be greater than zero".to_owned(),
        ));
    }
    if options.max_sse_clients == 0 {
        return Err(HttpBuildError::ZeroClientLimit);
    }
    if options.max_sse_clients > MAX_SSE_CLIENTS
        || options.max_sse_clients > tokio::sync::Semaphore::MAX_PERMITS
    {
        return Err(HttpBuildError::InvalidOption(format!(
            "max SSE clients cannot exceed {MAX_SSE_CLIENTS}"
        )));
    }
    if !options.listen.ip().is_loopback() {
        let valid = options.auth_token.as_deref().is_some_and(|token| {
            token.len() >= crate::config::MIN_REMOTE_AUTH_TOKEN_BYTES
                && token.trim() == token
                && !token.chars().any(char::is_control)
        });
        if !valid {
            return Err(HttpBuildError::InvalidOption(format!(
                "non-loopback HTTP requires a token of at least {} bytes",
                crate::config::MIN_REMOTE_AUTH_TOKEN_BYTES
            )));
        }
    }
    for origin in &options.allowed_origins {
        let valid = url::Url::parse(origin).ok().is_some_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none()
                && url.host().is_some()
                && !origin.contains('*')
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none()
                && url.origin().ascii_serialization() == *origin
        });
        if !valid {
            return Err(HttpBuildError::InvalidOption(
                "allowed origins must be exact canonical HTTP(S) origins".to_owned(),
            ));
        }
    }
    if let Some(host) = &options.public_host {
        let valid = (1..=253).contains(&host.len())
            && host.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
            })
            && host.split('.').all(|label| {
                (1..=63).contains(&label.len())
                    && label
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_alphanumeric)
                    && label
                        .as_bytes()
                        .last()
                        .is_some_and(u8::is_ascii_alphanumeric)
            });
        if !valid {
            return Err(HttpBuildError::InvalidOption(
                "public host must be a canonical lowercase DNS hostname".to_owned(),
            ));
        }
    }
    Ok(())
}

fn surface_router() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/docs/setup.html", get(setup))
        .route("/api/snapshot", get(snapshot).options(preflight))
        .route("/api/health", get(health).options(preflight))
        .route("/events", get(events).options(preflight))
        .route("/api/focus", post(focus).options(preflight))
        .route("/api/workspace", post(workspace).options(preflight))
        .route("/api/tab", post(tab).options(preflight))
        .fallback(not_found)
}

async fn index(State(state): State<AppState>) -> Response {
    state.assets.index().await.into_response()
}

async fn setup(State(state): State<AppState>) -> Response {
    state.assets.setup().await.into_response()
}

async fn snapshot(State(state): State<AppState>) -> Response {
    let bytes = state.states.current();
    if !valid_state_bytes(&bytes) {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, "invalid_state");
    }
    json_bytes(StatusCode::OK, bytes)
}

async fn health(State(state): State<AppState>) -> Response {
    match serde_json::to_vec(&state.health.report()) {
        Ok(bytes) => json_bytes(StatusCode::OK, Arc::<[u8]>::from(bytes)),
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
    }
}

async fn preflight() -> StatusCode {
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PaneRequest {
    #[serde(rename = "paneId")]
    pane_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceRequest {
    #[serde(rename = "workspaceId")]
    workspace_id: String,
}

async fn focus(State(state): State<AppState>, request: Request<Body>) -> Response {
    let Some(input) = parse_json::<PaneRequest>(request).await else {
        return api_error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    if !valid_identifier(&input.pane_id) {
        return api_error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    action_response(state.actions.focus_pane(&input.pane_id).await)
}

async fn workspace(State(state): State<AppState>, request: Request<Body>) -> Response {
    let Some(input) = parse_json::<WorkspaceRequest>(request).await else {
        return api_error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    if !valid_identifier(&input.workspace_id) {
        return api_error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    action_response(state.actions.focus_workspace(&input.workspace_id).await)
}

async fn tab(State(state): State<AppState>, request: Request<Body>) -> Response {
    let Some(input) = parse_json::<WorkspaceRequest>(request).await else {
        return api_error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    if !valid_identifier(&input.workspace_id) {
        return api_error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    action_response(state.actions.create_tab(&input.workspace_id).await)
}

async fn parse_json<T: for<'de> Deserialize<'de>>(request: Request<Body>) -> Option<T> {
    if !is_json_content_type(request.headers()) {
        return None;
    }
    let bytes = to_bytes(request.into_body(), BODY_LIMIT_BYTES).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
}

fn valid_identifier(value: &str) -> bool {
    // Herdr's verified focus/create argv currently has no `--` delimiter before
    // IDs. Keep its observed opaque forms (`w1:p1`, UUIDs, dotted/dashed IDs)
    // while rejecting anything option-looking or path/punctuation-bearing.
    (1..=MAX_IDENTIFIER_BYTES).contains(&value.len())
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

fn action_response(result: Result<(), ActionError>) -> Response {
    match result {
        Ok(()) => json_literal(StatusCode::OK, br#"{"ok":true}"#),
        Err(ActionError::HerdrUnavailable) => {
            api_error(StatusCode::SERVICE_UNAVAILABLE, "herdr_unavailable")
        }
    }
}

async fn events(State(state): State<AppState>) -> Response {
    if state.shutdown.is_cancelled() {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "shutting_down");
    }
    // Subscribe first, then derive the immediate frame from that receiver. A
    // separate `current()` read before subscribe can miss a publish in between.
    let mut receiver = state.states.subscribe();
    let initial = receiver.borrow_and_update().clone();
    if !valid_state_bytes(&initial) {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, "invalid_state");
    }
    let permit = match Arc::clone(&state.clients).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return api_error(StatusCode::SERVICE_UNAVAILABLE, "client_limit"),
    };
    struct ClientLease {
        _permit: tokio::sync::OwnedSemaphorePermit,
        active: Arc<AtomicUsize>,
    }
    impl ClientLease {
        fn new(permit: tokio::sync::OwnedSemaphorePermit, active: Arc<AtomicUsize>) -> Self {
            active.fetch_add(1, Ordering::AcqRel);
            Self {
                _permit: permit,
                active,
            }
        }
    }
    impl Drop for ClientLease {
        fn drop(&mut self) {
            self.active.fetch_sub(1, Ordering::AcqRel);
        }
    }
    let lease = ClientLease::new(permit, Arc::clone(&state.active_clients));
    let shutdown = state.shutdown.clone();
    let interval = state.liveness_interval;

    let stream = async_stream::stream! {
        let _lease = lease;
        let mut current = initial;
        yield Ok::<Bytes, Infallible>(frame(&current));
        let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                changed = receiver.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let next = receiver.borrow_and_update().clone();
                    if !valid_state_bytes(&next) {
                        break;
                    }
                    if next != current {
                        current = next;
                        yield Ok(frame(&current));
                    }
                }
                _ = heartbeat.tick() => {
                    yield Ok(frame(&current));
                }
            }
        }
    };

    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache, no-store"),
    );
    headers.insert(
        header::CONNECTION,
        header::HeaderValue::from_static("keep-alive"),
    );
    response
}

fn valid_state_bytes(bytes: &[u8]) -> bool {
    !bytes.contains(&b'\n')
        && !bytes.contains(&b'\r')
        && matches!(
            serde_json::from_slice::<serde_json::Value>(bytes),
            Ok(serde_json::Value::Object(_))
        )
}

fn frame(payload: &[u8]) -> Bytes {
    let mut frame = Vec::with_capacity(payload.len() + 8);
    frame.extend_from_slice(b"data: ");
    frame.extend_from_slice(payload);
    frame.extend_from_slice(b"\n\n");
    Bytes::from(frame)
}

fn json_bytes(status: StatusCode, bytes: Arc<[u8]>) -> Response {
    let mut response = (status, bytes.to_vec()).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    response
}

fn json_literal(status: StatusCode, body: &'static [u8]) -> Response {
    json_bytes(status, Arc::from(body))
}

fn api_error(status: StatusCode, code: &'static str) -> Response {
    let body = format!(r#"{{"ok":false,"error":"{code}"}}"#);
    json_bytes(status, Arc::from(body.into_bytes()))
}

async fn not_found(
    State(state): State<AppState>,
    method: Method,
    request: Request<Body>,
) -> Response {
    if method == Method::GET {
        let mut path = request.uri().path();
        if state.base_path != "/" {
            let mounted_prefix = format!("{}/", state.base_path);
            if let Some(relative) = path.strip_prefix(&mounted_prefix) {
                path = relative;
            }
        }
        let response = state.assets.file(path.trim_start_matches('/')).await;
        if response.status() != StatusCode::NOT_FOUND {
            return response;
        }
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

#[cfg(test)]
mod tests;

use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
};

use axum::{
    body::Body,
    extract::State,
    http::{HeaderName, HeaderValue, Method, Request, StatusCode, header},
    middleware::Next,
    response::Response,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::api_error;

#[derive(Clone)]
pub(super) struct SecurityPolicy {
    remote: bool,
    /// Exact canonical origin -> exact canonical HTTP authority.
    known_origins: BTreeMap<String, String>,
    auth_digest: Option<[u8; 32]>,
}

impl SecurityPolicy {
    pub(super) fn new(
        listen: SocketAddr,
        allowed_origins: Vec<String>,
        public_host: Option<String>,
        auth_token: Option<String>,
    ) -> Self {
        let mut known_origins = BTreeMap::new();
        if listen.ip().is_loopback() {
            add_origin(
                &mut known_origins,
                &loopback_origin(listen.ip(), listen.port()),
            );
            add_origin(&mut known_origins, &http_origin("localhost", listen.port()));
        }
        for origin in allowed_origins {
            add_origin(&mut known_origins, &origin);
        }
        if let Some(host) = public_host {
            add_origin(&mut known_origins, &format!("https://{host}"));
        }
        Self {
            remote: !listen.ip().is_loopback(),
            known_origins,
            auth_digest: auth_token.map(|token| Sha256::digest(token.as_bytes()).into()),
        }
    }
}

pub(super) async fn security_middleware(
    State(policy): State<SecurityPolicy>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let method = request.method().as_str().to_owned();
    let protected = protected_path(request.uri().path());
    if protected && !known_request_authority(&policy, &request) {
        return secured_error(StatusCode::FORBIDDEN, "origin_rejected", None, &method);
    }
    let cors_origin = match accepted_origin(&policy, &request) {
        Ok(origin) => origin,
        Err(()) => {
            return secured_error(StatusCode::FORBIDDEN, "origin_rejected", None, &method);
        }
    };
    if policy.remote
        && request.method() != Method::OPTIONS
        && protected
        && !authorized(&policy, &request)
    {
        let mut response = secured_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            cors_origin.as_deref(),
            &method,
        );
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return response;
    }

    let mut response = next.run(request).await;
    apply_security_headers(&mut response, cors_origin.as_deref(), &method);
    response
}

fn secured_error(
    status: StatusCode,
    code: &'static str,
    origin: Option<&str>,
    method: &str,
) -> Response {
    let mut response = api_error(status, code);
    apply_security_headers(&mut response, origin, method);
    response
}

fn protected_path(path: &str) -> bool {
    path == "/events"
        || path.ends_with("/events")
        || path.starts_with("/api/")
        || path.contains("/api/")
}

fn authorized(policy: &SecurityPolicy, request: &Request<Body>) -> bool {
    let Some(expected) = policy.auth_digest else {
        return false;
    };
    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    let provided_digest: [u8; 32] = Sha256::digest(provided.as_bytes()).into();
    bool::from(provided_digest.ct_eq(&expected))
}

fn accepted_origin(policy: &SecurityPolicy, request: &Request<Body>) -> Result<Option<String>, ()> {
    let Some(origin) = request.headers().get(header::ORIGIN) else {
        return Ok(None);
    };
    let origin = origin.to_str().map_err(|_| ())?;
    let expected_authority = policy.known_origins.get(origin).ok_or(())?;
    let request_authority = request_authority(request).ok_or(())?;
    if request_authority.eq_ignore_ascii_case(expected_authority) {
        Ok(Some(origin.to_owned()))
    } else {
        Err(())
    }
}

fn known_request_authority(policy: &SecurityPolicy, request: &Request<Body>) -> bool {
    let Some(authority) = request_authority(request) else {
        return false;
    };
    policy
        .known_origins
        .values()
        .any(|known| authority.eq_ignore_ascii_case(known))
}

fn request_authority(request: &Request<Body>) -> Option<&str> {
    request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
}

fn add_origin(origins: &mut BTreeMap<String, String>, origin: &str) {
    if let Ok(url) = url::Url::parse(origin) {
        let authority = url[url::Position::BeforeHost..url::Position::AfterPort].to_owned();
        origins.insert(url.origin().ascii_serialization(), authority);
    }
}

fn loopback_origin(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(ip) => http_origin(&ip.to_string(), port),
        IpAddr::V6(ip) => http_origin(&format!("[{ip}]"), port),
    }
}

fn http_origin(host: &str, port: u16) -> String {
    if port == 80 {
        format!("http://{host}")
    } else {
        format!("http://{host}:{port}")
    }
}

fn apply_security_headers(response: &mut Response, origin: Option<&str>, method: &str) {
    let headers = response.headers_mut();
    headers
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; connect-src 'self'; img-src 'self' data:; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    if let Some(origin) = origin.and_then(|value| HeaderValue::from_str(value).ok()) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        headers.insert(header::VARY, HeaderValue::from_static("Origin"));
        if method == "OPTIONS" {
            headers.insert(
                header::ACCESS_CONTROL_ALLOW_METHODS,
                HeaderValue::from_static("GET, POST, OPTIONS"),
            );
            headers.insert(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static("Authorization, Content-Type"),
            );
        }
    }
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
}

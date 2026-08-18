use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

use crate::error::AppError;
use crate::server::Server;
use crate::state::DaemonState;

/// Attach an X-Request-Id to every response (covers axios-style debugging
/// and matches wings' request id behavior).
pub async fn request_id(req: Request<Body>, next: Next) -> Response {
    let id = Uuid::new_v4().to_string();
    let mut resp = next.run(req).await;
    resp.headers_mut()
        .insert("X-Request-Id", HeaderValue::from_str(&id).unwrap_or_else(|_| HeaderValue::from_static("")));
    resp
}

/// CORS handling like wings:
/// - allowed when Origin == panel location, or appears in allowed_origins
///   ("*" is allowed), or is a private-network origin and
///   `allow_cors_private_network` is set
/// - preflight OPTIONS returns 204
pub async fn cors(
    State(state): State<DaemonState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let config = state.config.read().await.clone();
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let allowed = origin.as_ref().map(|o| {
        let panel = config.panel_url();
        o == &panel
            || config.allowed_origins.iter().any(|a| a == "*" || a == o)
            || (config.allow_cors_private_network && is_private_origin(o))
    });

    let is_preflight = req.method() == Method::OPTIONS;

    if allowed == Some(true) {
        if is_preflight {
            let mut resp = Response::new(Body::empty());
            *resp.status_mut() = StatusCode::NO_CONTENT;
            let headers = resp.headers_mut();
            headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, header_value_or_wildcard(origin.as_deref()));
            headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET,POST,PUT,DELETE,PATCH,OPTIONS"));
            headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("Content-Type,Authorization,X-Requested-With"));
            headers.insert(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, HeaderValue::from_static("true"));
            return Ok(resp);
        }

        let mut resp = next.run(req).await;
        resp.headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, header_value_or_wildcard(origin.as_deref()));
        Ok(resp)
    } else {
        let mut resp = next.run(req).await;
        resp.headers_mut()
            .insert(header::VARY, HeaderValue::from_static("Origin"));
        Ok(resp)
    }
}

fn header_value_or_wildcard(origin: Option<&str>) -> HeaderValue {
    match origin {
        Some(o) => HeaderValue::from_str(o).unwrap_or_else(|_| HeaderValue::from_static("*")),
        None => HeaderValue::from_static("*"),
    }
}

/// Check whether the request origin is from a private network (RFC1918,
/// loopback, link-local, CGNAT, ULA, IPv4-mapped). Off by default.
pub(crate) fn is_private_origin(origin: &str) -> bool {
    let host = match origin.split("://").nth(1) {
        Some(h) => h,
        None => return false,
    };
    let host = host.split('/').next().unwrap_or(host);
    // Strip a port: "[::1]:2022" and "127.0.0.1:8080" -> host only.
    let host = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest).to_string()
    } else if host.matches(':').count() == 1 {
        host.split(':').next().unwrap_or(host).to_string()
    } else {
        host.to_string()
    };
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(v6)) => {
            // ::ffff:a.b.c.d is an IPv4-mapped address — evaluate as IPv4.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_ipv4(&v4);
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        }
        Ok(std::net::IpAddr::V4(v4)) => is_private_ipv4(&v4),
        Err(_) => false,
    }
}

fn is_private_ipv4(v4: &std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || (o[0] == 100 && o[1] >> 6 == 1) // 100.64.0.0/10 CGNAT
        || (o[0] == 198 && (18..=19).contains(&o[1])) // 198.18/15
}

/// Resolve the `:server` path param and inject the server into the
/// request extensions. Works for every route under /api/servers/{uuid}.
/// Routes without a uuid (e.g. the list/create endpoints) pass through.
pub async fn server_exists(
    State(state): State<DaemonState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, AppError> {
    let segments: Vec<&str> = req.uri().path().split('/').filter(|s| !s.is_empty()).collect();
    let server_pos = segments.iter().position(|s| *s == "servers");

    if let Some(pos) = server_pos {
        if let Some(uuid_str) = segments.get(pos + 1) {
            tracing::debug!(path = %req.uri(), uuid = %uuid_str, "server_exists middleware");
            if let Ok(uuid) = Uuid::parse_str(uuid_str) {
                let server = state.manager.get(uuid).await?;
                let mut req = req;
                req.extensions_mut().insert(server);
                return Ok(next.run(req).await);
            }
        }
    }

    Ok(next.run(req).await)
}

/// Extract the injected server from request extensions.
#[allow(dead_code)]
pub fn server_from(req: &Request<Body>) -> Option<Arc<Server>> {
    req.extensions().get::<Arc<Server>>().cloned()
}

/// Handler extractor for servers injected by the `server_exists`
/// middleware.
#[derive(Clone)]
pub struct ServerExtractor(pub Arc<Server>);

impl std::ops::Deref for ServerExtractor {
    type Target = Server;
    fn deref(&self) -> &Server {
        &self.0
    }
}

#[axum::async_trait]
impl<S> axum::extract::FromRequestParts<S> for ServerExtractor
where
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Arc<Server>>()
            .cloned()
            .map(ServerExtractor)
            .ok_or(AppError::ServerNotFound)
    }
}

/// Rate limit helper used by the websocket handler.
pub struct RateLimiter {
    window_start: std::time::Instant,
    count: u32,
    limit: u32,
    window: Duration,
}

impl RateLimiter {
    pub fn new(limit: u32, window: Duration) -> Self {
        Self {
            window_start: std::time::Instant::now(),
            count: 0,
            limit,
            window,
        }
    }

    /// Returns false when the message should be dropped.
    pub fn allow(&mut self) -> bool {
        if self.window_start.elapsed() > self.window {
            self.window_start = std::time::Instant::now();
            self.count = 0;
        }
        self.count += 1;
        self.count <= self.limit
    }
}

#[allow(dead_code)]
pub type HeaderMapAlias = HeaderMap;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use subtle::ConstantTimeEq;

use crate::state::DaemonState;

/// Panel -> daemon requests carry `Authorization: Bearer <daemon-token>`.
/// Wings compares it with the token from config.yml using a constant-time
/// compare. Missing/malformed header -> 401, wrong token -> 403.
pub async fn require_authorization(
    State(state): State<DaemonState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let token = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok());

    let provided = match token.and_then(|h| h.strip_prefix("Bearer ")) {
        Some(t) => t.as_bytes(),
        None => return Err(StatusCode::UNAUTHORIZED),
    };

    let expected = state.config.read().await.token.clone().into_bytes();

    tracing::debug!(path = %req.uri(), got = %token.map(|s| s.len()).unwrap_or(0), want = expected.len(), "auth check");
    if provided.ct_eq(&expected).unwrap_u8() == 1 {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}
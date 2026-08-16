use std::sync::Arc;
use tokio::sync::RwLock;
use axum::body::Body;

use crate::config::Config;
use crate::jwt::TokenStore;
use crate::remote::PanelClient;
use crate::server::activity::ActivityCollector;
use crate::server::ServerManager;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

/// Shared configuration, also behind a lock so `POST /api/update`
/// (panel pushes a new config.yml) can rotate the token at runtime.
pub type SharedConfig = Arc<RwLock<Config>>;

/// Everything handlers need, shared via axum State (derives Clone, which
/// only clones cheap handles).
#[derive(Clone)]
pub struct DaemonState {
    pub config: SharedConfig,
    pub manager: Arc<ServerManager>,
    pub panel: Arc<RwLock<PanelClient>>,
    pub tokens: Arc<TokenStore>,
    /// Seconds since epoch when the daemon booted. JWTs issued before
    /// this time are rejected.
    pub boot_time: i64,
    /// Buffered activity events flushed to the panel on an interval.
    pub activity: Arc<ActivityCollector>,
}

/// Middleware that makes the state available to other middleware via
/// request extensions (axum only injects State into handlers).
#[allow(dead_code)]
pub async fn inject_state(
    State(state): State<DaemonState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    req.extensions_mut().insert(state);
    next.run(req).await
}

#[allow(dead_code)]
pub fn state_from_request(req: &Request<Body>) -> Option<DaemonState> {
    req.extensions().get::<DaemonState>().cloned()
}


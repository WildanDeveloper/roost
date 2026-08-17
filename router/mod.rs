pub mod backups;
pub mod downloader;
pub mod downloads;
pub mod files;
pub mod middleware;
pub mod servers;
pub mod system;
pub mod ws;

use axum::middleware::from_fn_with_state;
use axum::Router;

use crate::auth::require_authorization;
use crate::state::DaemonState;

/// Assemble the full daemon router:
/// - public routes: websocket (JWT in-band), download/upload (JWT in URL)
/// - protected routes: everything else, behind `Authorization: Bearer`.
pub fn build(state: DaemonState) -> Router {
    let public = Router::new()
        .merge(ws::router())
        .merge(downloads::router())
        .with_state(state.clone());

    let protected = Router::new()
        .merge(system::router())
        .merge(servers::router())
        .merge(files::router())
        .merge(backups::router())
        // Panel-initiated cancel of an incoming transfer (wings keeps this
        // on the protected router without ServerExists; unknown server -> 404
        // handled by server_exists below).
        .route("/api/transfers/:server", axum::routing::delete(downloads::delete_incoming_transfer));

    let protected = protected
        .route_layer(from_fn_with_state(state.clone(), middleware::server_exists))
        .route_layer(from_fn_with_state(state.clone(), require_authorization))
        .with_state(state.clone());

    public
        .merge(protected)
        .layer(from_fn_with_state(state.clone(), middleware::cors))
        .layer(axum::middleware::from_fn(middleware::request_id))
}
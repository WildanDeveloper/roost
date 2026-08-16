use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;

use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::state::DaemonState;

pub fn router() -> Router<DaemonState> {
    Router::new()
        .route(
            "/api/servers/{server}",
            get(get_server).delete(delete_server),
        )
        .route("/api/servers/{server}/logs", get(get_server_logs))
        .route("/api/servers/{server}/power", post(post_server_power))
        .route("/api/servers/{server}/commands", post(post_server_commands))
        .route("/api/servers/{server}/install", post(post_server_install))
        .route("/api/servers/{server}/reinstall", post(post_server_reinstall))
        .route("/api/servers/{server}/sync", post(post_server_sync))
        .route("/api/servers/{server}/ws/deny", post(post_server_deny_ws_tokens))
        .route(
            "/api/servers/{server}/transfer",
            post(post_server_transfer).delete(delete_server_transfer),
        )
        .route("/api/deauthorize-user", post(post_deauthorize_user))
}

async fn get_server(server: crate::router::middleware::ServerExtractor) -> Json<serde_json::Value> {
    Json(serde_json::to_value(server.api_response()).unwrap_or_default())
}

async fn delete_server(
    State(state): State<DaemonState>,
    server: crate::router::middleware::ServerExtractor,
) -> AppResult<Response> {
    state.manager.delete(server.uuid).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize)]
struct LogsQuery {
    #[serde(default = "default_log_size")]
    size: u32,
}

fn default_log_size() -> u32 {
    100
}

/// GET /api/servers/:id/logs?size=N  ->  {"data": [lines]}
async fn get_server_logs(
    server: crate::router::middleware::ServerExtractor,
    Query(query): Query<LogsQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let size = query.size.clamp(1, 100);
    let server = server;

    let mut lines = Vec::new();
    let mut stream = server.docker.logs_tail(&server.uuid.to_string(), size);
    use futures_util::StreamExt;
    while let Some(item) = stream.next().await {
        match item {
            Ok(bollard::container::LogOutput::StdOut { message: bytes })
            | Ok(bollard::container::LogOutput::StdErr { message: bytes }) => {
                for line in String::from_utf8_lossy(&bytes).split('\n') {
                    lines.push(line.trim_end_matches('\r').to_string());
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    Ok(Json(json!({ "data": lines })))
}

#[derive(Debug, Deserialize)]
struct PowerRequest {
    action: String,
    #[serde(default)]
    wait_seconds: Option<u32>,
}

static POWER_ACTIONS: [&str; 4] = ["start", "stop", "restart", "kill"];

/// POST /api/servers/:id/power — async; returns 202 immediately.
async fn post_server_power(
    server: crate::router::middleware::ServerExtractor,
    Json(payload): Json<PowerRequest>,
) -> AppResult<Response> {
    if !POWER_ACTIONS.contains(&payload.action.as_str()) {
        return Err(AppError::Unprocessable(format!("invalid power action: {}", payload.action)));
    }
    if server.suspended.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(AppError::BadRequest("server is suspended".into()));
    }

    let wait = payload.wait_seconds.unwrap_or(30).min(300);
    let server = server.0.clone();

    tokio::spawn(async move {
        let result = match payload_action(&payload.action) {
            PowerOp::Start => server.power_start().await,
            PowerOp::Stop => server.power_stop(wait).await,
            PowerOp::Restart => server.power_restart(wait).await,
            PowerOp::Kill => server.power_kill().await,
        };
        if let Err(e) = result {
            tracing::warn!(uuid = %server.uuid, action = %payload.action, error = %e, "power action failed");
        }
    });

    Ok(StatusCode::ACCEPTED.into_response())
}

enum PowerOp {
    Start,
    Stop,
    Restart,
    Kill,
}

fn payload_action(action: &str) -> PowerOp {
    match action {
        "start" => PowerOp::Start,
        "restart" => PowerOp::Restart,
        "kill" => PowerOp::Kill,
        _ => PowerOp::Stop,
    }
}

#[derive(Debug, Deserialize)]
struct CommandsRequest {
    commands: Vec<String>,
}

/// POST /api/servers/:id/commands {"commands": ["say hi", "/stop"]}
async fn post_server_commands(
    server: crate::router::middleware::ServerExtractor,
    Json(payload): Json<CommandsRequest>,
) -> AppResult<Response> {
    for command in &payload.commands {
        server.send_command(command).await?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// POST /api/servers/:id/install — run the egg install script.
async fn post_server_install(
    server: crate::router::middleware::ServerExtractor,
) -> AppResult<Response> {
    if server.is_installing() {
        return Err(AppError::Conflict("server is already installing".into()));
    }
    let srv = server.0.clone();
    tokio::spawn(async move { srv.install(false).await });
    Ok(StatusCode::ACCEPTED.into_response())
}

/// POST /api/servers/:id/reinstall — 409 while a power action is running.
async fn post_server_reinstall(
    server: crate::router::middleware::ServerExtractor,
) -> AppResult<Response> {
    if server.is_installing() {
        return Err(AppError::Conflict("server is already installing".into()));
    }
    if server.is_power_locked() {
        return Err(AppError::Conflict("a power action is currently in progress".into()));
    }
    let srv = server.0.clone();
    tokio::spawn(async move { srv.install(true).await });
    Ok(StatusCode::ACCEPTED.into_response())
}

/// POST /api/servers/:id/sync — re-pull config from the panel and apply.
async fn post_server_sync(
    server: crate::router::middleware::ServerExtractor,
) -> AppResult<Response> {
    server.sync_from_panel().await?;
    if server.is_running() {
        // Resource changes apply in place when possible.
        let _ = server.apply_limits().await;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// POST /api/servers/:id/ws/deny {"jtis": [...]} — legacy token revocation.
#[derive(Debug, Deserialize)]
struct DenyWsTokensRequest {
    jtis: Vec<String>,
}

async fn post_server_deny_ws_tokens(
    State(state): State<DaemonState>,
    server: crate::router::middleware::ServerExtractor,
    Json(payload): Json<DenyWsTokensRequest>,
) -> AppResult<Response> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    for jti in &payload.jtis {
        state.tokens.deny_jti(jti, now).await;
        tracing::info!(uuid = %server.uuid, jti, "websocket token denied");
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// POST /api/deauthorize-user {"user": uuid, "servers": [...]}
#[derive(Debug, Deserialize)]
struct DeauthorizeUserRequest {
    user: Uuid,
    #[serde(default)]
    servers: Vec<Uuid>,
}

async fn post_deauthorize_user(
    State(state): State<DaemonState>,
    Json(payload): Json<DeauthorizeUserRequest>,
) -> AppResult<Response> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    state.tokens.deny_user(&payload.user.to_string(), now).await;
    tracing::info!(user = %payload.user, servers = ?payload.servers, "user deauthorized");
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn post_server_transfer() -> AppResult<Response> {
    Err(AppError::NotImplemented("server transfers are not implemented in wings-rs".into()))
}

async fn delete_server_transfer() -> AppResult<Response> {
    Err(AppError::NotImplemented("server transfers are not implemented in wings-rs".into()))
}
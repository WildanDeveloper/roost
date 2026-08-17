use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;

use serde::Deserialize;
use std::sync::Arc;
use serde_json::json;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::state::DaemonState;

pub fn router() -> Router<DaemonState> {
    Router::new()
        .route("/api/ping", get(|| async { "pong" }))
        .route("/api/servers/:server/ping2", get(|| async { "pong2" }))
        .route(
            "/api/servers/:server",
            get(get_server).delete(delete_server),
        )
        .route("/api/servers/:server/logs", get(get_server_logs))
        .route("/api/servers/:server/power", post(post_server_power))
        .route("/api/servers/:server/commands", post(post_server_commands))
        .route("/api/servers/:server/install", post(post_server_install))
        .route("/api/servers/:server/reinstall", post(post_server_reinstall))
        .route("/api/servers/:server/sync", post(post_server_sync))
        .route("/api/servers/:server/ws/deny", post(post_server_deny_ws_tokens))
        .route(
            "/api/servers/:server/transfer",
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
    while let Some(item) = futures_util::StreamExt::next(&mut stream).await {
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

/// POST /api/servers/:server/transfer {"url", "token", "server"} — the
/// panel asks this node to push the server's data to another node.
/// Mirrors wings router_server_transfer.go: stop the server, archive its
/// data dir and stream it (multipart, with a sha256 checksum) to the
/// destination's POST /api/transfers. The destination node reports the
/// outcome to the panel; on failure we notify the panel ourselves.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TransferRequest {
    url: String,
    token: String,
    #[serde(default)]
    server: Option<serde_json::Value>,
}

async fn post_server_transfer(
    State(state): State<DaemonState>,
    server: crate::router::middleware::ServerExtractor,
    Json(payload): Json<TransferRequest>,
) -> AppResult<Response> {
    if server.is_transferring() {
        return Err(AppError::Conflict("A transfer is already in progress for this server.".into()));
    }
    server.set_transferring(true);

    // Ensure the server is offline before archiving its data.
    if *server.state.read().await != crate::server::ServerState::Offline {
        if let Err(e) = server.power_stop(15).await {
            server.set_transferring(false);
            return Err(e);
        }
        if *server.state.read().await != crate::server::ServerState::Offline {
            let _ = server.power_kill().await;
        }
    }

    let (url, token) = (payload.url, payload.token);
    let task_server = server.0.clone();
    let panel = state.panel.clone();
    let handle = tokio::spawn(async move {
        let result = push_archive_to_target(&task_server, &url, &token).await;
        match result {
            Ok(()) => {
                // Do NOT notify the panel of success: only the destination
                // node reports success (wings behavior).
                tracing::info!(uuid = %task_server.uuid, "outgoing transfer complete");
            }
            Err(e) => {
                tracing::warn!(uuid = %task_server.uuid, error = %e, "outgoing transfer failed");
                task_server.publish(crate::server::events::ServerEvent::TransferStatus("failure".into()));
                let _ = panel.read().await.post_transfer_status(task_server.uuid, false).await;
                let srv = task_server.clone();
                tokio::spawn(async move {
                    srv.publish_daemon_message(format!("Transfer failed: {e}")).await;
                });
            }
        }
        task_server.set_transferring(false);
    });
    server.set_transfer_task(Some(handle.abort_handle())).await;

    Ok(StatusCode::ACCEPTED.into_response())
}

/// DELETE /api/servers/:server/transfer — cancel an outgoing transfer.
async fn delete_server_transfer(
    server: crate::router::middleware::ServerExtractor,
) -> AppResult<Response> {
    if !server.is_transferring() {
        return Err(AppError::Conflict("Server is not currently being transferred.".into()));
    }
    server.cancel_transfer_task().await;
    server.set_transferring(false);
    server.publish(crate::server::events::ServerEvent::TransferStatus("failure".into()));
    Ok(StatusCode::ACCEPTED.into_response())
}

/// Archive the server data dir to a temp tar.gz, then POST it to the
/// destination as multipart/form-data with a sha256 checksum part
/// (mirrors wings server/transfer/source.go).
async fn push_archive_to_target(
    server: &Arc<crate::server::Server>,
    url: &str,
    token: &str,
) -> AppResult<()> {
    use sha2::{Digest, Sha256};

    let root = server.fs.root().to_path_buf();
    let daemon = server.daemon.read().await.clone();
    let archive_path = daemon.tmp_dir().join(format!("{}.outgoing.tar.gz", server.uuid));

    std::fs::create_dir_all(daemon.tmp_dir())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot create tmp dir: {e}")))?;

    // 1. Build the archive and checksum.
    let mut hasher = Sha256::new();
    {
        let gz = std::fs::File::create(&archive_path)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot create archive: {e}")))?;
        let encoder = flate2::write::GzEncoder::new(gz, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let entries = walkdir::WalkDir::new(&root).into_iter().filter_map(|e| e.ok());
        for entry in entries {
            let path = entry.path();
            if path == root {
                continue;
            }
            let rel = path.strip_prefix(&root).unwrap_or(path);
            builder
                .append_path_with_name(path, rel)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot add {}: {e}", rel.display())))?;
        }
        let encoder = builder.into_inner().map_err(|e| {
            AppError::Internal(anyhow::anyhow!("cannot finish archive: {e}"))
        })?;
        let _file = encoder.finish().map_err(|e| {
            AppError::Internal(anyhow::anyhow!("cannot finish gzip: {e}"))
        })?;
        let mut file = std::fs::File::open(&archive_path)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot reopen archive: {e}")))?;
        let mut buf = [0u8; 65536];
        loop {
            use std::io::Read;
            let n = file.read(&mut buf).map_err(|e| {
                AppError::Internal(anyhow::anyhow!("cannot read archive: {e}"))
            })?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        drop(file);
    }
    let checksum = hex_lower(&hasher.finalize());
    let size = std::fs::metadata(&archive_path).map(|m| m.len()).unwrap_or(0);
    tracing::info!(uuid = %server.uuid, bytes = size, "transfer archive built");

    // 2. Stream it to the destination (wings: Authorization header is the
    // raw token string provided by the panel, e.g. "Bearer ...").
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(&archive_path).await.map_err(|e| {
        AppError::Internal(anyhow::anyhow!("cannot open archive: {e}"))
    })?;
    let stream = futures_util::stream::try_unfold(file, |mut f| async move {
        let mut buf = vec![0u8; 65536];
        let n = f.read(&mut buf).await.map_err(std::io::Error::from)?;
        if n == 0 {
            Ok::<Option<(bytes::Bytes, tokio::fs::File)>, std::io::Error>(None)
        } else {
            buf.truncate(n);
            Ok(Some((bytes::Bytes::from(buf), f)))
        }
    });
    let part = reqwest::multipart::Part::stream(reqwest::Body::wrap_stream(stream))
        .file_name("archive.tar.gz")
        .mime_str("application/gzip")
        .map_err(|e| AppError::Internal(anyhow::anyhow!("bad mime: {e}")))?;
    let checksum_part = reqwest::multipart::Part::text(checksum);
    let form = reqwest::multipart::Form::new()
        .part("archive", part)
        .part("checksum", checksum_part);

    let client = reqwest::Client::builder()
        .http1_only()
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot build http client: {e}")))?;
    let resp = client
        .post(url)
        .header("Authorization", token)
        .multipart(form)
        .send()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot reach destination: {e}")))?;
    if !resp.status().is_success() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "unexpected status code from destination: {}",
            resp.status()
        )));
    }

    let _ = std::fs::remove_file(&archive_path);
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}
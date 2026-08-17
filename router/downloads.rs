use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;

use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::jwt::Claims;
use crate::server::events::ServerEvent;
use crate::state::DaemonState;

pub fn router() -> Router<DaemonState> {
    Router::new()
        .route("/download/backup", get(download_backup))
        .route("/download/file", get(download_file))
        .route("/upload/file", post(upload_file))
        // Transfer destination endpoint (called by other daemons).
        .route("/api/transfers", post(incoming_transfer))
}

/// Cancel an incoming transfer for a server (panel-initiated). Mirrors
/// wings `deleteTransfer`: 404 for an unknown server (via the protected
/// router's server_exists), 409 unless a transfer is actually in progress.
/// Lives on the protected router (authorization required).
pub async fn delete_incoming_transfer(
    State(state): State<DaemonState>,
    Path(uuid): Path<Uuid>,
) -> AppResult<StatusCode> {
    let server = state.manager.get(uuid).await?;
    if !server.is_transferring() {
        return Err(AppError::Conflict(
            "Server is not currently being transferred.".into(),
        ));
    }
    server.cancel_incoming_transfer().await;
    Ok(StatusCode::ACCEPTED)
}

#[derive(Debug, Deserialize)]
struct TokenQuery {
    token: String,
}

/// Parse + validate a scoped JWT, and load the target server from the
/// manager. Requires `server_uuid` to exist on this node.
async fn authorize(
    state: DaemonState,
    token: &str,
    expected_scope: &str,
) -> AppResult<(Claims, Arc<crate::server::Server>)> {
    let secret = state.config.read().await.token.clone();
    let claims =
        crate::jwt::parse_token(token, secret.as_bytes(), &state.tokens, state.boot_time).await?;

    if !claims.has_scope(expected_scope) {
        return Err(AppError::Unauthorized(format!("token does not have the {expected_scope} scope")));
    }

    let server_uuid = claims
        .server_uuid()
        .ok_or_else(|| AppError::Unauthorized("token has no server_uuid claim".into()))?;
    let uuid = Uuid::parse_str(&server_uuid)
        .map_err(|_| AppError::Unauthorized("invalid server_uuid claim".into()))?;

    let server = state.manager.get(uuid).await?;
    Ok((claims, server))
}

/// GET /download/backup?token=... — stream a backup archive (single-use).
async fn download_backup(
    State(state): State<DaemonState>,
    Query(query): Query<TokenQuery>,
) -> AppResult<Response> {
    let (claims, _server) = authorize(state.clone(), &query.token, "backup-download").await?;
    let backup_uuid = claims
        .backup_uuid
        .ok_or_else(|| AppError::Unauthorized("token has no backup_uuid claim".into()))?;

    let daemon = state.config.read().await.clone();
    let archive = daemon.backup_dir().join(format!("{backup_uuid}.tar.gz"));
    if !archive.exists() {
        return Err(AppError::ServerNotFound);
    }

    // Stream the file in chunks instead of loading it into memory.
    let file = tokio::fs::File::open(&archive)
        .await
        .map_err(|e| AppError::BadRequest(format!("cannot read backup: {e}")))?;
    let stream = tokio_util::io::ReaderStream::new(file);
    let filename = format!("{backup_uuid}.tar.gz");
    let mut resp = axum::body::Body::from_stream(stream).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/gzip"));
    resp.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .unwrap_or(HeaderValue::from_static("attachment")),
    );
    Ok(resp)
}

#[derive(Debug, Deserialize)]
struct FileQuery {
    #[serde(default)]
    file: Option<String>,
}

/// GET /download/file?token=... — stream one file (single-use). The path
/// comes from the JWT `file_path` claim (wings parity); `?file=` is kept as
/// a fallback for tokens issued without it.
async fn download_file(
    State(state): State<DaemonState>,
    Query(query): Query<TokenQuery>,
    Query(file_q): Query<FileQuery>,
) -> AppResult<Response> {
    let (claims, server) = authorize(state, &query.token, "file-download").await?;
    let raw = claims
        .file_path
        .or(file_q.file)
        .ok_or_else(|| AppError::BadRequest("no file path in request".into()))?;
    let file_path = server.fs.resolve(&raw)?;
    server.fs.check_denied(&raw)?;
    let filename = raw
        .rsplit('/')
        .next()
        .unwrap_or("file")
        .to_string();

    let file = tokio::fs::File::open(&file_path)
        .await
        .map_err(|e| AppError::BadRequest(format!("cannot read file: {e}")))?;
    let stream = tokio_util::io::ReaderStream::new(file);

    let mut resp = axum::body::Body::from_stream(stream).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    resp.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .unwrap_or(HeaderValue::from_static("attachment")),
    );
    Ok(resp)
}

#[derive(Debug, Deserialize)]
struct UploadQuery {
    token: String,
    #[serde(default)]
    directory: String,
}

/// POST /upload/file?token=...&directory=... — multipart upload under the
/// `files` field name (wings parity). `directory` is optional.
async fn upload_file(
    State(state): State<DaemonState>,
    Query(query): Query<UploadQuery>,
    axum::extract::connect_info::ConnectInfo(addr): axum::extract::connect_info::ConnectInfo<std::net::SocketAddr>,
    mut multipart: Multipart,
) -> AppResult<Response> {
    let client_ip = addr.ip().to_string();
    let (claims, server) = authorize(state.clone(), &query.token, "file-upload").await?;
    let daemon = state.config.read().await.clone();
    let max_bytes = daemon.api.upload_limit.saturating_mul(1024 * 1024);

    let mut uploaded = 0usize;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("multipart error: {e}")))?
    {
        if field.name() != Some("files") && field.name() != Some("files[]") {
            continue;
        }
        let name = field
            .file_name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "upload".to_string());
        if name.contains("..") || name.contains('/') {
            return Err(AppError::BadRequest("invalid file name".into()));
        }

        let mut collected = Vec::new();
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| AppError::BadRequest(format!("upload interrupted: {e}")))?
        {
            collected.extend_from_slice(&chunk);
            if max_bytes > 0 && collected.len() as u64 > max_bytes {
                return Err(AppError::BadRequest(format!(
                    "File {name} is larger than the maximum file upload size of {} MB.",
                    daemon.api.upload_limit
                )));
            }
        }

        let path = format!("{}/{}", query.directory.trim_matches('/'), name);
        server.fs.write(&path, &collected)?;
        uploaded += 1;

        state.activity.push(
            crate::models::Activity::new(&server.uuid.to_string(), "server:file.uploaded")
                .with_user(claims.user_uuid.clone())
                .with_ip(client_ip.clone())
                .with_metadata(serde_json::json!({
                    "directory": query.directory,
                    "name": name,
                    "size": collected.len() as u64,
                })),
        );
    }

    if uploaded == 0 {
        return Err(AppError::BadRequest(
            "No files were found on the request body.".into(),
        ));
    }

    Ok(StatusCode::OK.into_response())
}

/// POST /api/transfers — destination node endpoint for server transfers.
/// The sending daemon streams a multipart form: "archive" (tar.gz of the
/// server data) and "checksum" (sha256 hex of the archive). Auth is a
/// panel-issued JWT with scope "transfer" and sub = server uuid.
async fn incoming_transfer(
    State(state): State<DaemonState>,
    headers: axum::http::HeaderMap,
    mut multipart: Multipart,
) -> AppResult<Response> {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| AppError::Unauthorized("missing Bearer token".into()))?;

    let secret = state.config.read().await.token.clone();
    let claims =
        crate::jwt::parse_token(bearer, secret.as_bytes(), &state.tokens, state.boot_time).await?;
    if !claims.has_scope("transfer") {
        return Err(AppError::Unauthorized("token does not have the transfer scope".into()));
    }
    let uuid = claims
        .sub
        .and_then(|s| Uuid::parse_str(&s).ok())
        .ok_or_else(|| AppError::Unauthorized("token has no valid server subject".into()))?;

    let server = state.manager.register(uuid).await?;
    if server.is_transferring() {
        return Err(AppError::Conflict("A transfer is already in progress for this server.".into()));
    }
    server.set_transferring(true);
    let cancel = server.fresh_incoming_cancel().await;

    let data_dir = server.fs.root().to_path_buf();
    let daemon = state.config.read().await.clone();
    let tmp_dir = daemon.tmp_dir();
    std::fs::create_dir_all(&tmp_dir)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot create tmp dir: {e}")))?;
    let archive_path = tmp_dir.join(format!("{uuid}.transfer.tar.gz"));

    let result = tokio::select! {
        r = receive_transfer_archive(&mut multipart, &archive_path, &data_dir) => r,
        _ = cancel.cancelled() => {
            Err(AppError::Internal(anyhow::anyhow!("incoming transfer cancelled")))
        }
    };

    let successful = result.is_ok();
    if successful {
        server.publish(ServerEvent::TransferStatus("success".into()));
        tracing::info!(uuid = %uuid, "incoming transfer completed");
    } else {
        let err = result.unwrap_err();
        tracing::warn!(uuid = %uuid, error = %err, "incoming transfer failed");
        server.publish(ServerEvent::TransferStatus("failure".into()));
    }

    server.set_transferring(false);
    server.clear_incoming_cancel().await;
    let _ = state
        .panel
        .read()
        .await
        .post_transfer_status(uuid, successful)
        .await;

    if !successful {
        // Mirror wings: drop the server from the manager and delete the
        // extracted files so a retry starts from a clean slate.
        let _ = state.manager.remove(uuid).await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }
    let _ = std::fs::remove_file(&archive_path);

    Ok(StatusCode::OK.into_response())
}

/// Drain the multipart body: stream "archive" into `archive_path` (hashing
/// it as we go), then read "checksum" and verify. The extracted payload is
/// unpacked into `data_dir` (wings ExtractStreamUnsafe).
async fn receive_transfer_archive(
    multipart: &mut Multipart,
    archive_path: &std::path::Path,
    data_dir: &std::path::Path,
) -> AppResult<()> {
    use sha2::{Digest, Sha256};

    let mut file = std::fs::File::create(archive_path)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot create archive: {e}")))?;
    let mut hasher = Sha256::new();
    let mut got_archive = false;
    let mut checksum: Option<String> = None;

    while let Some(mut field) = multipart.next_field().await.map_err(|e| {
        AppError::BadRequest(format!("transfer multipart error: {e}"))
    })? {
        match field.name() {
            Some("archive") => {
                let mut chunks = 0usize;
                while let Some(chunk) = field.chunk().await.map_err(|e| {
                    AppError::BadRequest(format!("transfer archive interrupted: {e}"))
                })? {
                    hasher.update(&chunk);
                    std::io::Write::write_all(&mut file, &chunk).map_err(|e| {
                        AppError::Internal(anyhow::anyhow!("cannot write archive: {e}"))
                    })?;
                    chunks += chunk.len();
                }
                tracing::info!(bytes = chunks, "received transfer archive");
                got_archive = true;
            }
            Some("checksum") => {
                if !got_archive {
                    return Err(AppError::BadRequest("archive must be sent before the checksum".into()));
                }
                let mut buf = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(|e| {
                    AppError::BadRequest(format!("transfer checksum interrupted: {e}"))
                })? {
                    buf.extend_from_slice(&chunk);
                }
                checksum = Some(String::from_utf8_lossy(&buf).trim().to_string());
            }
            _ => {}
        }
    }

    if !got_archive {
        return Err(AppError::BadRequest("missing archive part".into()));
    }
    let expected = checksum.ok_or_else(|| AppError::BadRequest("missing checksum part".into()))?;
    let actual = hex_lower(&hasher.finalize());
    if expected != actual {
        return Err(AppError::BadRequest(format!(
            "archive checksum mismatch: expected {expected}, got {actual}"
        )));
    }

    std::fs::create_dir_all(data_dir)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot create data dir: {e}")))?;
    let gz = std::fs::File::open(archive_path)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot open archive: {e}")))?;
    let decoder = flate2::read::GzDecoder::new(gz);
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(data_dir)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot extract archive: {e}")))?;
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
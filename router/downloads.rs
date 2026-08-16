use axum::extract::{Multipart, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;

use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::jwt::Claims;
use crate::state::DaemonState;

pub fn router() -> Router<DaemonState> {
    Router::new()
        .route("/download/backup", get(download_backup))
        .route("/download/file", get(download_file))
        .route("/upload/file", post(upload_file))
        // Transfer destination endpoint (called by other daemons).
        .route("/api/transfers", post(incoming_transfer))
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

    let bytes = std::fs::read(&archive)
        .map_err(|e| AppError::BadRequest(format!("cannot read backup: {e}")))?;
    let filename = format!("{backup_uuid}.tar.gz");
    let mut resp = bytes.into_response();
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
    file: String,
}

/// GET /download/file?token=...&file=... — stream one file (single-use).
async fn download_file(
    State(state): State<DaemonState>,
    Query(query): Query<TokenQuery>,
    Query(file_q): Query<FileQuery>,
) -> AppResult<Response> {
    let (_claims, server) = authorize(state, &query.token, "file-download").await?;
    let bytes = server.fs.read(&file_q.file)?;
    let filename = file_q
        .file
        .rsplit('/')
        .next()
        .unwrap_or("file")
        .to_string();

    let mut resp = bytes.into_response();
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
    directory: String,
}

/// POST /upload/file?token=...&directory=... — multipart upload (files[]).
async fn upload_file(
    State(state): State<DaemonState>,
    Query(query): Query<UploadQuery>,
    mut multipart: Multipart,
) -> AppResult<Response> {
    let (claims, server) = authorize(state.clone(), &query.token, "file-upload").await?;
    let daemon = state.config.read().await.clone();
    let max_bytes = daemon.api.upload_limit.saturating_mul(1024 * 1024);

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("multipart error: {e}")))?
    {
        if field.name() != Some("files[]") {
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
                return Err(AppError::PayloadTooLarge);
            }
        }

        let path = format!("{}/{}", query.directory.trim_matches('/'), name);
        server.fs.write(&path, &collected)?;

        state.activity.push(
            crate::models::Activity::new(&server.uuid.to_string(), "server:file.uploaded")
                .with_user(claims.user_uuid.clone())
                .with_metadata(serde_json::json!({
                    "directory": query.directory,
                    "name": name,
                    "size": collected.len() as u64,
                })),
        );
    }

    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn incoming_transfer() -> AppResult<Response> {
    Err(AppError::NotImplemented("incoming transfers are not implemented in wings-rs".into()))
}
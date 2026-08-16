use std::sync::Arc;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, post};
use axum::Json;
use axum::Router;
use serde::Deserialize;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::router::middleware::ServerExtractor;
use crate::remote::types::BackupRemoteStatus;
use crate::server::events::ServerEvent;
use crate::state::DaemonState;

pub fn router() -> Router<DaemonState> {
    Router::new()
        .route("/api/servers/{server}/backup", post(create_backup))
        .route(
            "/api/servers/{server}/backup/{backup}/restore",
            post(restore_backup),
        )
        .route("/api/servers/{server}/backup/{backup}", delete(delete_backup))
}

#[derive(Debug, Deserialize)]
struct BackupRequest {
    // "wings" (local) or "s3"
    adapter: String,
    uuid: Uuid,
    #[serde(default)]
    ignore: String,
}

/// POST /api/servers/:id/backup — create a local (or S3) backup.
async fn create_backup(
    server: ServerExtractor,
    Json(payload): Json<BackupRequest>,
) -> AppResult<Response> {
    if payload.adapter != "wings" {
        return Err(AppError::NotImplemented("only the wings backup adapter is implemented".into()));
    }
    if server.is_installing() {
        return Err(AppError::Conflict("server is installing".into()));
    }

    let srv = server.0.clone();
    tokio::spawn(async move {
        run_backup(srv, payload.uuid, payload.ignore).await;
    });

    Ok(StatusCode::ACCEPTED.into_response())
}

async fn run_backup(server: Arc<crate::server::Server>, backup_uuid: Uuid, ignore: String) {
    server.publish(ServerEvent::DaemonMessage("Preparing backup...".to_string()));

    let daemon = server.daemon.read().await.clone();
    let backup_dir = daemon.backup_dir();
    if let Err(e) = std::fs::create_dir_all(&backup_dir) {
        tracing::error!(error = %e, "cannot create backup dir");
        server.publish(ServerEvent::BackupCompleted(backup_failed_json(backup_uuid)));
        return;
    }

    let archive_path = backup_dir.join(format!("{backup_uuid}.tar.gz"));

    let result = (async {
        let file = std::fs::File::create(&archive_path)?;
        let gz = flate2::GzBuilder::new()
            .filename(format!("{backup_uuid}.tar.gz"))
            .write(file, flate2::Compression::new(1));
        let mut tar = tar::Builder::new(gz);

        for path in server.fs.walk_files(&ignore) {
            let rel = path.strip_prefix(server.fs.root()).unwrap_or(&path);
            if rel.as_os_str().is_empty() {
                continue;
            }
            tar.append_path_with_name(&path, rel)?;
        }
        let gz = tar.into_inner()?;
        gz.finish()?.sync_all()?;
        Ok::<_, std::io::Error>(())
    })
    .await;

    match result {
        Ok(()) => {
            let size = std::fs::metadata(&archive_path).map(|m| m.len() as i64).unwrap_or(0);
            let checksum = server.fs.checksum_sha1(&archive_path).unwrap_or_default();

            let status = BackupRemoteStatus {
                checksum: checksum.clone(),
                checksum_type: "sha1".to_string(),
                size,
                successful: true,
                parts: vec![],
            };
            let _ = server
                .panel
                .read()
                .await
                .post_backup_status(backup_uuid, &status)
                .await;

            let payload = serde_json::json!({
                "uuid": backup_uuid,
                "is_successful": true,
                "checksum": checksum,
                "checksum_type": "sha1",
                "file_size": size,
            });
            server.publish(ServerEvent::BackupCompleted(payload.to_string()));
            tracing::info!(uuid = %backup_uuid, size, "backup completed");
        }
        Err(e) => {
            tracing::error!(uuid = %backup_uuid, error = %e, "backup failed");
            let _ = std::fs::remove_file(&archive_path);
            let _ = server
                .panel
                .read()
                .await
                .post_backup_status(backup_uuid, &BackupRemoteStatus {
                    checksum: String::new(),
                    checksum_type: "sha1".to_string(),
                    size: 0,
                    successful: false,
                    parts: vec![],
                })
                .await;
            server.publish(ServerEvent::BackupCompleted(backup_failed_json(backup_uuid)));
        }
    }
}

fn backup_failed_json(uuid: Uuid) -> String {
    serde_json::json!({
        "uuid": uuid,
        "is_successful": false,
        "checksum": "",
        "checksum_type": "sha1",
        "file_size": 0,
    })
    .to_string()
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct RestoreRequest {
    adapter: String,
    #[serde(default)]
    truncate_directory: bool,
    #[serde(default)]
    download_url: String,
}

/// POST /api/servers/:id/backup/:backup/restore
async fn restore_backup(
    server: ServerExtractor,
    Path(backup_uuid): Path<Uuid>,
    Json(payload): Json<RestoreRequest>,
) -> AppResult<Response> {
    if payload.adapter != "wings" {
        return Err(AppError::NotImplemented("only the wings backup adapter is implemented".into()));
    }

    let srv = server.0.clone();
    tokio::spawn(async move {
        run_restore(srv, backup_uuid, payload.truncate_directory).await;
    });

    Ok(StatusCode::ACCEPTED.into_response())
}

async fn run_restore(server: Arc<crate::server::Server>, backup_uuid: Uuid, truncate: bool) {
    server.publish(ServerEvent::DaemonMessage(
        "(restoring): backup selected; stopping server...".to_string(),
    ));

    if server.is_running() {
        let _ = server.power_stop(30).await;
    }

    let daemon = server.daemon.read().await.clone();
    let archive = daemon.backup_dir().join(format!("{backup_uuid}.tar.gz"));

    let result = (async {
        if !archive.exists() {
            return Err(anyhow::anyhow!("backup {} does not exist on this node", backup_uuid));
        }
        if truncate {
            server.fs.truncate_directory()?;
        }
        server.fs.decompress_archive(&archive)?;
        Ok::<_, anyhow::Error>(())
    })
    .await;

    if let Err(e) = &result {
        tracing::error!(uuid = %backup_uuid, error = %e, "restore failed");
        server.publish(ServerEvent::DaemonMessage(format!("(restoring): failed: {e}")));
    } else {
        server.publish(ServerEvent::DaemonMessage("(restoring): completed".to_string()));
    }

    let _ = server
        .panel
        .read()
        .await
        .post_backup_restore_status(backup_uuid, result.is_ok())
        .await;
}

/// DELETE /api/servers/:id/backup/:backup — delete the local archive.
async fn delete_backup(
    server: ServerExtractor,
    Path(backup_uuid): Path<Uuid>,
) -> AppResult<Response> {
    let daemon = server.daemon.read().await.clone();
    let archive = daemon.backup_dir().join(format!("{backup_uuid}.tar.gz"));
    if !archive.exists() {
        return Err(AppError::ServerNotFound);
    }
    std::fs::remove_file(&archive)
        .map_err(|e| AppError::BadRequest(format!("cannot delete backup: {e}")))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
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
        .route("/api/servers/:server/backup", post(create_backup))
        .route(
            "/api/servers/:server/backup/:backup/restore",
            post(restore_backup),
        )
        .route("/api/servers/:server/backup/:backup", delete(delete_backup))
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
    if payload.adapter != "wings" && payload.adapter != "s3" {
        return Err(AppError::NotImplemented(
            "only the wings and s3 backup adapters are implemented".into(),
        ));
    }
    if server.is_installing() {
        return Err(AppError::Conflict("server is installing".into()));
    }

    let srv = server.0.clone();
    let adapter = payload.adapter.clone();
    tokio::spawn(async move {
        if adapter == "s3" {
            run_s3_backup(srv, payload.uuid, payload.ignore).await;
        } else {
            run_backup(srv, payload.uuid, payload.ignore).await;
        }
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

    let result = create_archive(&server, &archive_path, &ignore).await;

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

/// Build the gzipped tar archive of the server data directory.
async fn create_archive(
    server: &crate::server::Server,
    archive_path: &std::path::Path,
    ignore: &str,
) -> std::io::Result<()> {
    let file = std::fs::File::create(archive_path)?;
    let gz = flate2::GzBuilder::new()
        .filename("backup.tar.gz")
        .write(file, flate2::Compression::new(1));
    let mut tar = tar::Builder::new(gz);

    for path in server.fs.walk_files(ignore) {
        let rel = path.strip_prefix(server.fs.root()).unwrap_or(&path);
        if rel.as_os_str().is_empty() {
            continue;
        }
        tar.append_path_with_name(&path, rel)?;
    }
    let gz = tar.into_inner()?;
    gz.finish()?.sync_all()
}

/// S3 adapter: build the archive locally, stream it to the panel-provided
/// presigned upload URLs, then report the upload parts. Mirrors wings
/// `backup_s3.go` (Generate -> generateRemoteRequest -> uploadPart).
async fn run_s3_backup(server: Arc<crate::server::Server>, backup_uuid: Uuid, ignore: String) {
    use crate::remote::types::BackupPart;

    server.publish(ServerEvent::DaemonMessage("Preparing backup...".to_string()));

    let daemon = server.daemon.read().await.clone();
    let backup_dir = daemon.backup_dir();
    if let Err(e) = std::fs::create_dir_all(&backup_dir) {
        tracing::error!(error = %e, "cannot create backup dir");
        server.publish(ServerEvent::BackupCompleted(backup_failed_json(backup_uuid)));
        return;
    }

    let archive_path = backup_dir.join(format!("{backup_uuid}.tar.gz"));
    let _ = std::fs::remove_file(&archive_path);

    if let Err(e) = create_archive(&server, &archive_path, &ignore).await {
        tracing::error!(uuid = %backup_uuid, error = %e, "s3 backup archive creation failed");
        let _ = server
            .panel
            .read()
            .await
            .post_backup_status(
                backup_uuid,
                &BackupRemoteStatus {
                    checksum: String::new(),
                    checksum_type: "sha1".to_string(),
                    size: 0,
                    successful: false,
                    parts: vec![],
                },
            )
            .await;
        server.publish(ServerEvent::BackupCompleted(backup_failed_json(backup_uuid)));
        return;
    }

    let size = std::fs::metadata(&archive_path)
        .map(|m| m.len())
        .unwrap_or(0);

    let urls = match server
        .panel
        .read()
        .await
        .get_backup_remote_upload_urls(backup_uuid, size as i64)
        .await
    {
        Ok(urls) => urls,
        Err(e) => {
            tracing::error!(uuid = %backup_uuid, error = %e, "failed to get S3 upload urls");
            let _ = std::fs::remove_file(&archive_path);
            let _ = server
                .panel
                .read()
                .await
                .post_backup_status(
                    backup_uuid,
                    &BackupRemoteStatus {
                        checksum: String::new(),
                        checksum_type: "sha1".to_string(),
                        size: 0,
                        successful: false,
                        parts: vec![],
                    },
                )
                .await;
            server.publish(ServerEvent::BackupCompleted(backup_failed_json(backup_uuid)));
            return;
        }
    };

    let file = std::fs::File::open(&archive_path);
    let mut reader = match file {
        Ok(f) => std::io::BufReader::new(f),
        Err(e) => {
            tracing::error!(uuid = %backup_uuid, error = %e, "cannot open backup archive");
            let _ = std::fs::remove_file(&archive_path);
            let _ = server
                .panel
                .read()
                .await
                .post_backup_status(
                    backup_uuid,
                    &BackupRemoteStatus {
                        checksum: String::new(),
                        checksum_type: "sha1".to_string(),
                        size: 0,
                        successful: false,
                        parts: vec![],
                    },
                )
                .await;
            server.publish(ServerEvent::BackupCompleted(backup_failed_json(backup_uuid)));
            return;
        }
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2 * 60 * 60))
        .build()
        .unwrap_or_default();

    let mut uploaded_parts = Vec::new();
    let mut failed = false;
    for (i, part_url) in urls.parts.iter().enumerate() {
        let part_size = if i + 1 < urls.parts.len() {
            urls.part_size as u64
        } else {
            size - (i as u64 * urls.part_size as u64)
        };

        let mut limited = std::io::Read::take(&mut reader, part_size);
        let mut body = Vec::with_capacity(part_size as usize);
        if std::io::Read::read_to_end(&mut limited, &mut body).is_err() {
            failed = true;
            break;
        }

        let mut attempts = 0u32;
        let etag = loop {
            attempts += 1;
            let res = client
                .put(part_url.clone())
                .header("Content-Type", "application/x-gzip")
                .header("Content-Length", part_size.to_string())
                .body(body.clone())
                .send()
                .await;
            match res {
                Ok(r) if r.status().is_success() => {
                    break r
                        .headers()
                        .get("etag")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                }
                Ok(r) if r.status().is_server_error() && attempts < 6 => {
                    let wait = std::time::Duration::from_millis(200 * 2u64.pow(attempts - 1));
                    tokio::time::sleep(wait).await;
                }
                Ok(r) => {
                    tracing::error!(uuid = %backup_uuid, status = %r.status(), "S3 part upload failed");
                    failed = true;
                    break String::new();
                }
                Err(e) => {
                    tracing::warn!(uuid = %backup_uuid, error = %e, attempts, "S3 part upload error, retrying");
                    if attempts >= 6 {
                        failed = true;
                        break String::new();
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200 * 2u64.pow(attempts - 1))).await;
                }
            }
        };
        if failed {
            break;
        }
        uploaded_parts.push(BackupPart {
            part_number: (i + 1) as i64,
            etag,
        });
        tracing::info!(uuid = %backup_uuid, part = i + 1, "S3 backup part uploaded");
    }

    let successful = !failed;
    let _ = std::fs::remove_file(&archive_path);

    let _ = server
        .panel
        .read()
        .await
        .post_backup_status(
            backup_uuid,
            &BackupRemoteStatus {
                checksum: String::new(),
                checksum_type: "sha1".to_string(),
                size: if successful { size as i64 } else { 0 },
                successful,
                parts: uploaded_parts,
            },
        )
        .await;

    let payload = serde_json::json!({
        "uuid": backup_uuid,
        "is_successful": successful,
        "checksum": "",
        "checksum_type": "sha1",
        "file_size": if successful { size } else { 0 },
    });
    server.publish(ServerEvent::BackupCompleted(payload.to_string()));
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

    server.publish(ServerEvent::BackupRestoreCompleted(
        serde_json::json!({
            "uuid": backup_uuid,
            "successful": result.is_ok(),
        })
        .to_string(),
    ));

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
use std::os::unix::fs::FileTypeExt;
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

/// Build the gzipped tar archive of the server data directory. Mirrors
/// wings archive.go: symlinks are stored as links (not followed), sockets
/// are skipped, the compression level comes from system.backups
/// compression_level, and writes are throttled by system.backups
/// write_limit (MiB/s) when configured.
async fn create_archive(
    server: &crate::server::Server,
    archive_path: &std::path::Path,
    ignore: &str,
) -> std::io::Result<()> {
    let daemon = server.daemon.read().await.clone();
    let compression_level = daemon.system.backups.compression_level.clone();
    let write_limit = daemon.system.backups.write_limit;
    drop(daemon);

    let file = std::fs::File::create(archive_path)?;
    let writer: ArchiveWriter = if write_limit > 0 {
        ArchiveWriter::Throttled(ThrottledWriter::new(file, write_limit))
    } else {
        ArchiveWriter::Plain(file)
    };

    let level = match compression_level.as_str() {
        "none" => flate2::Compression::none(),
        "best_compression" => flate2::Compression::best(),
        _ => flate2::Compression::new(1),
    };
    let gz = flate2::GzBuilder::new()
        .filename("backup.tar.gz")
        .write(writer, level);
    let mut tar = tar::Builder::new(gz);
    tar.follow_symlinks(false);

    for path in server.fs.walk_files(ignore) {
        let rel = path.strip_prefix(server.fs.root()).unwrap_or(&path);
        if rel.as_os_str().is_empty() {
            continue;
        }
        // Skip sockets (archive/tar does not support them).
        if let Ok(ft) = std::fs::symlink_metadata(&path) {
            if ft.file_type().is_socket() {
                continue;
            }
        }
        tar.append_path_with_name(&path, rel)?;
    }
    let gz = tar.into_inner()?;
    gz.finish()?.sync_all()
}

/// Either the raw archive file or a throttled wrapper around it.
enum ArchiveWriter {
    Plain(std::fs::File),
    Throttled(ThrottledWriter<std::fs::File>),
}

impl std::io::Write for ArchiveWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(f) => f.write(buf),
            Self::Throttled(t) => t.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(f) => f.flush(),
            Self::Throttled(t) => t.flush(),
        }
    }
}

impl ArchiveWriter {
    fn sync_all(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(f) => f.sync_all(),
            Self::Throttled(t) => t.inner.sync_all(),
        }
    }
}

/// Token-bucket write throttle (system.backups.write_limit MiB/s), mirroring
/// wings' ratelimit writer around the archive file.
struct ThrottledWriter<W: std::io::Write> {
    inner: W,
    limit_bytes_per_sec: u64,
    tokens: f64,
    last_refill: std::time::Instant,
}

impl<W: std::io::Write> ThrottledWriter<W> {
    fn new(inner: W, limit_mib_per_sec: i64) -> Self {
        Self {
            inner,
            limit_bytes_per_sec: limit_mib_per_sec.max(1) as u64 * 1024 * 1024,
            tokens: 0.0,
            last_refill: std::time::Instant::now(),
        }
    }
}

impl<W: std::io::Write> std::io::Write for ThrottledWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * self.limit_bytes_per_sec as f64)
            .min(self.limit_bytes_per_sec as f64);
        let mut written = 0;
        while written < buf.len() {
            if self.tokens < 1.0 {
                let missing = 1.0 - self.tokens;
                let wait = missing / self.limit_bytes_per_sec as f64;
                std::thread::sleep(std::time::Duration::from_secs_f64(wait));
                self.tokens += wait * self.limit_bytes_per_sec as f64;
            }
            let take = (self.tokens as usize).max(1).min(buf.len() - written);
            let n = self.inner.write(&buf[written..written + take])?;
            self.tokens -= n as f64;
            written += n;
            if n == 0 {
                break;
            }
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
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
    match payload.adapter.as_str() {
        "wings" => {
            let srv = server.0.clone();
            tokio::spawn(async move {
                run_restore(srv, backup_uuid, payload.truncate_directory).await;
            });
        }
        "s3" => {
            if payload.download_url.is_empty() {
                return Err(AppError::BadRequest(
                    "The download_url field is required when the backup adapter is set to S3.".into(),
                ));
            }
            // SSRF validation before anything else (wings validateBackupDownloadUrl).
            let allowlist = {
                let daemon = server.daemon.read().await.clone();
                daemon.system.backups.restore_host_allowlist
            };
            if let Err(e) = validate_backup_download_url(&payload.download_url, &allowlist).await {
                return Err(AppError::BadRequest(e));
            }
            let srv = server.0.clone();
            let url = payload.download_url.clone();
            let allowlist = allowlist.clone();
            tokio::spawn(async move {
                run_s3_restore(srv, backup_uuid, &url, payload.truncate_directory, &allowlist).await;
            });
        }
        other => {
            return Err(AppError::BadRequest(format!(
                "invalid backup adapter: {other} (expected \"wings\" or \"s3\")"
            )));
        }
    }

    Ok(StatusCode::ACCEPTED.into_response())
}

/// S3 restore: download the archive from the presigned URL (SSRF-safe
/// HTTP client), verify content type, then restore like a local backup.
async fn run_s3_restore(
    server: Arc<crate::server::Server>,
    backup_uuid: Uuid,
    download_url: &str,
    truncate: bool,
    allowlist: &[String],
) {
    server.publish(ServerEvent::DaemonMessage(
        "(restoring): backup selected; stopping server...".to_string(),
    ));

    // wings: restore aborts when the server cannot be stopped (WaitForStop,
    // 2 minutes) — truncating and extracting over a live process would
    // corrupt the data.
    if server.is_running() {
        if let Err(e) = server.power_stop(30).await {
            tracing::error!(uuid = %backup_uuid, error = %e, "restore aborted: server did not stop");
            server.publish(ServerEvent::DaemonMessage(format!("(restoring): failed: {e}")));
            server.publish(ServerEvent::BackupRestoreCompleted(
                serde_json::json!({
                    "uuid": backup_uuid,
                    "successful": false,
                })
                .to_string(),
            ));
            let _ = server
                .panel
                .read()
                .await
                .post_backup_restore_status(backup_uuid, false)
                .await;
            return;
        }
    }
    server.set_restoring(true);

    let result = (async {
        if truncate {
            server.fs.truncate_directory()?;
        }

        let client = backup_restore_http_client(allowlist)?;
        let resp = client.get(download_url).send().await.map_err(|e| {
            if e.is_redirect() {
                anyhow::anyhow!("The provided backup link redirects too many times.")
            } else if e.is_timeout() {
                anyhow::anyhow!("The provided backup link timed out.")
            } else {
                anyhow::anyhow!("The provided backup link returned an invalid response: {e}")
            }
        })?;
        if resp.status() != reqwest::StatusCode::OK {
            return Err(anyhow::anyhow!(
                "The provided backup link returned an invalid response status: {}",
                resp.status()
            ));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if !is_supported_backup_restore_content_type(&content_type) {
            return Err(anyhow::anyhow!(
                "The provided backup link is not a supported content type. \"{content_type}\" is not application/x-gzip."
            ));
        }

        // Stream the archive to a temp file instead of buffering the whole
        // response in memory (wings streams the response body).
        let daemon = server.daemon.read().await.clone();
        let tmp = daemon.tmp_dir().join(format!("restore-{backup_uuid}.tar.gz"));
        let stream_result: Result<(), anyhow::Error> = (async {
            use futures_util::StreamExt;
            use tokio::io::AsyncWriteExt;
            let mut out = tokio::fs::File::create(&tmp).await?;
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| anyhow::anyhow!("download failed: {e}"))?;
                out.write_all(&chunk).await?;
            }
            out.flush().await?;
            Ok(())
        })
        .await;
        if let Err(e) = stream_result {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        let result = server.fs.decompress_archive(&tmp);
        let _ = std::fs::remove_file(&tmp);
        result?;
        Ok::<_, anyhow::Error>(())
    })
    .await;

    if let Err(e) = &result {
        tracing::error!(uuid = %backup_uuid, error = %e, "s3 restore failed");
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

    server.set_restoring(false);

    let _ = server
        .panel
        .read()
        .await
        .post_backup_restore_status(backup_uuid, result.is_ok())
        .await;
}

/// Block private/internal destinations on backup restore downloads
/// (wings `validateBackupDownloadUrl`). Unlike wings' per-hop dial-time
/// blocking, hostnames are resolved here so a DNS name pointing at a
/// private/local address is rejected before the request is even sent.
/// Returns the error message.
async fn validate_backup_download_url(raw: &str, allowlist: &[String]) -> Result<(), String> {
    let parsed = match url::Url::parse(raw) {
        Ok(u) => u,
        Err(_) => return Err("The provided backup link is not a valid URL.".into()),
    };
    let host = parsed.host_str().unwrap_or_default();
    if host.is_empty() {
        return Err("The provided backup link is not a valid URL.".into());
    }
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err("The provided backup link must use HTTP or HTTPS.".into());
    }
    let ips: Vec<std::net::IpAddr> = match host.parse::<std::net::IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => match tokio::net::lookup_host((host, parsed.port_or_known_default().unwrap_or(80)))
            .await
        {
            Ok(addrs) => addrs.map(|a| a.ip()).collect(),
            Err(_) => return Err("The provided backup link could not be resolved.".into()),
        },
    };
    if ips.iter().any(|ip| is_blocked_backup_restore_ip(host, *ip, allowlist)) {
        return Err("The provided backup link resolves to a blocked address.".into());
    }
    Ok(())
}

fn is_blocked_backup_restore_ip(host: &str, ip: std::net::IpAddr, allowlist: &[String]) -> bool {
    let allowed = allowlist.iter().any(|entry| {
        let entry = entry.trim();
        if entry.is_empty() {
            return false;
        }
        if entry.eq_ignore_ascii_case(host) {
            return true;
        }
        if let Ok(addr) = entry.parse::<std::net::IpAddr>() {
            if addr == ip {
                return true;
            }
        }
        if let Ok(cidr) = entry.parse::<ipnet::IpNet>() {
            if cidr.contains(&ip) {
                return true;
            }
        }
        false
    });
    if allowed {
        return false;
    }
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || is_private_addr(ip)
        || is_link_local_addr(ip)
}

fn is_private_addr(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.octets()[0] == 10
                || (v4.octets()[0] == 172 && (16..=31).contains(&v4.octets()[1]))
                || (v4.octets()[0] == 192 && v4.octets()[1] == 168)
                || (v4.octets()[0] == 100 && (64..=127).contains(&v4.octets()[1])) // CGNAT
        }
        std::net::IpAddr::V6(v6) => {
            let seg = v6.segments();
            matches!(seg[0], 0xfc00 | 0xfd00 | 0xfe80..=0xfebf) // ULA + link-local
                || seg[0] == 0 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0 && seg[4] == 0 && seg[5] == 0 && seg[6] == 0 && seg[7] == 1 // ::1 handled by loopback
        }
    }
}

fn is_link_local_addr(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.octets()[0] == 169 && v4.octets()[1] == 254,
        std::net::IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// HTTP client for backup restore downloads. Redirects are validated on
/// every hop against the SSRF blocklist before being followed (wings
/// blocks at DialContext per hop); a redirect to a blocked address stops
/// the request.
fn backup_restore_http_client(allowlist: &[String]) -> AppResult<reqwest::Client> {
    let allowlist = allowlist.to_vec();
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        let url = attempt.url();
        let host = url.host_str().unwrap_or_default();
        if host.is_empty() {
            return attempt.stop();
        }
        // Synchronous resolve here is fine: redirects are rare, and this
        // path is only hit when following one.
        let blocked = match host.parse::<std::net::IpAddr>() {
            Ok(ip) => is_blocked_backup_restore_ip(host, ip, &allowlist),
            Err(_) => {
                use std::net::ToSocketAddrs;
                match (host, url.port_or_known_default().unwrap_or(80)).to_socket_addrs() {
                    Ok(addrs) => addrs
                        .map(|a| a.ip())
                        .any(|ip| is_blocked_backup_restore_ip(host, ip, &allowlist)),
                    Err(_) => true,
                }
            }
        };
        if blocked {
            attempt.stop()
        } else {
            attempt.follow()
        }
    });
    reqwest::Client::builder()
        .redirect(policy)
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot build http client: {e}")))
}

fn is_supported_backup_restore_content_type(value: &str) -> bool {
    let media_type = value
        .split(';')
        .next()
        .unwrap_or(value)
        .trim()
        .to_lowercase();
    matches!(media_type.as_str(), "application/x-gzip" | "application/gzip")
}

async fn run_restore(server: Arc<crate::server::Server>, backup_uuid: Uuid, truncate: bool) {
    server.publish(ServerEvent::DaemonMessage(
        "(restoring): backup selected; stopping server...".to_string(),
    ));

    // wings: restore aborts when the server cannot be stopped.
    if server.is_running() {
        if let Err(e) = server.power_stop(30).await {
            tracing::error!(uuid = %backup_uuid, error = %e, "restore aborted: server did not stop");
            server.publish(ServerEvent::DaemonMessage(format!("(restoring): failed: {e}")));
            server.publish(ServerEvent::BackupRestoreCompleted(
                serde_json::json!({
                    "uuid": backup_uuid,
                    "successful": false,
                })
                .to_string(),
            ));
            let _ = server
                .panel
                .read()
                .await
                .post_backup_restore_status(backup_uuid, false)
                .await;
            return;
        }
    }
    server.set_restoring(true);

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

    server.set_restoring(false);

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
    let meta = match std::fs::symlink_metadata(&archive) {
        Ok(m) => m,
        Err(_) => return Err(AppError::ServerNotFound),
    };
    if meta.is_dir() {
        return Err(AppError::BadRequest("invalid archive, is directory".into()));
    }
    // Wings treats a backup that vanished between locate and delete as a
    // success, so ignore NotFound here.
    match std::fs::remove_file(&archive) {
        Ok(_) => Ok(StatusCode::NO_CONTENT.into_response()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StatusCode::NO_CONTENT.into_response()),
        Err(e) => Err(AppError::BadRequest(format!("cannot delete backup: {e}"))),
    }
}
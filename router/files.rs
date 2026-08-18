use axum::body::Bytes;
use axum::extract::Query;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put, delete};
use axum::Json;
use axum::Router;

use serde::Deserialize;
use serde_json::json;
use std::net::IpAddr;
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::router::middleware::ServerExtractor;

use crate::state::DaemonState;

pub fn router() -> Router<DaemonState> {
    Router::new()
        .route(
            "/api/servers/:server/files/list-directory",
            get(list_directory),
        )
        .route("/api/servers/:server/files/contents", get(read_contents))
        .route("/api/servers/:server/files/rename", put(rename_files))
        .route("/api/servers/:server/files/copy", post(copy_file))
        .route("/api/servers/:server/files/write", post(write_file))
        .route(
            "/api/servers/:server/files/create-directory",
            post(create_directory),
        )
        .route("/api/servers/:server/files/delete", post(delete_files))
        .route("/api/servers/:server/files/compress", post(compress))
        .route("/api/servers/:server/files/decompress", post(decompress))
        .route("/api/servers/:server/files/chmod", post(chmod))
        .route(
            "/api/servers/:server/files/pull",
            get(pull_status).post(post_pull),
        )
        .route(
            "/api/servers/:server/files/pull/:download",
            delete(cancel_pull),
        )
}

#[derive(Debug, Deserialize)]
struct DirectoryQuery {
    directory: Option<String>,
}

async fn list_directory(
    server: ServerExtractor,
    Query(query): Query<DirectoryQuery>,
) -> AppResult<JsonValue> {
    let directory = query.directory.unwrap_or_default();
    let files = server.fs.list_directory(&directory)?;
    Ok(JsonValue(json!(files)))
}

#[derive(Debug, Deserialize)]
struct ContentsQuery {
    file: String,
    #[serde(default)]
    download: Option<String>,
}

/// GET /api/servers/:id/files/contents?file=path  (raw bytes)
async fn read_contents(
    server: ServerExtractor,
    Query(query): Query<ContentsQuery>,
) -> AppResult<Response> {
    let bytes = server.fs.read(&query.file)?;
    let filename = query
        .file
        .rsplit('/')
        .next()
        .unwrap_or("file")
        .to_string();

    let mut resp = bytes.into_response();
    let mime = mime_for_name(&filename);
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(mime),
    );
    resp.headers_mut().insert("X-Mime-Type", HeaderValue::from_static(mime));
    if query.download.is_some() {
        resp.headers_mut().insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
                .unwrap_or(HeaderValue::from_static("attachment")),
        );
    }
    Ok(resp)
}

#[derive(Debug, Deserialize)]
struct RenameRequest {
    root: String,
    files: Vec<RenameFile>,
}

#[derive(Debug, Deserialize)]
struct RenameFile {
    to: String,
    from: String,
}

async fn rename_files(
    server: ServerExtractor,
    Json(payload): Json<RenameRequest>,
) -> AppResult<Response> {
    let pairs: Vec<(String, String)> = payload
        .files
        .into_iter()
        .map(|f| (f.to, f.from))
        .collect();
    server.fs.rename(&payload.root, &pairs)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize)]
struct LocationRequest {
    location: String,
}

async fn copy_file(
    server: ServerExtractor,
    Json(payload): Json<LocationRequest>,
) -> AppResult<Response> {
    let stat = server.fs.copy(&payload.location)?;
    Ok(JsonValue(json!(stat)).into_response())
}

#[derive(Debug, Deserialize)]
struct WriteQuery {
    file: String,
}

/// POST /api/servers/:id/files/write?file=path  (raw body = file content)
async fn write_file(
    server: ServerExtractor,
    Query(query): Query<WriteQuery>,
    body: Bytes,
) -> AppResult<Response> {
    server.fs.write(&query.file, &body)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize)]
struct CreateDirectoryRequest {
    name: String,
    path: String,
}

async fn create_directory(
    server: ServerExtractor,
    Json(payload): Json<CreateDirectoryRequest>,
) -> AppResult<Response> {
    server.fs.create_directory(&payload.name, &payload.path)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize)]
struct FilesRequest {
    root: String,
    files: Vec<String>,
}

async fn delete_files(
    server: ServerExtractor,
    Json(payload): Json<FilesRequest>,
) -> AppResult<Response> {
    server.fs.delete(&payload.root, &payload.files)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn compress(
    server: ServerExtractor,
    Json(payload): Json<FilesRequest>,
) -> AppResult<JsonValue> {
    let stat = server.fs.compress(&payload.root, &payload.files)?;
    Ok(JsonValue(json!(stat)))
}

#[derive(Debug, Deserialize)]
struct DecompressRequest {
    root: String,
    file: String,
}

async fn decompress(
    server: ServerExtractor,
    Json(payload): Json<DecompressRequest>,
) -> AppResult<Response> {
    server.fs.decompress(&payload.root, &payload.file)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize)]
struct ChmodFile {
    file: String,
    mode: String,
}

#[derive(Debug, Deserialize)]
struct ChmodRequest {
    root: String,
    files: Vec<ChmodFile>,
}

async fn chmod(
    server: ServerExtractor,
    Json(payload): Json<ChmodRequest>,
) -> AppResult<Response> {
    let pairs: Vec<(String, u32)> = payload
        .files
        .into_iter()
        .map(|f| {
            let trimmed = if f.mode.len() > 1 {
                f.mode.trim_start_matches('0')
            } else {
                &f.mode
            };
            let mode = u32::from_str_radix(trimmed, 8)
                .map_err(|_| AppError::BadRequest(format!("invalid mode: {}", f.mode)))?;
            Ok((f.file, mode))
        })
        .collect::<AppResult<Vec<_>>>()?;
    server.fs.chmod(&payload.root, &pairs)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---- pull (remote downloads) ----------------------------------------------

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PullRequest {
    root: Option<String>,
    // legacy key
    directory: Option<String>,
    url: String,
    file_name: Option<String>,
    #[serde(default)]
    use_header: bool,
    #[serde(default)]
    foreground: bool,
}

async fn pull_status(
    server: ServerExtractor,
) -> Json<serde_json::Value> {
    let downloads: Vec<super::downloader::DownloadInfo> = super::downloader::by_server(server.uuid)
        .await
        .into_iter()
        .map(|d| super::downloader::DownloadInfo {
            identifier: d.identifier.clone(),
            progress: d.progress(),
        })
        .collect();
    Json(serde_json::json!({ "downloads": downloads }))
}

async fn cancel_pull(
    server: ServerExtractor,
    axum::extract::Path((_server_uuid, download)): axum::extract::Path<(String, String)>,
) -> AppResult<Response> {
    if let Some(dl) = super::downloader::by_id(&download).await {
        if dl.server_uuid == server.uuid {
            dl.cancel();
            super::downloader::untrack(&download).await;
        }
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// POST /api/servers/:id/files/pull — download a URL into the server.
/// Mirrors wings router_server_files.go postServerPullRemoteFile: max 3
/// concurrent downloads per server, SSRF guard, foreground/background mode.
async fn post_pull(
    server: ServerExtractor,
    Json(payload): Json<PullRequest>,
) -> AppResult<Response> {
    let daemon_cfg = server.daemon.read().await.clone();
    if daemon_cfg.api.disable_remote_download {
        return Err(AppError::Forbidden("remote downloads have been disabled by the daemon administrator".into()));
    }

    let directory = payload.root.or(payload.directory).unwrap_or_default();
    let file_name = payload
        .file_name
        .unwrap_or_else(|| "download".to_string());

    if file_name.contains("..") || file_name.contains('/') {
        return Err(AppError::BadRequest("invalid file name".into()));
    }

    // Mirror wings: check disk space before starting the download.
    if !server.has_space_available().await {
        return Err(AppError::Internal(anyhow::anyhow!(
            "not enough disk space to download this file"
        )));
    }

    // Do not allow more than three simultaneous remote file downloads at once.
    if super::downloader::count_for_server(server.uuid).await >= super::downloader::MAX_PER_SERVER {
        return Err(AppError::BadRequest(
            "This server has reached its limit of 3 simultaneous remote file downloads at once. Please wait for one to complete before trying again.".into(),
        ));
    }

    validate_download_url(&payload.url).await?;

    // Download into a temp file, then move into place (atomic-ish).
    let dest = server.fs.resolve(&directory)?.join(&file_name);
    server.fs.assert_contained(&dest)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AppError::BadRequest(format!("cannot create directory: {e}")))?;
    }

    let dl = super::downloader::track(
        server.uuid,
        payload.url.clone(),
        file_name.clone(),
        directory.clone(),
    )
    .await;
    tracing::info!(
        server = %server.uuid,
        download_id = %dl.identifier,
        file = %dl.file_name,
        dir = %dl.directory,
        "starting pull of remote file to disk"
    );

    let task_server = server.0.clone();
    let dl_clone = dl.clone();
    let url_clone = payload.url.clone();
    let file_name_clone = file_name.clone();
    let dest_clone = dest.clone();
    let run = async move {
        let result = run_download(&dest_clone, &url_clone, &file_name_clone, &dl_clone)
            .await;
        let _ = super::downloader::untrack(&dl_clone.identifier);
        result
    };

    if payload.foreground {
        let result = run.await;
        match result {
            Ok(()) => {
                let stat = task_server.fs.stat(&dest)?;
                Ok(JsonValue(json!(stat)).into_response())
            }
            Err(e) => Err(e),
        }
    } else {
        let url = dl.url.clone();
        let identifier = dl.identifier.clone();
        tokio::spawn(async move {
            if let Err(e) = run.await {
                tracing::warn!(url = %url, error = %e, "background pull failed");
            }
        });
        Ok((StatusCode::ACCEPTED, Json(serde_json::json!({ "identifier": identifier }))).into_response())
    }
}

/// Perform the actual HTTP download to `dest`, tracking progress on `dl`.
async fn run_download(
    dest: &std::path::Path,
    url: &str,
    file_name: &str,
    dl: &Arc<super::downloader::Download>,
) -> AppResult<()> {
    use tokio::io::AsyncWriteExt;
    let cancel = dl.child_token();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        // Never follow redirects: an attacker-controlled server could
        // redirect the download to a private/local address, bypassing the
        // SSRF guard in validate_download_url (wings rejects redirects
        // to private ranges on each hop; we reject all redirects).
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| AppError::BadRequest(format!("download failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(AppError::BadRequest(format!("remote returned {}", resp.status())));
    }
    let total = resp.content_length().unwrap_or(0);

    let mut out = {
        use nix::fcntl::OFlag;
        use std::os::unix::fs::OpenOptionsExt;
        // O_NOFOLLOW: never write through a planted symlink.
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC).bits());
        tokio::fs::File::from_std(
            opts.open(dest)
                .map_err(|e| AppError::BadRequest(format!("cannot create {file_name}: {e}")))?,
        )
    };
    let mut written: u64 = 0;
    while let Some(chunk) = resp.chunk().await.map_err(|e| AppError::BadRequest(format!("download interrupted: {e}")))? {
        if cancel.is_cancelled() {
            drop(out);
            let _ = std::fs::remove_file(dest);
            return Err(AppError::BadRequest("download cancelled".into()));
        }
        use tokio::io::AsyncWriteExt;
        out.write_all(&chunk)
            .await
            .map_err(|e| AppError::BadRequest(format!("write failed: {e}")))?;
        written += chunk.len() as u64;
        if total > 0 {
            dl.set_progress(written as f64 / total as f64);
        } else {
            dl.set_progress(1.0);
        }
    }
    out.flush().await.map_err(|e| AppError::BadRequest(format!("flush failed: {e}")))?;
    drop(out);
    dl.set_progress(1.0);
    Ok(())
}

/// Blocks private/loopback/link-local targets (SSRF). Never accepts non
/// http(s) schemes.
async fn validate_download_url(url: &str) -> AppResult<()> {
    let parsed = url::Url::parse(url).map_err(|_| AppError::BadRequest("invalid URL".into()))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(AppError::BadRequest(format!("unsupported URL scheme: {other}")));
        }
    }

    let host = parsed.host_str().ok_or_else(|| AppError::BadRequest("URL has no host".into()))?;
    // Resolve IP literals directly; DNS names are resolved here.
    let ips = tokio::net::lookup_host((host, parsed.port_or_known_default().unwrap_or(80)))
        .await
        .map_err(|_| AppError::BadRequest("cannot resolve host".into()))?;

    for ip in ips {
        if blocklisted(&ip.ip()) {
            return Err(AppError::Forbidden("the requested URL points to a private or local network address".into()));
        }
    }
    Ok(())
}

fn blocklisted(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || (v4.octets()[0] == 100 && v4.octets()[1] >> 6 == 1) // 100.64.0.0/10 CGNAT
                || (v4.octets()[0] == 198 && (18..=19).contains(&v4.octets()[1])) // 198.18/15
                || (v4.octets()[0] == 169 && v4.octets()[1] == 254)
        }
        IpAddr::V6(v6) => {
            // ::ffff:a.b.c.d is an IPv4-mapped address — evaluate as IPv4.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return blocklisted(&IpAddr::V4(v4));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        }
    }
}

fn mime_for_name(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "txt" | "log" | "properties" => "text/plain",
        "json" => "application/json",
        "yml" | "yaml" => "text/yaml",
        "xml" => "application/xml",
        "html" => "text/html",
        "sh" => "text/x-sh",
        "md" => "text/markdown",
        "jar" => "application/java-archive",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Json wrapper to keep handlers tidy.
pub struct JsonValue(pub serde_json::Value);
impl IntoResponse for JsonValue {
    fn into_response(self) -> Response {
        Json(self.0).into_response()
    }
}
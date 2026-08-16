use axum::body::Bytes;
use axum::extract::Query;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::Json;
use axum::Router;

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::IpAddr;

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
            get(pull_status).post(post_pull).delete(cancel_pull),
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
            let mode = u32::from_str_radix(f.mode.trim_start_matches('0'), 8)
                .map_err(|_| AppError::BadRequest(format!("invalid mode: {}", f.mode)))
                .ok()
                .unwrap_or(0);
            (f.file, mode)
        })
        .collect();
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

#[derive(Debug, Serialize)]
struct PullStatusResponse {
    downloads: Vec<serde_json::Value>,
}

async fn pull_status() -> Json<PullStatusResponse> {
    Json(PullStatusResponse { downloads: vec![] })
}

async fn cancel_pull() -> AppResult<Response> {
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// POST /api/servers/:id/files/pull — download a URL into the server.
/// Includes the SSRF guard wings has (no private/loopback targets).
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

    validate_download_url(&payload.url).await?;

    // Download into a temp file, then move into place (atomic-ish).
    let dest = server.fs.resolve(&directory)?.join(&file_name);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AppError::BadRequest(format!("cannot create directory: {e}")))?;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;
    let mut resp = client
        .get(&payload.url)
        .send()
        .await
        .map_err(|e| AppError::BadRequest(format!("download failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(AppError::BadRequest(format!("remote returned {}", resp.status())));
    }

    let mut out = std::fs::File::create(&dest)
        .map_err(|e| AppError::BadRequest(format!("cannot create {file_name}: {e}")))?;
    while let Some(chunk) = resp.chunk().await.map_err(|e| AppError::BadRequest(format!("download interrupted: {e}")))? {
        use std::io::Write;
        out.write_all(&chunk)
            .map_err(|e| AppError::BadRequest(format!("write failed: {e}")))?;
    }

    let stat = server.fs.stat(&dest)?;
    Ok(JsonValue(json!(stat)).into_response())
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
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
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
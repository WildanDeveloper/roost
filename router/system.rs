use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{routing::{get, post}, Json, Router};

use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::remote::PanelClient;
use crate::state::DaemonState;

pub fn router() -> Router<DaemonState> {
    Router::new()
        .route("/api/system", get(get_system_information))
        .route("/api/test/:x/ping", get(|| async { "ptest" }))
        .route("/api/update", post(post_update_configuration))
        .route("/api/servers", get(get_all_servers).post(post_create_server))
}

/// GET /api/system
#[derive(Debug, Serialize)]
struct SystemInformation {
    architecture: String,
    cpu_count: u64,
    kernel_version: String,
    os: String,
    version: String,
}

async fn get_system_information(
    State(state): State<DaemonState>,
    Query(query): Query<SystemQuery>,
) -> Json<serde_json::Value> {
    let uname = nix::sys::utsname::uname().ok();
    if query.v == Some(2) {
        return Json(system_information_v2(&state).await);
    }
    Json(serde_json::to_value(SystemInformation {
        architecture: std::env::consts::ARCH.to_string(),
        cpu_count: std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(1),
        kernel_version: uname.map(|u| u.release().to_string_lossy().to_string()).unwrap_or_default(),
        os: std::env::consts::OS.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }).unwrap_or_default())
}

#[derive(Debug, Deserialize)]
struct SystemQuery {
    v: Option<u8>,
}

/// `?v=2` full system information (wings system/system.go). Used by the
/// panel telemetry service.
async fn system_information_v2(state: &DaemonState) -> serde_json::Value {
    let mut docker = serde_json::json!({
        "version": "",
        "cgroups": {"driver": "", "version": ""},
        "containers": {"total": 0, "running": 0, "paused": 0, "stopped": 0},
        "storage": {"driver": "", "filesystem": ""},
        "runc": {"version": ""},
    });
    let mut sys = serde_json::json!({
        "architecture": std::env::consts::ARCH,
        "cpu_threads": std::thread::available_parallelism().map(|n| n.get() as i64).unwrap_or(1),
        "memory_bytes": 0,
        "kernel_version": nix::sys::utsname::uname().map(|u| u.release().to_string_lossy().to_string()).unwrap_or_default(),
        "os": os_release_name(),
        "os_type": std::env::consts::OS,
    });

    if let Ok(info) = state.manager.shared().docker.engine_info().await {
        docker["containers"] = serde_json::json!({
            "total": info.containers.unwrap_or(0),
            "running": info.containers_running.unwrap_or(0),
            "paused": info.containers_paused.unwrap_or(0),
            "stopped": info.containers_stopped.unwrap_or(0),
        });
        if let Some(driver) = &info.driver {
            docker["storage"]["driver"] = serde_json::json!(driver);
        }
        for row in info.driver_status.unwrap_or_default() {
            if row.first().map(|s| s.as_str()) == Some("Backing Filesystem") {
                if let Some(v) = row.get(1) {
                    docker["storage"]["filesystem"] = serde_json::json!(v);
                }
            }
        }
        docker["cgroups"]["driver"] = serde_json::json!(
            info.cgroup_driver.map(|d| d.to_string()).unwrap_or_default()
        );
        docker["cgroups"]["version"] = serde_json::json!(
            info.cgroup_version.map(|v| v.to_string()).unwrap_or_default()
        );
        if let Some(mem) = info.mem_total {
            sys["memory_bytes"] = serde_json::json!(mem);
        }
    }

    if let Ok(version) = state.manager.shared().docker.engine_version().await {
        if let Some(v) = &version.version {
            docker["version"] = serde_json::json!(v);
        }
        for component in version.components.unwrap_or_default() {
            if component.name == "runc" {
                docker["runc"]["version"] = serde_json::json!(component.version);
            }
        }
    }

    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "docker": docker,
        "system": sys,
    })
}

/// Pretty name from /etc/os-release (fallback NAME, then docker's).
fn os_release_name() -> String {
    if let Ok(content) = std::fs::read_to_string("/etc/os-release") {
        for line in content.lines() {
            if let Some(v) = line.strip_prefix("PRETTY_NAME=").or_else(|| line.strip_prefix("NAME=")) {
                return v.trim_matches('"').to_string();
            }
        }
    }
    std::env::consts::OS.to_string()
}

/// POST /api/update — the panel pushes a full config.yml replacement.
#[derive(Debug, Deserialize)]
struct UpdatePayload {
    #[serde(flatten)]
    values: serde_json::Map<String, serde_json::Value>,
}

async fn post_update_configuration(
    State(state): State<DaemonState>,
    Json(payload): Json<UpdatePayload>,
) -> AppResult<Json<serde_json::Value>> {
    if state.config.read().await.ignore_panel_config_updates {
        tracing::warn!("ignoring config update from panel (ignore_panel_config_updates)");
        return Ok(Json(json!({ "applied": false })));
    }

    // Parse into the config struct to validate.
    let value = serde_json::Value::Object(payload.values);
    let mut new_config: Config = serde_json::from_value(value)
        .map_err(|e| AppError::BadRequest(format!("invalid config payload: {e}")))?;

    // Wings: keep the SSL certificate paths when the panel passes through
    // the LetsEncrypt defaults, so manual locations are not overwritten.
    {
        let existing = state.config.read().await;
        if new_config.api.ssl.key.starts_with("/etc/letsencrypt/live/") {
            new_config.api.ssl.key = existing.api.ssl.key.clone();
        }
        if new_config.api.ssl.cert.starts_with("/etc/letsencrypt/live/") {
            new_config.api.ssl.cert = existing.api.ssl.cert.clone();
        }
    }

    // Wings ResolveToken(remote=true): a panel push must not carry token
    // indirection, and environment overrides must match the local value.
    if new_config.token.contains('$') || new_config.token.starts_with("file://") {
        return Err(AppError::BadRequest(
            "config: remote token cannot use token indirection".into(),
        ));
    }
    if new_config.token_id.contains('$') || new_config.token_id.starts_with("file://") {
        return Err(AppError::BadRequest(
            "config: remote token ID cannot use token indirection".into(),
        ));
    }
    if let Ok(t) = std::env::var("WINGS_TOKEN") {
        if !t.is_empty() && t.trim() != new_config.token {
            return Err(AppError::BadRequest(
                "config: remote token does not match environment override".into(),
            ));
        }
    }
    if let Ok(t) = std::env::var("WINGS_TOKEN_ID") {
        if !t.is_empty() && t.trim() != new_config.token_id {
            return Err(AppError::BadRequest(
                "config: remote token ID does not match environment override".into(),
            ));
        }
    }

    // Refuse to apply a token we could never authenticate against.
    if new_config.token.is_empty() || new_config.token_id.is_empty() {
        return Err(AppError::BadRequest(
            "config: refusing to apply an update with an empty authentication token".into(),
        ));
    }

    // Keep the path we loaded from.
    let path = state.config.read().await.path.clone();

    // Persist as YAML with 0600 permissions (wings WriteToDisk) so the
    // daemon token is never world-readable.
    if let Some(path) = &path {
        new_config.path = Some(path.clone());
        let yaml = serde_yaml::to_string(&new_config)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot serialize config: {e}")))?;
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot open {}: {e}", path)))?;
            use std::io::Write;
            file.write_all(yaml.as_bytes())
                .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot write {}: {e}", path)))?;
            file.sync_all()
                .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot sync {}: {e}", path)))?;
        }
        tracing::info!(path = %path, "new configuration persisted");
    }

    // Rotate in-memory config + panel client credentials.
    {
        let mut guard = state.config.write().await;
        *guard = new_config.clone();
    }

    let panel = PanelClient::new(&new_config.remote, &new_config.token_id, &new_config.token)?;
    *state.panel.write().await = panel;

    tracing::info!("configuration updated from panel; token rotated");
    Ok(Json(json!({ "applied": true })))
}

/// GET /api/servers — all servers as an array of API responses.
async fn get_all_servers(State(state): State<DaemonState>) -> Json<Vec<serde_json::Value>> {
    let servers = state.manager.list().await;
    Json(servers.iter().map(|s| serde_json::to_value(s.api_response()).unwrap_or_default()).collect())
}

/// POST /api/servers — {"uuid": "...", "start_on_completion": bool}
/// The full config is fetched from the panel; the install runs async.
#[derive(Debug, Deserialize)]
struct CreateServerRequest {
    uuid: Uuid,
    #[serde(default)]
    start_on_completion: bool,
}

async fn post_create_server(
    State(state): State<DaemonState>,
    Json(payload): Json<CreateServerRequest>,
) -> AppResult<Response> {
    state
        .manager
        .create_remote(payload.uuid, payload.start_on_completion)
        .await?;
    Ok(StatusCode::ACCEPTED.into_response())
}
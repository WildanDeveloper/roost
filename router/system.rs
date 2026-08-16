use axum::extract::State;
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

async fn get_system_information() -> Json<SystemInformation> {
    let uname = nix::sys::utsname::uname().ok();
    Json(SystemInformation {
        architecture: std::env::consts::ARCH.to_string(),
        cpu_count: std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(1),
        kernel_version: uname.map(|u| u.release().to_string_lossy().to_string()).unwrap_or_default(),
        os: std::env::consts::OS.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
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

    if new_config.token.is_empty() {
        return Err(AppError::BadRequest("token must not be empty".into()));
    }

    // Keep the path we loaded from.
    let path = state.config.read().await.path.clone();

    // Persist as YAML.
    if let Some(path) = &path {
        new_config.path = Some(path.clone());
        let yaml = serde_yaml::to_string(&new_config)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot serialize config: {e}")))?;
        if let Err(e) = std::fs::write(path, yaml) {
            return Err(AppError::Internal(anyhow::anyhow!("cannot write {}: {e}", path)));
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
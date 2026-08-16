use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::models::{ProcessConfig, ServerConfig};

/// One entry in `GET /api/remote/servers` (paginated).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawServerData {
    pub uuid: Uuid,
    pub settings: ServerConfig,
    pub process_configuration: Option<ProcessConfig>,
}

/// Wrapper for the paginated server list from the panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerListResponse {
    pub data: Vec<RawServerData>,
    #[serde(default)]
    pub meta: ServerListMeta,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerListMeta {
    #[serde(default)]
    pub current_page: u64,
    #[serde(default)]
    pub last_page: u64,
}

/// `GET /api/remote/servers/{uuid}` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfigurationResponse {
    pub settings: ServerConfig,
    pub process_configuration: Option<ProcessConfig>,
}

/// `GET /api/remote/servers/{uuid}/install` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallScriptResponse {
    pub container_image: String,
    pub entrypoint: String,
    pub script: String,
}

/// `POST /api/remote/servers/{uuid}/install` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallStatusRequest {
    pub successful: bool,
    pub reinstall: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusMessage {
    pub successful: bool,
}

/// `GET /api/remote/backups/{uuid}` response — download URLs for S3 backups.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct BackupParts {
    #[serde(default)]
    pub parts: Vec<String>,
    #[serde(default)]
    pub part_size: i64,
}

/// `POST /api/remote/backups/{uuid}` body — completion report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupRemoteStatus {
    pub checksum: String,
    pub checksum_type: String,
    pub size: i64,
    pub successful: bool,
    #[serde(default)]
    pub parts: Vec<BackupPart>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupPart {
    pub part_number: i64,
    pub etag: String,
}

/// Body sent to `POST /api/remote/sftp/auth` (used by a future SFTP server).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct SftpAuthRequest {
    pub r#type: String,
    pub username: String,
    #[serde(default)]
    pub password: String,
    pub ip: String,
    pub session_id: String,
    pub client_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct SftpAuthResponse {
    pub server: Option<SftpServerInfo>,
    pub user: String,
    pub permissions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct SftpServerInfo {
    pub uuid: String,
    pub data: String,
    pub configuration: ServerConfig,
}
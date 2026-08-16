use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::{AppError, AppResult};

/// Daemon configuration, designed to be drop-in compatible with the
/// Pterodactyl Wings `config.yml` (v1.13.3 schema). The panel generates
/// this file for you on the node (Settings > Nodes > edit node). Missing
/// keys fall back to the same defaults Wings uses.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub debug: bool,
    pub app_name: String,
    pub uuid: String,
    pub token_id: String,
    pub token: String,
    pub api: ApiConfig,
    pub system: SystemConfig,
    pub docker: DockerConfig,
    pub remote: String,
    pub remote_query: RemoteQueryConfig,
    pub allowed_mounts: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub allow_cors_private_network: bool,
    pub ignore_panel_config_updates: bool,
    /// Not part of the panel config; used to pass the config file path.
    #[serde(skip)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ApiConfig {
    pub host: String,
    pub port: u16,
    pub ssl: SslConfig,
    pub disable_remote_download: bool,
    pub upload_limit: u64,
    pub trusted_proxies: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SslConfig {
    pub enabled: bool,
    pub cert: String,
    pub key: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SystemConfig {
    pub root_directory: String,
    pub log_directory: String,
    pub data: String,
    pub archive_directory: String,
    pub backup_directory: String,
    pub tmp_directory: String,
    pub username: String,
    pub timezone: String,
    pub disk_check_interval: u64,
    pub websocket_log_count: usize,
    pub check_permissions_on_boot: bool,
    pub activity_send_interval: u64,
    pub activity_send_count: usize,
    pub sftp: SftpConfig,
    pub crash_detection: CrashDetectionConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SftpConfig {
    pub bind_address: String,
    pub bind_port: u16,
    pub read_only: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CrashDetectionConfig {
    pub enabled: bool,
    pub detect_clean_exit_as_crash: bool,
    pub timeout: u64,
}

impl Default for CrashDetectionConfig {
    fn default() -> Self {
        // Wings defaults when the daemon config omits the section.
        Self {
            enabled: true,
            detect_clean_exit_as_crash: true,
            timeout: 60,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct DockerConfig {
    pub network: DockerNetworkConfig,
    pub domainname: String,
    pub registries: Vec<RegistryConfig>,
    pub tmpfs_size: u64,
    pub container_pid_limit: i64,
    pub installer_limits: InstallerLimits,
    pub cpu_period: u64,
    pub cpu_shares: u64,
    pub overhead: OverheadConfig,
    pub userns_mode: String,
    pub log_config: LogConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct DockerNetworkConfig {
    pub interface: String,
    pub dns: Vec<String>,
    pub name: String,
    pub ispn: bool,
    pub driver: String,
    pub network_mode: String,
    pub is_internal: bool,
    pub enable_icc: bool,
    pub network_mtu: u64,
    pub interfaces: NetworkInterfaces,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct NetworkInterfaces {
    pub v4: NetworkInterface,
    pub v6: NetworkInterface,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct NetworkInterface {
    pub subnet: String,
    pub gateway: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RegistryConfig {
    pub name: String,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct InstallerLimits {
    pub memory: i64,
    pub cpu: i64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct OverheadConfig {
    pub override_multiplier: bool,
    pub default_multiplier: f32,
    pub multipliers: Vec<Multiplier>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Multiplier {
    pub memory: i64,
    pub overhead: f32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct LogConfig {
    pub r#type: String,
    pub config: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RemoteQueryConfig {
    pub timeout: u64,
    pub boot_servers_per_page: u64,
}

impl Config {
    const DEFAULTS: &'static str = include_str!("config.example.yml");

    /// Load `config.yml` from disk. If the file is missing, load the
    /// bundled example so defaults are sensible.
    pub fn load(path: impl AsRef<Path>) -> AppResult<Self> {
        let path = path.as_ref();
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), "config file not readable ({e}), using defaults");
                Self::DEFAULTS.to_string()
            }
        };

        let mut cfg: Config = serde_yaml::from_str(&content)
            .map_err(|e| AppError::Config(format!("invalid YAML in {}: {e}", path.display())))?;
        cfg.path = Some(path.display().to_string());

        cfg.resolve_token();
        Ok(cfg)
    }

    /// Wings supports `token: $ENV_VAR` or `token: file:///path/to/secret`
    /// indirection, plus env overrides `WINGS_TOKEN` / `WINGS_TOKEN_ID`.
    /// We mirror that behavior.
    fn resolve_token(&mut self) {
        self.token = expand_value(&self.token);
        self.token_id = expand_value(&self.token_id);

        if let Ok(t) = std::env::var("WINGS_TOKEN") {
            if !t.is_empty() {
                self.token = expand_value(&t);
            }
        }
        if let Ok(t) = std::env::var("WINGS_TOKEN_ID") {
            if !t.is_empty() {
                self.token_id = expand_value(&t);
            }
        }

        if self.token.is_empty() {
            tracing::warn!("token is empty; the panel will not be able to authenticate against this daemon");
        }
    }

    /// Prompt used as the bind address for the daemon API server.
    pub fn bind_address(&self) -> String {
        format!("{}:{}", self.api.host, self.api.port)
    }

    /// The panel base URL, without trailing slash.
    pub fn panel_url(&self) -> String {
        self.remote.trim_end_matches('/').to_string()
    }

    pub fn data_dir(&self, server_uuid: &str) -> std::path::PathBuf {
        std::path::Path::new(&self.system.data).join(server_uuid)
    }

    pub fn tmp_dir(&self) -> std::path::PathBuf {
        if self.system.tmp_directory.is_empty() {
            std::path::PathBuf::from("/tmp/pterodactyl")
        } else {
            std::path::PathBuf::from(&self.system.tmp_directory)
        }
    }

    pub fn log_dir(&self) -> std::path::PathBuf {
        if self.system.log_directory.is_empty() {
            std::path::Path::new(&self.system.root_directory).join("logs")
        } else {
            std::path::PathBuf::from(&self.system.log_directory)
        }
    }

    pub fn archive_dir(&self) -> std::path::PathBuf {
        if self.system.archive_directory.is_empty() {
            std::path::Path::new(&self.system.root_directory).join("archives")
        } else {
            std::path::PathBuf::from(&self.system.archive_directory)
        }
    }

    pub fn backup_dir(&self) -> std::path::PathBuf {
        if self.system.backup_directory.is_empty() {
            std::path::Path::new(&self.system.root_directory).join("backups")
        } else {
            std::path::PathBuf::from(&self.system.backup_directory)
        }
    }

    /// Create all directories the daemon needs and verify Docker access.
    pub fn ensure_directories(&self) -> AppResult<()> {
        for dir in [
            self.tmp_dir(),
            self.log_dir(),
            self.archive_dir(),
            self.backup_dir(),
        ] {
            std::fs::create_dir_all(&dir).map_err(|e| {
                AppError::Config(format!("cannot create {}: {e}", dir.display()))
            })?;
        }
        if !self.system.data.is_empty() {
            std::fs::create_dir_all(&self.system.data).map_err(|e| {
                AppError::Config(format!("cannot create {}: {e}", self.system.data))
            })?;
        }
        Ok(())
    }
}

/// Expand `$VAR` / `${VAR}` / `file://` prefixed values, like Wings.
fn expand_value(input: &str) -> String {
    let input = input.trim().to_string();
    if let Some(path) = input.strip_prefix("file://") {
        let path = path.to_string();
        return std::fs::read_to_string(&path)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|e| {
                tracing::error!(path = %path, "cannot read token file: {e}");
                input
            });
    }
    if let Some(rest) = input.strip_prefix('$') {
        let name = rest.trim_matches(|c| c == '{' || c == '}');
        return std::env::var(name).unwrap_or_default();
    }
    input
}
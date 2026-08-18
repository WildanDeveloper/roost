use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::{AppError, AppResult};

fn default_true() -> bool { true }

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
    #[serde(default)]
    pub throttles: ConsoleThrottles,
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
    pub enable_log_rotate: bool,
    pub openat_mode: String,
    pub activity_send_interval: u64,
    pub activity_send_count: usize,
    pub user: UserConfig,
    #[serde(default)]
    pub passwd: PasswdConfig,
    #[serde(default)]
    pub machine_id: MachineIdConfig,
    #[serde(default)]
    pub backups: BackupsConfig,
    #[serde(default)]
    pub transfers: TransfersConfig,
    pub sftp: SftpConfig,
    pub crash_detection: CrashDetectionConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SftpConfig {
    pub bind_address: String,
    pub bind_port: u16,
    pub read_only: bool,
}

impl Default for SftpConfig {
    fn default() -> Self {
        // Wings defaults when the daemon config omits the sftp section.
        Self {
            bind_address: "0.0.0.0".to_string(),
            bind_port: 2022,
            read_only: false,
        }
    }
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ConsoleThrottles {
    pub enabled: bool,
    pub lines: u64,
    pub line_reset_interval: u64,
}

impl Default for ConsoleThrottles {
    fn default() -> Self {
        Self {
            enabled: true,
            lines: 2000,
            line_reset_interval: 100,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct UserConfig {
    pub uid: i64,
    pub gid: i64,
    pub rootless: RootlessConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RootlessConfig {
    pub enabled: bool,
    pub container_uid: i64,
    pub container_gid: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PasswdConfig {
    pub enabled: bool,
    pub directory: String,
}

impl Default for PasswdConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            directory: "/run/wings/etc".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct MachineIdConfig {
    pub enabled: bool,
    pub directory: String,
}

impl Default for MachineIdConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: "/run/wings/machine-id".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct BackupsConfig {
    pub write_limit: i64,
    pub compression_level: String,
    pub restore_host_allowlist: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct TransfersConfig {
    pub download_limit: i64,
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
    #[serde(default)]
    pub cpu_burst: CpuBurstConfig,
    pub cpu_shares: u64,
    pub overhead: OverheadConfig,
    #[serde(default = "default_true")]
    pub use_performant_inspect: bool,
    pub userns_mode: String,
    pub log_config: LogConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CpuBurstConfig {
    pub enabled: bool,
    pub percent: i64,
}

impl Default for CpuBurstConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            percent: 100,
        }
    }
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

    /// Write a logrotate configuration for the daemon log file, mirroring
    /// wings EnableLogRotation: only when enabled, /etc/logrotate.d exists
    /// and a config for the daemon is not already present.
    pub fn enable_log_rotation(&self) -> AppResult<()> {
        if !self.system.enable_log_rotate {
            return Ok(());
        }
        let logrotate_dir = std::path::Path::new("/etc/logrotate.d");
        let Ok(meta) = std::fs::metadata(logrotate_dir) else {
            return Ok(());
        };
        if !meta.is_dir() {
            return Ok(());
        }
        let conf = logrotate_dir.join("roost");
        if std::fs::metadata(&conf).is_ok() {
            return Ok(());
        }
        let log_file = self.log_dir().join("roost.log");
        let contents = format!(
            "{} {{\n    size 10M\n    compress\n    delaycompress\n    dateext\n    maxage 7\n    missingok\n    notifempty\n    postrotate\n        /usr/bin/systemctl kill -s HUP roost.service >/dev/null 2>&1 || true\n    endscript\n}}\n",
            log_file.display()
        );
        std::fs::write(&conf, contents).map_err(|e| {
            AppError::Config(format!("failed to write logrotate config: {e}"))
        })?;
        tracing::info!("no log rotation configuration found: added /etc/logrotate.d/roost");
        Ok(())
    }

    /// Create all directories the daemon needs and verify Docker access.
    /// Directories are created with 0700 permissions like wings
    /// (os.MkdirAll with 0o700).
    pub fn ensure_directories(&self) -> AppResult<()> {
        use std::os::unix::fs::DirBuilderExt;
        for dir in [
            self.tmp_dir(),
            self.log_dir(),
            self.archive_dir(),
            self.backup_dir(),
        ] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .map_err(|e| AppError::Config(format!("cannot create {}: {e}", dir.display())))?;
        }
        if !self.system.data.is_empty() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&self.system.data)
                .map_err(|e| AppError::Config(format!("cannot create {}: {e}", self.system.data)))?;
        }
        // Wings ConfigurePasswd: generate /etc/{group,passwd} overrides for
        // containers when the feature is enabled.
        if self.system.passwd.enabled {
            std::fs::create_dir_all(&self.system.passwd.directory).map_err(|e| {
                AppError::Config(format!(
                    "cannot create {}: {e}",
                    self.system.passwd.directory
                ))
            })?;
            let group = format!(
                "root:x:0:\ncontainer:x:{}:\nnogroup:x:65534:\n",
                self.system.user.gid
            );
            let passwd = format!(
                "root:x:0:0::/root:/bin/sh\ncontainer:x:{}:{}::/home/container:/bin/sh\nnobody:x:65534:65534::/var/empty:/bin/sh\n",
                self.system.user.uid, self.system.user.gid
            );
            std::fs::write(
                std::path::Path::new(&self.system.passwd.directory).join("group"),
                group,
            )
            .map_err(|e| AppError::Config(format!("cannot write passwd group file: {e}")))?;
            std::fs::write(
                std::path::Path::new(&self.system.passwd.directory).join("passwd"),
                passwd,
            )
            .map_err(|e| AppError::Config(format!("cannot write passwd file: {e}")))?;
        }
        if self.system.machine_id.enabled {
            std::fs::create_dir_all(&self.system.machine_id.directory).map_err(|e| {
                AppError::Config(format!(
                    "cannot create {}: {e}",
                    self.system.machine_id.directory
                ))
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
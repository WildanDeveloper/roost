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
    #[serde(deserialize_with = "de_registries")]
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
    /// Wings YAML key is `override`; `override_multiplier` kept as alias.
    #[serde(rename = "override", alias = "override_multiplier")]
    pub override_multiplier: bool,
    pub default_multiplier: f32,
    #[serde(
        deserialize_with = "de_multipliers",
        serialize_with = "ser_multipliers"
    )]
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
    pub(crate) const DEFAULTS: &'static str = include_str!("config.example.yml");

    /// Load `config.yml` from disk. If the file is missing, load the
    /// bundled example so defaults are sensible.
    pub fn load(path: impl AsRef<Path>) -> AppResult<Self> {
        let path = path.as_ref();
        // Wings fails hard when the configuration file is missing or
        // unreadable (config.FromFile returns the error and cmd/root.go
        // aborts) — a silent default fallback would boot the daemon with a
        // known token.
        let content = std::fs::read_to_string(path).map_err(|e| {
            AppError::Config(format!("cannot read config file {}: {e}", path.display()))
        })?;

        let mut cfg: Config = serde_yaml::from_str(&content)
            .map_err(|e| AppError::Config(format!("invalid YAML in {}: {e}", path.display())))?;
        cfg.path = Some(path.display().to_string());

        cfg.resolve_token();
        Ok(cfg)
    }

    /// Wings supports `token: $ENV_VAR` or `token: file:///path/to/secret`
    /// indirection, plus env overrides `WINGS_TOKEN` / `WINGS_TOKEN_ID`.
    /// We mirror that behavior.
    pub(crate) fn resolve_token(&mut self) {
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

    /// Location of the cached server-state file used to restore servers
    /// after a daemon or machine restart (wings `GetStatesPath`).
    pub fn states_path(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.system.root_directory).join("states.json")
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
        // copytruncate: logrotate copies then truncates the active file, so
        // rotation works without the writer reopening (HUP does nothing for
        // tracing_appender's non_blocking writer).
        let contents = format!(
            "{} {{\n    size 10M\n    copytruncate\n    compress\n    delaycompress\n    dateext\n    maxage 7\n    missingok\n    notifempty\n}}\n",
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
        // `${VAR}` or `$VAR` — only valid identifier characters are part of
        // the variable name (`$A.B` must not swallow the `.B`).
        let name: String = match rest.strip_prefix('{') {
            Some(braced) => braced
                .split('}')
                .next()
                .unwrap_or_default()
                .to_string(),
            None => rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect(),
        };
        if name.is_empty() {
            return input;
        }
        return std::env::var(name).unwrap_or_default();
    }
    input
}
/// Accept `docker.overhead.multipliers` in both shapes: the wings map
/// (`<memory_mb>: <multiplier>`, keys may be ints or numeric strings) and
/// the sequence form (`[{memory, overhead}]`). Serialized back as a map
/// with sorted integer keys to match wings.
fn de_multipliers<'de, D>(deserializer: D) -> Result<Vec<Multiplier>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct MemoryKey(i64);

    impl<'de> serde::Deserialize<'de> for MemoryKey {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            struct V;
            impl<'de> serde::de::Visitor<'de> for V {
                type Value = MemoryKey;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("a memory limit in MB")
                }
                fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<MemoryKey, E> {
                    Ok(MemoryKey(v))
                }
                fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<MemoryKey, E> {
                    Ok(MemoryKey(v as i64))
                }
                fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<MemoryKey, E> {
                    v.parse::<i64>()
                        .map(MemoryKey)
                        .map_err(|_| E::invalid_value(serde::de::Unexpected::Str(v), &self))
                }
            }
            deserializer.deserialize_any(V)
        }
    }

    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = Vec<Multiplier>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a map or sequence of memory multipliers")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut access: A,
        ) -> Result<Vec<Multiplier>, A::Error> {
            let mut out = Vec::new();
            while let Some((MemoryKey(memory), overhead)) =
                access.next_entry::<MemoryKey, f32>()?
            {
                out.push(Multiplier { memory, overhead });
            }
            Ok(out)
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut access: A,
        ) -> Result<Vec<Multiplier>, A::Error> {
            let mut out = Vec::new();
            while let Some(m) = access.next_element::<Multiplier>()? {
                out.push(m);
            }
            Ok(out)
        }
    }
    deserializer.deserialize_any(V)
}

fn ser_multipliers<S: serde::Serializer>(
    multipliers: &[Multiplier],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut sorted: Vec<&Multiplier> = multipliers.iter().collect();
    sorted.sort_by_key(|m| m.memory);
    let mut map = serializer.serialize_map(Some(sorted.len()))?;
    for m in sorted {
        map.serialize_entry(&m.memory, &m.overhead)?;
    }
    map.end()
}

/// Accept `docker.registries` in both shapes the ecosystem produces:
/// the wings map (`"<host>": {username, password}`) and the plain
/// sequence form (`[{name, username, password}]`).
fn de_registries<'de, D>(deserializer: D) -> Result<Vec<RegistryConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Map(std::collections::HashMap<String, RegistryEntry>),
        Seq(Vec<RegistryConfig>),
    }

    #[derive(serde::Deserialize)]
    struct RegistryEntry {
        #[serde(default)]
        username: String,
        #[serde(default)]
        password: String,
    }

    match Raw::deserialize(deserializer)? {
        Raw::Seq(list) => Ok(list),
        Raw::Map(map) => Ok(map
            .into_iter()
            .map(|(name, entry)| RegistryConfig {
                name,
                username: entry.username,
                password: entry.password,
            })
            .collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registries_accept_wings_map_shape() {
        let y = r#"
docker:
  registries:
    docker.io:
      username: u1
      password: p1
    ghcr.io:
      username: u2
      password: p2
"#;
        let cfg: Config = serde_yaml::from_str(y).unwrap();
        assert_eq!(cfg.docker.registries.len(), 2);
        let dio = cfg
            .docker
            .registries
            .iter()
            .find(|r| r.name == "docker.io")
            .unwrap();
        assert_eq!(dio.username, "u1");
        assert_eq!(dio.password, "p1");
    }

    #[test]
    fn registries_accept_sequence_shape() {
        let y = "docker:\n  registries: []\n";
        let cfg: Config = serde_yaml::from_str(y).unwrap();
        assert!(cfg.docker.registries.is_empty());

        let y = "docker:\n  registries:\n    - name: docker.io\n      username: u\n      password: p\n";
        let cfg: Config = serde_yaml::from_str(y).unwrap();
        assert_eq!(cfg.docker.registries[0].name, "docker.io");
        assert_eq!(cfg.docker.registries[0].username, "u");
    }

    #[test]
    fn overhead_accepts_wings_map_shape() {
        // Exactly what the panel generates, including an empty map.
        let y = "docker:\n  overhead:\n    override: false\n    default_multiplier: 1.05\n    multipliers: {}\n";
        let cfg: Config = serde_yaml::from_str(y).unwrap();
        assert!(!cfg.docker.overhead.override_multiplier);
        assert!(cfg.docker.overhead.multipliers.is_empty());

        let y = "docker:\n  overhead:\n    override: true\n    default_multiplier: 1.05\n    multipliers:\n      2048: 1.15\n      4096: 1.10\n";
        let cfg: Config = serde_yaml::from_str(y).unwrap();
        assert!(cfg.docker.overhead.override_multiplier);
        assert_eq!(cfg.docker.overhead.multipliers.len(), 2);
        let m = cfg
            .docker
            .overhead
            .multipliers
            .iter()
            .find(|m| m.memory == 4096)
            .unwrap();
        assert!((m.overhead - 1.10).abs() < 1e-6);

        // String keys (JSON round-trips map keys as strings).
        let y = "docker:\n  overhead:\n    multipliers:\n      \"2048\": 1.15\n";
        let cfg: Config = serde_yaml::from_str(y).unwrap();
        assert_eq!(cfg.docker.overhead.multipliers[0].memory, 2048);

        // Legacy sequence form still accepted.
        let y = "docker:\n  overhead:\n    multipliers:\n      - memory: 2048\n        overhead: 1.2\n";
        let cfg: Config = serde_yaml::from_str(y).unwrap();
        assert_eq!(cfg.docker.overhead.multipliers[0].overhead, 1.2);

        // Round-trips back to the wings map shape.
        let out = serde_yaml::to_string(&cfg.docker.overhead).unwrap();
        assert!(out.contains("2048: 1.2"), "{out}");
    }

    #[test]
    fn overhead_accepts_legacy_override_multiplier_key() {
        let y = "docker:\n  overhead:\n    override_multiplier: true\n";
        let cfg: Config = serde_yaml::from_str(y).unwrap();
        assert!(cfg.docker.overhead.override_multiplier);
    }
}

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

/// The `settings` object the panel sends in
/// `GET /api/remote/servers/{uuid}`. This is the contractual shape that
/// the panel builds in ServerConfigurationStructureService — do not rename.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerConfig {
    pub uuid: Uuid,
    pub meta: ServerMeta,
    pub suspended: bool,
    #[serde(deserialize_with = "de_string_map")]
    pub environment: HashMap<String, String>,
    pub invocation: String,
    pub skip_egg_scripts: bool,
    pub build: ServerBuild,
    pub allocations: Allocations,
    pub mounts: Vec<ServerMount>,
    pub egg: Egg,
    pub container: ContainerConfig,
    #[serde(default)]
    pub labels: HashMap<String, String>,
    #[serde(default = "default_true")]
    pub crash_detection_enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerMeta {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// Resource limits for the server, in MB for memory/disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerBuild {
    /// MB
    pub memory_limit: i64,
    /// MB; -1 = unlimited
    pub swap: i64,
    /// 10..1000
    pub io_weight: i64,
    /// %; 0 = unlimited
    pub cpu_limit: i64,
    /// cpuset, e.g. "0-3"; empty = all
    #[serde(default, deserialize_with = "de_nullable_string")]
    pub threads: String,
    /// MB
    pub disk_space: i64,
    #[serde(default)]
    pub oom_disabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Allocations {
    #[serde(default)]
    pub force_outgoing_ip: bool,
    pub default: Allocation,
    #[serde(default)]
    pub mappings: HashMap<String, Vec<u16>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Allocation {
    pub ip: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerMount {
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Egg {
    pub id: Uuid,
    #[serde(default)]
    pub file_denylist: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContainerConfig {
    pub image: String,
    #[serde(default)]
    pub oom_disabled: bool,
    #[serde(default)]
    pub requires_rebuild: bool,
}

/// The `process_configuration` object the panel sends alongside settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessConfig {
    pub startup: ProcessStartup,
    pub stop: ProcessStop,
    #[serde(default)]
    pub configs: Vec<PatternConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessStartup {
    /// Lines that match these mark the server as "started".
    #[serde(default)]
    pub done: Vec<String>,
    #[serde(default)]
    pub user_interaction: Vec<String>,
    #[serde(default)]
    pub strip_ansi: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessStop {
    /// "command" | "signal" | "stop"
    pub r#type: String,
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PatternConfig {
    pub file: String,
    #[serde(default)]
    pub replace: Vec<PatternReplace>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternReplace {
    #[serde(rename = "match")]
    pub match_: String,
    pub replace_with: String,
    #[serde(default)]
    pub if_value: String,
}

impl ServerConfig {
    /// Default allocation IP used for the SERVER_IP env var and port binds.
    pub fn default_allocation(&self) -> &Allocation {
        &self.allocations.default
    }

    /// All `ip:port` pairs to bind.
    pub fn allocations(&self) -> Vec<(String, u16)> {
        let mut out = Vec::new();
        for (ip, ports) in &self.allocations.mappings {
            for port in ports {
                out.push((ip.clone(), *port));
            }
        }
        if out.is_empty() {
            out.push((self.default_allocation().ip.clone(), self.default_allocation().port));
        }
        out
    }
}
fn de_nullable_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Option::<String>::deserialize(deserializer)?;
    Ok(v.unwrap_or_default())
}

fn de_string_map<'de, D>(deserializer: D) -> Result<HashMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = HashMap::<String, Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .map(|(k, v)| {
            let s = match v {
                Value::String(s) => s,
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            (k, s)
        })
        .collect())
}

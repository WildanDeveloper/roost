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
    /// Lines that match these mark the server as "started". Each entry is
    /// either a raw substring or `regex:...` (wings OutputLineMatcher).
    /// Accepts a bare string as well as a list (some egg exports use a
    /// single string where wings expects `[]*OutputLineMatcher`).
    #[serde(default, deserialize_with = "de_one_or_many")]
    pub done: Vec<OutputLineMatcher>,
    #[serde(default)]
    pub user_interaction: Vec<String>,
    #[serde(default)]
    pub strip_ansi: bool,
}

fn de_one_or_many<'de, D>(deserializer: D) -> Result<Vec<OutputLineMatcher>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(OutputLineMatcher),
        Many(Vec<OutputLineMatcher>),
    }
    match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(m) => Ok(vec![m]),
        OneOrMany::Many(list) => Ok(list),
    }
}

/// One startup "done" line matcher (wings `remote.OutputLineMatcher`):
/// a raw substring, or a compiled regex when the string is prefixed
/// with `regex:`.
#[derive(Debug, Clone)]
pub struct OutputLineMatcher {
    raw: Option<String>,
    regex: Option<regex::Regex>,
}

impl<'de> Deserialize<'de> for OutputLineMatcher {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        if let Some(pattern) = raw.strip_prefix("regex:") {
            if pattern.is_empty() {
                return Ok(Self { raw: Some(raw), regex: None });
            }
            match regex::Regex::new(pattern) {
                Ok(re) => Ok(Self { raw: None, regex: Some(re) }),
                Err(e) => {
                    tracing::warn!(raw = %raw, error = %e, "failed to compile output line marked as being regex");
                    Ok(Self { raw: Some(raw), regex: None })
                }
            }
        } else {
            Ok(Self { raw: Some(raw), regex: None })
        }
    }
}

impl Serialize for OutputLineMatcher {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl OutputLineMatcher {
    pub fn as_str(&self) -> &str {
        self.raw.as_deref().unwrap_or("")
    }

    /// wings `Matches`: regex match when compiled, otherwise substring
    /// containment against the (possibly ANSI-stripped) line.
    pub fn matches(&self, line: &str) -> bool {
        match &self.regex {
            Some(re) => re.is_match(line),
            None => match &self.raw {
                Some(raw) => line.contains(raw.as_str()),
                None => false,
            },
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessStop {
    /// "command" | "signal" | "stop"
    pub r#type: String,
    #[serde(default)]
    pub value: String,
}

/// One egg configuration file entry from
/// `process_configuration.configs` (panel `ConfigurationFile`):
/// `{"file": "...", "parser": "...", "replace": [...]}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternConfig {
    pub file: String,
    /// "file" | "yaml"/"yml" | "properties" | "ini" | "json" | "xml"
    #[serde(default)]
    pub parser: String,
    #[serde(default)]
    pub replace: Vec<PatternReplace>,
}

/// One find/replace rule. `replace_with` is a typed JSON value in the
/// panel payload (string, bool or number); very old eggs used the key
/// "value" instead. `if_value` is optional (exact match or `regex:`).
#[derive(Debug, Clone, Serialize)]
pub struct PatternReplace {
    #[serde(rename = "match")]
    pub match_: String,
    pub replace_with: ReplaceValue,
    #[serde(default)]
    pub if_value: String,
}

/// The typed replacement value (wings `ReplaceValue` over jsonparser).
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ReplaceValue {
    Str(String),
    Bool(bool),
    Number(serde_json::Number),
    Null,
}

impl ReplaceValue {
    /// wings `ReplaceValue.String()`: JSON strings are unescaped, null
    /// renders as "<nil>", booleans/numbers as their raw representation.
    pub fn as_string(&self) -> String {
        match self {
            ReplaceValue::Str(s) => s.clone(),
            ReplaceValue::Null => "<nil>".to_string(),
            ReplaceValue::Bool(b) => b.to_string(),
            ReplaceValue::Number(n) => n.to_string(),
        }
    }

    /// Raw string used by the text/file parser (wings `Bytes()`).
    #[allow(dead_code)]
    pub fn raw_string(&self) -> String {
        match self {
            ReplaceValue::Str(s) => s.clone(),
            ReplaceValue::Null => "<nil>".to_string(),
            ReplaceValue::Bool(b) => b.to_string(),
            ReplaceValue::Number(n) => n.to_string(),
        }
    }
}

impl<'de> Deserialize<'de> for PatternReplace {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(rename = "match")]
            match_: String,
            #[serde(default)]
            if_value: String,
            #[serde(default)]
            replace_with: Option<Value>,
            #[serde(default)]
            value: Option<Value>,
        }
        let raw = Raw::deserialize(deserializer)?;
        // Old eggs use the "value" key; prefer "replace_with" when present.
        let replace_with = match (raw.replace_with, raw.value) {
            (Some(v), _) | (None, Some(v)) => ReplaceValue::from_json(v),
            (None, None) => ReplaceValue::Null,
        };
        Ok(PatternReplace {
            match_: raw.match_,
            replace_with,
            if_value: raw.if_value,
        })
    }
}

impl ReplaceValue {
    fn from_json(v: Value) -> Self {
        match v {
            Value::String(s) => ReplaceValue::Str(s),
            Value::Bool(b) => ReplaceValue::Bool(b),
            Value::Number(n) => ReplaceValue::Number(n),
            Value::Null => ReplaceValue::Null,
            // Objects/arrays are not valid replacement values; keep the raw
            // JSON text like wings would for a non-scalar (it treats them
            // as "<invalid>", but keeping the text is more useful here).
            other => ReplaceValue::Str(other.to_string()),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact JSON shape the panel sends in
    /// `GET /api/remote/servers/{uuid}` (ServerConfigurationStructureService).
    #[test]
    fn parses_panel_process_configuration() {
        let raw = serde_json::json!({
            "startup": {
                "done": "Server marked as running...",
                "user_interaction": ["press ENTER"],
                "strip_ansi": false
            },
            "stop": { "type": "command", "value": "stop" },
            "configs": [
                {
                    "file": "server.properties",
                    "parser": "properties",
                    "replace": [
                        { "match": "server-port", "replace_with": "${SERVER_PORT}" },
                        { "match": "motd", "replace_with": "A Server", "if_value": "old" },
                        { "match": "legacy", "value": "old-key-form" },
                        { "match": "flag", "replace_with": true },
                        { "match": "count", "replace_with": 3 }
                    ]
                },
                {
                    "file": "config.yml",
                    "parser": "yaml",
                    "replace": [
                        { "match": "listeners.*.host", "replace_with": "0.0.0.0" },
                        { "match": "servers[0].address", "replace_with": "{{config.docker.interface}}" }
                    ]
                }
            ]
        });

        let cfg: ProcessConfig = serde_json::from_value(raw).expect("panel payload must parse");
        assert_eq!(cfg.startup.done.len(), 1);
        assert_eq!(cfg.startup.done[0].as_str(), "Server marked as running...");
        assert_eq!(cfg.stop.r#type, "command");

        assert_eq!(cfg.configs.len(), 2);
        assert_eq!(cfg.configs[0].parser, "properties");
        assert_eq!(cfg.configs[0].replace.len(), 5);
        assert_eq!(cfg.configs[0].replace[0].replace_with, ReplaceValue::Str("${SERVER_PORT}".into()));
        assert_eq!(cfg.configs[0].replace[1].if_value, "old");
        // legacy "value" key fallback
        assert_eq!(cfg.configs[0].replace[2].replace_with, ReplaceValue::Str("old-key-form".into()));
        assert_eq!(cfg.configs[0].replace[3].replace_with, ReplaceValue::Bool(true));
        assert_eq!(cfg.configs[0].replace[4].replace_with, ReplaceValue::Number(serde_json::Number::from(3)));

        assert_eq!(cfg.configs[1].parser, "yaml");
    }

    /// `startup.done` entries prefixed with regex: compile into matchers.
    #[test]
    fn done_line_regex_matcher() {
        let raw = serde_json::json!({
            "startup": { "done": ["plain line", "regex:^\\[Server thread/INFO\\]: Done \\("], "strip_ansi": true },
            "stop": { "type": "signal", "value": "SIGTERM" }
        });
        let cfg: ProcessConfig = serde_json::from_value(raw).expect("must parse");
        assert!(cfg.startup.done[0].matches("some log plain line here"));
        assert!(!cfg.startup.done[0].matches("no match"));
        assert!(cfg.startup.done[1].matches("[Server thread/INFO]: Done (3.141s)! For help, type \"help\""));
        assert!(!cfg.startup.done[1].matches("starting up"));
    }
}

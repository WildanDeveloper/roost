use chrono::Utc;
use serde::Serialize;
use serde_json::Value;

/// An activity entry sent to the panel via POST /api/remote/activity.
/// Mirrors wings internal/models/activity.go (JSON shape: user, server,
/// event, metadata, ip, timestamp).
#[derive(Debug, Clone, Serialize)]
pub struct Activity {
    /// UUID of the user that triggered this event, or null for system events
    /// (wings JsonNullString serializes empty user as `null`).
    #[serde(default)]
    pub user: Option<String>,
    pub server: String,
    pub event: String,
    #[serde(default)]
    pub metadata: Value,
    pub ip: String,
    /// RFC3339 (UTC) timestamp, matching wings' time.Time JSON encoding.
    pub timestamp: String,
}

impl Activity {
    pub fn new(server: &str, event: impl Into<String>) -> Self {
        Self {
            user: None,
            server: server.to_string(),
            event: event.into(),
            metadata: Value::Null,
            ip: String::new(),
            timestamp: Utc::now().to_rfc3339(),
        }
    }

    pub fn with_user(mut self, user: Option<String>) -> Self {
        self.user = user;
        self
    }

    pub fn with_ip(mut self, ip: String) -> Self {
        self.ip = ip;
        self
    }

    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = metadata;
        self
    }
}

use serde::{Deserialize, Serialize};

/// Resource usage pushed to websocket clients as the `stats` event.
/// Serialized as a flat JSON object (mirrors wings `server.Proc()`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceUsage {
    pub memory_bytes: u64,
    pub memory_limit_bytes: u64,
    pub cpu_absolute: f64,
    pub network: NetworkStats,
    /// seconds
    pub uptime: u64,
    /// "offline" | "starting" | "running" | "stopping"
    pub state: String,
    pub disk_bytes: u64,
}

impl ResourceUsage {
    pub fn offline() -> Self {
        Self {
            state: "offline".to_string(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkStats {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}
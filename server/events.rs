use serde::Serialize;

use crate::models::ResourceUsage;

/// Events published by a server, broadcast to all websocket clients.
/// The router maps these to the wings websocket protocol events.
#[derive(Debug, Clone, Serialize)]
pub enum ServerEvent {
    /// One console line.
    ConsoleOutput(String),
    /// State change: "starting" | "running" | "stopping" | "offline".
    Status(String),
    /// Fresh resource usage snapshot.
    Stats(ResourceUsage),
    /// One install output line.
    InstallOutput(String),
    InstallStarted,
    InstallCompleted,
    /// Message shown to users, e.g. "[Pterodactyl Daemon]: ...".
    DaemonMessage(String),
    /// JSON payload of a completed backup.
    BackupCompleted(String),
    /// Backup restore completed.
    BackupRestoreCompleted(String),
    /// Server deleted event (panel cleanup).
    Deleted,
    /// "started" | "success" | "failure" (wings transfer status event).
    TransferStatus(String),
}

impl ServerEvent {
    /// The websocket event name for this event.
    pub fn event_name(&self) -> &'static str {
        match self {
            ServerEvent::ConsoleOutput(_) => "console output",
            ServerEvent::Status(_) => "status",
            ServerEvent::Stats(_) => "stats",
            ServerEvent::InstallOutput(_) => "install output",
            ServerEvent::InstallStarted => "install started",
            ServerEvent::InstallCompleted => "install completed",
            ServerEvent::DaemonMessage(_) => "daemon message",
            ServerEvent::BackupCompleted(_) => "backup completed",
            ServerEvent::BackupRestoreCompleted(_) => "backup restore completed",
            ServerEvent::Deleted => "server deleted",
            ServerEvent::TransferStatus(_) => "transfer status",
        }
    }

    /// The args array sent on the websocket.
    pub fn args(&self) -> Vec<String> {
        match self {
            ServerEvent::ConsoleOutput(line) | ServerEvent::InstallOutput(line) => {
                vec![line.clone()]
            }
            ServerEvent::Status(state) => vec![state.clone()],
            ServerEvent::Stats(usage) => vec![serde_json::to_string(usage).unwrap_or_default()],
            ServerEvent::InstallStarted | ServerEvent::InstallCompleted => vec![String::new()],
            ServerEvent::DaemonMessage(msg) => vec![msg.clone()],
            ServerEvent::BackupCompleted(payload) | ServerEvent::BackupRestoreCompleted(payload) => vec![payload.clone()],
            ServerEvent::Deleted => vec![String::new()],
            ServerEvent::TransferStatus(state) => vec![state.clone()],
        }
    }

    /// Whether this event requires a specific websocket permission.
    /// Returns the required permission if any.
    pub fn required_permission(&self) -> Option<&'static str> {
        match self {
            ServerEvent::InstallOutput(_) | ServerEvent::InstallStarted | ServerEvent::InstallCompleted => {
                Some("admin.websocket.install")
            }
            ServerEvent::BackupCompleted(_) | ServerEvent::BackupRestoreCompleted(_) => Some("backup.read"),
            _ => None,
        }
    }
}
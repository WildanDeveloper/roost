pub mod server;

use std::sync::OnceLock;
use std::sync::Arc;

static SFTP_SERVER: OnceLock<Arc<server::SftpServer>> = OnceLock::new();

pub fn set_sftp_server(s: Arc<server::SftpServer>) {
    let _ = SFTP_SERVER.set(s);
}

/// Abort all active SFTP sessions for a server (mirrors wings
/// `Sftp().CancelAll()` when an installation begins).
pub async fn cancel_sessions_for(uuid: &str) {
    if let Some(s) = SFTP_SERVER.get() {
        s.cancel_sessions(uuid).await;
    }
}
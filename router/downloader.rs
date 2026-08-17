use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Global registry of in-flight remote file downloads, keyed by a random
/// identifier (wings `downloader` package). Downloads are tracked per
/// server so the panel can list and cancel them.
static REGISTRY: std::sync::OnceLock<Mutex<HashMap<String, Arc<Download>>>> =
    std::sync::OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, Arc<Download>>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Maximum concurrent downloads per server (wings: 3).
pub const MAX_PER_SERVER: usize = 3;

pub struct Download {
    pub identifier: String,
    pub server_uuid: Uuid,
    pub url: String,
    pub file_name: String,
    pub directory: String,
    /// 0..100
    progress: AtomicU64,
    cancel: CancellationToken,
}

impl Download {
    fn new(server_uuid: Uuid, url: String, file_name: String, directory: String) -> Arc<Self> {
        Arc::new(Self {
            identifier: Uuid::new_v4().to_string(),
            server_uuid,
            url,
            file_name,
            directory,
            progress: AtomicU64::new(0),
            cancel: CancellationToken::new(),
        })
    }

    pub fn progress(&self) -> f64 {
        self.progress.load(Ordering::SeqCst) as f64 / 100.0
    }

    pub fn set_progress(&self, fraction: f64) {
        let pct = (fraction.clamp(0.0, 1.0) * 100.0) as u64;
        self.progress.store(pct, Ordering::SeqCst);
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn child_token(&self) -> CancellationToken {
        self.cancel.child_token()
    }
}

/// JSON shape sent to the panel (wings Download.MarshalJSON).
#[derive(Debug, Serialize)]
pub struct DownloadInfo {
    pub identifier: String,
    pub progress: f64,
}

/// Register and track a new download for a server.
pub async fn track(
    server_uuid: Uuid,
    url: String,
    file_name: String,
    directory: String,
) -> Arc<Download> {
    let dl = Download::new(server_uuid, url, file_name, directory);
    registry().lock().await.insert(dl.identifier.clone(), dl.clone());
    dl
}

/// All tracked downloads for a server.
pub async fn by_server(server_uuid: Uuid) -> Vec<Arc<Download>> {
    registry()
        .lock()
        .await
        .values()
        .filter(|d| d.server_uuid == server_uuid)
        .cloned()
        .collect()
}

/// Count of tracked downloads for a server.
pub async fn count_for_server(server_uuid: Uuid) -> usize {
    by_server(server_uuid).await.len()
}

/// A specific download by id, if tracked.
pub async fn by_id(identifier: &str) -> Option<Arc<Download>> {
    registry().lock().await.get(identifier).cloned()
}

/// Remove a download from tracking (wings removes after completion).
pub async fn untrack(identifier: &str) {
    registry().lock().await.remove(identifier);
}

/// Cancel all downloads for a server (wings deleteServer does this).
pub async fn cancel_for_server(server_uuid: Uuid) {
    for dl in by_server(server_uuid).await {
        dl.cancel();
        untrack(&dl.identifier).await;
    }
}
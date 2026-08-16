use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::docker::DockerClient;
use crate::error::{AppError, AppResult};
use crate::remote::types::RawServerData;
use crate::remote::PanelClient;
use crate::server::Server;
use crate::state::SharedConfig;

/// Everything a server needs from the daemon.
#[derive(Clone)]
pub struct ManagerShared {
    pub docker: DockerClient,
    pub daemon: SharedConfig,
    pub panel: Arc<RwLock<PanelClient>>,
}

/// Registry for all servers on this node.
pub struct ServerManager {
    shared: ManagerShared,
    servers: RwLock<HashMap<Uuid, Arc<Server>>>,
}

impl ServerManager {
    pub fn new(docker: DockerClient, daemon: SharedConfig, panel: Arc<RwLock<PanelClient>>) -> Self {
        Self {
            shared: ManagerShared { docker, daemon, panel },
            servers: RwLock::new(HashMap::new()),
        }
    }

    pub async fn get(&self, uuid: Uuid) -> AppResult<Arc<Server>> {
        let map = self.servers.read().await;
        tracing::debug!(uuid = %uuid, keys = map.len(), "manager get");
        map.get(&uuid).cloned().ok_or(AppError::ServerNotFound)
    }

    pub async fn list(&self) -> Vec<Arc<Server>> {
        self.servers.read().await.values().cloned().collect()
    }

    pub async fn contains(&self, uuid: Uuid) -> bool {
        self.servers.read().await.contains_key(&uuid)
    }

    /// Bootstrap sequence at daemon boot:
    /// 1. reset stale install states on the panel
    /// 2. fetch the full server list (paginated)
    /// 3. register every server
    /// 4. clean up orphaned containers (crash leftovers, _installer)
    pub async fn boot(&self) -> AppResult<()> {
        tracing::info!("syncing servers from panel...");

        self.shared.panel.read().await.reset_servers().await?;

        let per_page = self.shared.daemon.read().await.remote_query.boot_servers_per_page.max(1);
        let remote_servers = self.shared.panel.read().await.list_servers(per_page).await?;

        let daemon = self.shared.daemon.read().await.clone();
        let mut known = Vec::new();
        for data in remote_servers {
            let data_dir = daemon.data_dir(&data.uuid.to_string());
            let server = Arc::new(Server::new(data, &self.shared, data_dir));
            known.push(server.uuid.to_string());
            self.servers.write().await.insert(server.uuid, server);
        }

        tracing::info!(count = known.len(), "servers registered from panel");

        self.cleanup_orphaned_containers(&known).await;
        Ok(())
    }

    /// Remove containers that don't belong to any known server (crashes,
    /// leftover installers) so the node heals itself after a restart.
    async fn cleanup_orphaned_containers(&self, known: &[String]) {
        let containers = match self.shared.docker.list_managed_containers().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "cannot list managed containers");
                return;
            }
        };

        let valid: Vec<String> = containers
            .iter()
            .filter(|name| {
                let name = name.as_str();
                let is_server = known.iter().any(|k| k.as_str() == name);
                let is_installer =
                    name.ends_with("_installer") && known.iter().any(|k| name == &format!("{k}_installer"));
                is_server || is_installer
            })
            .cloned()
            .collect();

        for name in containers {
            if !valid.contains(&name) {
                tracing::info!(container = %name, "removing orphaned container");
                let _ = self.shared.docker.remove(&name).await;
            }
        }
    }

    /// Register a server from POST /api/servers and (optionally) start
    /// the install process asynchronously.
    pub async fn create_remote(&self, uuid: Uuid, start_on_completion: bool) -> AppResult<()> {
        if self.contains(uuid).await {
            return Ok(());
        }

        let resp = self.shared.panel.read().await.get_server(uuid).await?;
        let data = RawServerData {
            uuid,
            settings: resp.settings,
            process_configuration: resp.process_configuration,
        };

        let daemon = self.shared.daemon.read().await.clone();
        let data_dir = daemon.data_dir(&uuid.to_string());
        let server = Arc::new(Server::new(data, &self.shared, data_dir));
        self.servers.write().await.insert(uuid, server.clone());

        // Mirrors wings: the install process always runs; start_on_completion
        // only controls whether the server is started once it finishes.
        let server = server.clone();
        tokio::spawn(async move {
            server.install(false).await;
            if start_on_completion {
                let _ = server.power_start().await;
            }
        });

        Ok(())
    }

    /// Remove a server: stop it, destroy its container, remove data.
    pub async fn delete(&self, uuid: Uuid) -> AppResult<()> {
        let server = self.get(uuid).await?;
        server.delete_container(true).await?;
        self.servers.write().await.remove(&uuid);
        tracing::info!(uuid = %uuid, "server deleted");
        Ok(())
    }

    /// Suspend flag is managed by the panel (set via config sync); this
    /// helper marks it in memory.
    #[allow(dead_code)]
pub async fn set_suspended(&self, uuid: Uuid, suspended: bool) -> AppResult<()> {
        let server = self.get(uuid).await?;
        server.suspended.store(suspended, Ordering::SeqCst);
        Ok(())
    }

    #[allow(dead_code)]
pub fn shared(&self) -> &ManagerShared {
        &self.shared
    }
}
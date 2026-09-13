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
    #[allow(dead_code)]
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

    /// All server UUIDs currently registered on this node.
    pub async fn all_uuids(&self) -> Vec<Uuid> {
        self.servers.read().await.keys().cloned().collect()
    }
    #[allow(dead_code)]

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
        self.restore_states().await;
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
        let server = self.register(uuid).await?;

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

    /// Load a server's config from the panel and insert it into the
    /// manager without starting any install process (used for incoming
    /// server transfers, mirrors wings installer.New + manager.Add).
    pub async fn register(&self, uuid: Uuid) -> AppResult<Arc<Server>> {
        if let Some(server) = self.servers.read().await.get(&uuid) {
            return Ok(server.clone());
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
        tracing::info!(uuid = %uuid, "server registered for transfer");
        Ok(server)
    }

    /// Drop a server from the manager without touching the container or
    /// its files (used by the incoming transfer failure path).
    pub async fn remove(&self, uuid: Uuid) -> AppResult<()> {
        self.servers.write().await.remove(&uuid);
        Ok(())
    }

    /// Remove a server: stop it, destroy its container, remove data. All
    /// live websocket and SFTP sessions are aborted first (wings
    /// Server.Delete calls Websockets().CancelAll() and Sftp().CancelAll()).
    pub async fn delete(&self, uuid: Uuid) -> AppResult<()> {
        let server = self.get(uuid).await?;
        server.cancel_websockets().await;
        crate::sftp::cancel_sessions_for(&uuid.to_string()).await;
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

    /// Write the current state of every server to `states.json` so a
    /// daemon or machine restart can restore servers to their previous
    /// state (wings `Manager.PersistStates`). Runs on a 60s ticker; the
    /// file is allowed to lag behind reality.
    pub async fn persist_states(&self) -> AppResult<()> {
        let path = {
            let daemon = self.shared.daemon.read().await;
            daemon.states_path()
        };
        let mut states = serde_json::Map::new();
        for server in self.list().await {
            states.insert(
                server.uuid.to_string(),
                serde_json::Value::String(server.query_state().await.as_str().to_string()),
            );
        }
        let data = serde_json::to_vec(&states)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot serialize states: {e}")))?;
        std::fs::write(&path, data)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot write {}: {e}", path.display())))?;
        Ok(())
    }

    /// Read the cached states from disk, keeping only servers that are
    /// currently registered (wings `Manager.ReadStates`).
    fn read_states(&self) -> HashMap<Uuid, String> {
        let path = {
            match self.shared.daemon.try_read() {
                Ok(daemon) => daemon.states_path(),
                Err(_) => return HashMap::new(),
            }
        };
        let raw = match std::fs::read(&path) {
            Ok(data) => data,
            Err(_) => return HashMap::new(),
        };
        let parsed: HashMap<String, String> = match serde_json::from_slice(&raw) {
            Ok(m) => m,
            Err(_) => return HashMap::new(),
        };
        parsed
            .into_iter()
            .filter_map(|(k, v)| k.parse::<Uuid>().ok().map(|u| (u, v)))
            .collect()
    }

    /// Restore servers to their pre-restart state after a daemon boot
    /// (wings cmd/root.go): servers whose container is still running get
    /// re-attached (never stopped externally!), servers that were
    /// running/starting before the restart are booted again, everything
    /// else is forced offline. Up to 4 servers are processed concurrently
    /// with a 30s Docker timeout each (wings workerpool of 4).
    pub async fn restore_states(&self) {
        let states = self.read_states();
        if states.is_empty() {
            return;
        }
        tracing::info!(count = states.len(), "restoring cached server states");

        let mut futures = Vec::new();
        for (uuid, state) in states {
            let server = match self.get(uuid).await {
                Ok(s) => s,
                Err(_) => continue,
            };
            futures.push(Self::restore_one(server, state));
        }

        use futures_util::StreamExt;
        let _ = futures_util::stream::iter(futures)
            .buffer_unordered(4)
            .collect::<Vec<()>>()
            .await;

        // Refresh the cache immediately so a crash right after boot does
        // not resurrect stale states.
        if let Err(e) = self.persist_states().await {
            tracing::warn!(error = %e, "failed to persist server states after restore");
        }
    }

    async fn restore_one(server: Arc<Server>, state: String) {
        let name = server.uuid.to_string();

        // Is the container actually running right now? Bounded to 30s so
        // a hung Docker daemon cannot block the whole boot (wings uses a
        // 30s context for exactly this reason).
        let container_running = {
            let docker = server.docker.clone();
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                async { docker.inspect_container(&name).await.ok().flatten() },
            )
            .await
            .ok()
            .flatten()
            .map(|c| c.state.and_then(|s| s.running).unwrap_or(false))
            .unwrap_or(false)
        };

        if container_running {
            // Never stop a container that is running outside our control —
            // re-attach instead (wings keeps those processes alive).
            tracing::info!(uuid = %name, "detected server is running, re-attaching to process...");
            if let Ok(stream) = server.docker.attach(&name).await {
                crate::server::console::start_console(server.clone(), stream).await;
            }
            server.set_state(crate::server::ServerState::Running).await;
            server.mark_started_from_container(&name).await;
            server.start_stats_loop();
            if let Err(e) = server.sync_from_panel().await {
                tracing::warn!(uuid = %name, error = %e, "failed to re-sync server configuration");
            }
            return;
        }

        match state.as_str() {
            "running" | "starting" => {
                tracing::info!(uuid = %name, previous = %state, "returning server to running state");
                if let Err(e) = server.power_start().await {
                    tracing::warn!(uuid = %name, error = %e, "failed to return server to running state");
                }
            }
            _ => {
                server.set_state(crate::server::ServerState::Offline).await;
            }
        }
    }

    #[allow(dead_code)]
pub fn shared(&self) -> &ManagerShared {
        &self.shared
    }
}
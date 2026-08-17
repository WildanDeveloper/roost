use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

use serde::Serialize;
use uuid::Uuid;

use crate::docker::DockerClient;
use crate::error::{AppError, AppResult};
use crate::models::{ProcessConfig, ResourceUsage, ServerConfig};
use crate::remote::types::RawServerData;
use crate::remote::PanelClient;
use crate::server::events::ServerEvent;
use crate::server::files::Filesystem;
use crate::server::ManagerShared;
use crate::state::SharedConfig;

pub const MAX_WEBSOCKETS_PER_SERVER: usize = 30;

/// Server state as reported to the panel. Mirrors wings states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerState {
    Offline,
    Starting,
    Running,
    Stopping,
}

impl ServerState {
    pub const fn as_str(self) -> &'static str {
        match self {
            ServerState::Offline => "offline",
            ServerState::Starting => "starting",
            ServerState::Running => "running",
            ServerState::Stopping => "stopping",
        }
    }
}

/// Snapshot of the previous docker cpu stats, for cpu_absolute deltas.
#[derive(Default, Clone, Copy)]
#[allow(dead_code)]
pub struct CpuPrev {
    pub total: u64,
    pub system: u64,
}

/// One managed server on this node.
/// Servers that crashed and need an automatic restart, dispatched to the
/// neutral restart loop in main (keeps the crash path out of the
/// start_unlocked/power_start opaque-future cycle).
pub static CRASH_RESTART_TX: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<Arc<Server>>> =
    std::sync::OnceLock::new();

pub struct Server {
    pub uuid: Uuid,
    #[allow(dead_code)]
    pub name: RwLock<String>,
    pub state: RwLock<ServerState>,

    pub config: RwLock<ServerConfig>,
    pub process_config: RwLock<ProcessConfig>,
    pub suspended: AtomicBool,
    pub installing: AtomicBool,

    pub docker: DockerClient,
    pub fs: Filesystem,
    pub daemon: SharedConfig,
    pub panel: Arc<RwLock<PanelClient>>,

    /// Broadcast channel for all server events (console, status, stats...).
    events: tokio::sync::broadcast::Sender<ServerEvent>,
    /// Recent console lines for the websocket `send logs` replay.
    logs: RwLock<VecDeque<String>>,
    log_count: usize,

    /// Latest computed resource usage.
    usage: RwLock<ResourceUsage>,
    cpu_prev: Mutex<CpuPrev>,
    started_at: Mutex<Option<Instant>>,
    /// Fingerprint of the config the current container was created with.
    container_fingerprint: RwLock<Option<String>>,

    /// Serializes power actions for this server.
    power_lock: Mutex<()>,
    /// stdin endpoint for console commands.
    console_tx: RwLock<Option<tokio::sync::mpsc::Sender<String>>>,
    stats_running: AtomicBool,

    /// Connected websocket clients (for the connection cap).
    pub ws_connections: AtomicUsize,
    /// Last crash time for wings-style crash detection.
    last_crash: tokio::sync::Mutex<Option<std::time::Instant>>,
    /// True while a server transfer is in progress.
    pub transferring: AtomicBool,
    /// Handle of the background transfer task (outgoing), for cancel.
    transfer_task: tokio::sync::Mutex<Option<tokio::task::AbortHandle>>,
    incoming_cancel: tokio::sync::Mutex<Option<tokio_util::sync::CancellationToken>>,
}

/// GET /api/servers and GET /api/servers/:id response shape.
#[derive(Debug, Clone, Serialize)]
pub struct ApiResponse {
    pub state: String,
    pub is_suspended: bool,
    pub utilization: ResourceUsage,
    pub configuration: ServerConfig,
}

impl Server {
    pub fn new(data: RawServerData, shared: &ManagerShared, data_dir: PathBuf) -> Self {
        let (events, _) = tokio::sync::broadcast::channel(1024);
        let denylist = data.settings.egg.file_denylist.clone();

        Self {
            uuid: data.uuid,
            name: RwLock::new(data.settings.meta.name.clone()),
            state: RwLock::new(ServerState::Offline),
            config: RwLock::new(data.settings),
            process_config: RwLock::new(data.process_configuration.unwrap_or_default()),
            suspended: AtomicBool::new(false),
            installing: AtomicBool::new(false),
            docker: shared.docker.clone(),
            fs: Filesystem::new(data_dir, denylist),
            daemon: shared.daemon.clone(),
            panel: shared.panel.clone(),
            events,
            logs: RwLock::new(VecDeque::new()),
            log_count: 150,
            usage: RwLock::new(ResourceUsage::offline()),
            cpu_prev: Mutex::new(CpuPrev::default()),
            started_at: Mutex::new(None),
            container_fingerprint: RwLock::new(None),
            power_lock: Mutex::new(()),
            console_tx: RwLock::new(None),
            stats_running: AtomicBool::new(false),
            ws_connections: AtomicUsize::new(0),
            last_crash: tokio::sync::Mutex::new(None),
            transferring: AtomicBool::new(false),
            transfer_task: tokio::sync::Mutex::new(None),
            incoming_cancel: tokio::sync::Mutex::new(None),
        }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<ServerEvent> {
        self.events.subscribe()
    }

    pub fn publish(&self, event: ServerEvent) {
        let _ = self.events.send(event);
    }

    pub async fn set_state(&self, state: ServerState) {
        *self.state.write().await = state;
        self.publish(ServerEvent::Status(state.as_str().to_string()));
    }

    pub async fn query_state(&self) -> ServerState {
        *self.state.read().await
    }

    pub fn is_running(&self) -> bool {
        matches!(
            self.state.try_read().map(|s| *s).unwrap_or(ServerState::Offline),
            ServerState::Running | ServerState::Starting
        )
    }

    // ---- console ----------------------------------------------------------

    pub async fn set_console_tx(&self, tx: Option<tokio::sync::mpsc::Sender<String>>) {
        *self.console_tx.write().await = tx;
    }

    /// Push raw console bytes; emit complete lines to the log buffer and
    /// the broadcast channel.
    pub async fn push_console_bytes(&self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        for raw in text.split('\n') {
            let line = raw.trim_end_matches('\r').to_string();
            if line.is_empty() {
                continue;
            }
            {
                let mut logs = self.logs.write().await;
                if logs.len() >= self.log_count {
                    logs.pop_front();
                }
                logs.push_back(line.clone());
            }
            self.publish(ServerEvent::ConsoleOutput(line));
        }
    }

    pub async fn recent_logs(&self) -> Vec<String> {
        self.logs.read().await.iter().cloned().collect()
    }

    /// Send a command to the container's stdin. Only valid while running.
    pub async fn send_command(&self, command: &str) -> AppResult<()> {
        if !self.is_running() {
            return Err(AppError::BadRequest(
                "Cannot send commands to a stopped server instance.".into(),
            ));
        }
        let tx = self.console_tx.read().await.clone();
        match tx {
            Some(tx) => tx
                .send(command.to_string())
                .await
                .map_err(|_| AppError::BadRequest("console stream is not attached".into())),
            None => Err(AppError::BadRequest("console stream is not attached".into())),
        }
    }

    #[allow(dead_code)]
pub async fn has_console(&self) -> bool {
        self.console_tx.read().await.is_some()
    }

    // ---- resource usage ----------------------------------------------------

    pub async fn usage(&self) -> ResourceUsage {
        self.usage.read().await.clone()
    }

    #[allow(dead_code)]
pub async fn disk_bytes(&self) -> u64 {
        self.fs.disk_usage()
    }

    /// One usage snapshot from docker stats.
    pub async fn snapshot_usage(&self) -> AppResult<ResourceUsage> {
        let state = self.query_state().await;
        let container = self.uuid.to_string();

        let stats = match self.docker.stats_one_shot(&container).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(uuid = %self.uuid, error = %e, "stats unavailable");
                return Ok(ResourceUsage::offline());
            }
        };

        let mut network = crate::models::NetworkStats::default();
        if let Some(nets) = &stats.networks {
            for n in nets.values() {
                network.rx_bytes += n.rx_bytes;
                network.tx_bytes += n.tx_bytes;
            }
        }

        let cpu_stats = &stats.cpu_stats;
        let precpu = &stats.precpu_stats;
        let total = cpu_stats.cpu_usage.total_usage;
        let system = cpu_stats.system_cpu_usage.unwrap_or(0);
        let online = cpu_stats.online_cpus.unwrap_or(1);
        let prev_total = precpu.cpu_usage.total_usage;
        let prev_system = precpu.system_cpu_usage.unwrap_or(system);

        let prev = CpuPrev { total: prev_total, system: prev_system };
        *self.cpu_prev.lock().await = prev;

        let cpu_absolute = if system > prev_system && total >= prev_total {
            ((total - prev_total) as f64 / (system - prev_system) as f64) * online as f64 * 100.0
        } else {
            0.0
        };

        let uptime = self
            .started_at
            .lock()
            .await
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);

        let usage = ResourceUsage {
            memory_bytes: stats.memory_stats.usage.unwrap_or(0),
            memory_limit_bytes: stats.memory_stats.limit.unwrap_or(0),
            cpu_absolute,
            network,
            uptime,
            state: state.as_str().to_string(),
            disk_bytes: self.fs.disk_usage(),
        };
        *self.usage.write().await = usage.clone();
        Ok(usage)
    }

    /// Periodic stats collection while the server runs (1s cadence).
    pub fn start_stats_loop(self: &Arc<Self>) {
        if self.stats_running.swap(true, Ordering::SeqCst) {
            return;
        }
        let server = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                if !server.is_running() {
                    server.stats_running.store(false, Ordering::SeqCst);
                    let mut offline = ResourceUsage::offline();
                    offline.disk_bytes = server.fs.disk_usage();
                    *server.usage.write().await = offline.clone();
                    server.publish(ServerEvent::Stats(offline));
                    return;
                }
                if let Ok(usage) = server.snapshot_usage().await {
                    server.publish(ServerEvent::Stats(usage));
                }
            }
        });
    }

    // ---- configuration -----------------------------------------------------

    /// Re-fetch configuration from the panel and apply it.
    pub async fn sync_from_panel(&self) -> AppResult<()> {
        let fresh = self
            .panel
            .read()
            .await
            .get_server(self.uuid)
            .await
            .map_err(|e| AppError::Remote(format!("panel sync failed: {e}")))?;

        self.suspended.store(fresh.settings.suspended, Ordering::SeqCst);
        *self.config.write().await = fresh.settings;
        *self.process_config.write().await = fresh.process_configuration.unwrap_or_default();
        self.apply_denylist().await;
        tracing::info!(uuid = %self.uuid, "configuration synced from panel");
        Ok(())
    }

    async fn apply_denylist(&self) {
        let denylist = self.config.read().await.egg.file_denylist.clone();
        self.fs.set_denylist(denylist);
    }

    /// Fingerprint of the current config; containers are rebuilt when this
    /// changes between the stored fingerprint and the fresh one.
    async fn fingerprint(&self) -> String {
        let cfg = self.config.read().await.clone();
        let env = self.build_env().await;
        format!("{:?}|{:?}", cfg, env)
    }

    async fn container_matches_config(&self) -> bool {
        let stored = self.container_fingerprint.read().await.clone();
        match stored {
            Some(f) => f == self.fingerprint().await,
            None => false,
        }
    }

    /// Environment variables, mirroring wings `GetEnvironmentVariables`:
    /// TZ, STARTUP, SERVER_MEMORY, SERVER_IP, SERVER_PORT, then egg vars.
    pub async fn build_env(&self) -> Vec<String> {
        let cfg = self.config.read().await.clone();
        let daemon = self.daemon.read().await.clone();
        let mut env: Vec<String> = Vec::new();

        let timezone = if daemon.system.timezone.is_empty() {
            "UTC".to_string()
        } else {
            daemon.system.timezone.clone()
        };

        let allocation = cfg.default_allocation();
        let server_ip = if allocation.ip == "127.0.0.1" {
            daemon.docker.network.interface.clone()
        } else {
            allocation.ip.clone()
        };

        env.push(format!("TZ={timezone}"));
        env.push(format!("STARTUP={}", cfg.invocation));
        env.push(format!("SERVER_MEMORY={}", cfg.build.memory_limit));
        env.push(format!("SERVER_IP={server_ip}"));
        env.push(format!("SERVER_PORT={}", allocation.port));

        for (key, value) in &cfg.environment {
            if matches!(key.as_str(), "TZ" | "STARTUP" | "SERVER_MEMORY" | "SERVER_IP" | "SERVER_PORT") {
                continue;
            }
            env.push(format!("{key}={value}"));
        }
        env.push(format!("PTERODACTYL_SERVER_UUID={}", cfg.uuid));
        env
    }

    pub fn is_installing(&self) -> bool {
        self.installing.load(Ordering::SeqCst)
    }

    pub fn api_response(&self) -> ApiResponse {
        let cfg = self.config.try_read().map(|c| c.clone()).unwrap_or_default();
        let state = self.state.try_read().map(|s| *s).unwrap_or(ServerState::Offline);
        let mut usage = self
            .usage
            .try_read()
            .map(|u| u.clone())
            .unwrap_or_else(|_| ResourceUsage::offline());
        usage.state = state.as_str().to_string();
        ApiResponse {
            state: state.as_str().to_string(),
            is_suspended: self.suspended.load(Ordering::SeqCst),
            utilization: usage,
            configuration: cfg,
        }
    }

    // ---- power actions -----------------------------------------------------

    /// Stop the container; afterwards the server is offline.
    async fn stop_unlocked(&self, wait_seconds: u32) -> AppResult<()> {
        let name = self.uuid.to_string();
        let stop = self.process_config.read().await.stop.clone();

        match stop.r#type.as_str() {
            "stop" => {
                self.docker.stop(&name, wait_seconds).await?;
            }
            "signal" => {
                let signal = if stop.value.is_empty() { "SIGTERM" } else { &stop.value };
                self.docker.kill(&name, signal).await?;
            }
            _ => {
                // command: send the stop command to the console, then wait.
                if !stop.value.is_empty() {
                    if let Some(tx) = self.console_tx.read().await.clone() {
                        let _ = tx.send(stop.value).await;
                    }
                }
                let timed_out = tokio::time::timeout(
                    Duration::from_secs(wait_seconds.into()),
                    async {
                        use futures_util::StreamExt;
                        let mut wait = self.docker.wait_until_stopped(&name);
                        let _ = wait.next().await;
                    },
                )
                .await
                .is_err();
                if timed_out {
                    self.docker.kill(&name, "SIGKILL").await?;
                }
            }
        }
        Ok(())
    }

    /// Start the container (assumes the power lock is held).
    async fn start_unlocked(self: &Arc<Self>) -> AppResult<()> {
        if self.is_running() {
            return Ok(());
        }
        self.set_state(ServerState::Starting).await;

        std::fs::create_dir_all(self.fs.root())
            .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot create data dir: {e}")))?;

        chown_recursive(self.fs.root(), 1000, 1000);

        let image = self.config.read().await.container.image.clone();

        if !self.container_matches_config().await {
            self.docker.remove(&self.uuid.to_string()).await?;
            self.create_container().await?;
        }

        {
            let daemon = self.daemon.read().await.clone();
            let docker_cfg = daemon.docker.clone();
            if let Err(e) = self.docker.pull_image(&image, &docker_cfg).await {
                tracing::warn!(uuid = %self.uuid, image = %image, error = %e, "image unavailable");
            }
        }

        let stream = match self.docker.attach(&self.uuid.to_string()).await {
            Ok(s) => s,
            Err(e) => {
                self.set_state(ServerState::Offline).await;
                return Err(e);
            }
        };
        crate::server::console::start_console(self.clone(), stream).await;

        self.docker.start(&self.uuid.to_string()).await?;
        *self.started_at.lock().await = Some(Instant::now());
        self.set_state(ServerState::Running).await;
        self.start_stats_loop();
        let name = self.uuid.to_string();
        let watcher = self.clone();
        tokio::spawn(async move {
            use futures_util::StreamExt;
            let mut wait = watcher.docker.wait_until_stopped(&name);
            let mut code = None;
            while let Some(item) = wait.next().await {
                if let Ok(ev) = item {
                    code = Some(ev.status_code);
                    tracing::info!(uuid = %name, code = ev.status_code, "container stopped");
                    break;
                }
            }
            drop(wait);
            watcher.set_state(ServerState::Offline).await;
            watcher.stats_running.store(false, Ordering::SeqCst);
            tokio::spawn(Server::handle_server_crash(watcher, code));
        });
        tracing::info!(uuid = %self.uuid, "server started");
        Ok(())
    }

    async fn create_container(&self) -> AppResult<()> {
        let cfg = self.config.read().await.clone();
        let env = self.build_env().await;
        let daemon = self.daemon.read().await.clone();
        let network_ip = daemon.docker.network.interface.clone();
        if let Err(e) = self
            .docker
            .pull_image(&cfg.container.image, &daemon.docker)
            .await
        {
            tracing::warn!(image = %cfg.container.image, error = %e, "could not pull server image");
            return Err(e);
        }
        self.docker
            .create_server_container(self.uuid, &cfg, self.fs.root(), &daemon, &env, &network_ip)
            .await?;
        *self.container_fingerprint.write().await = Some(self.fingerprint().await);
        Ok(())
    }

    async fn handle_server_crash(self: Arc<Self>, exit_code: Option<i64>) {
        let srv_cfg = self.config.read().await.clone();
        let daemon_cfg = self.daemon.read().await.clone();
        if !srv_cfg.crash_detection_enabled || !daemon_cfg.system.crash_detection.enabled {
            return;
        }
        if self.is_installing() || self.suspended.load(Ordering::SeqCst) {
            return;
        }
        let code = exit_code.unwrap_or(0);
        let oom = self
            .docker
            .container_was_oom_killed(&self.uuid.to_string())
            .await
            .unwrap_or(false);
        if code == 0 && !oom && !daemon_cfg.system.crash_detection.detect_clean_exit_as_crash {
            return;
        }
        let srv = self.clone();
        tokio::spawn(async move {
            srv.publish_daemon_message(format!("---------- Detected server process in a crashed state! ----------")).await;
            srv.publish_daemon_message(format!("Exit code: {code}")).await;
            srv.publish_daemon_message(format!("Out of memory: {oom}")).await;
        });

        let timeout = daemon_cfg.system.crash_detection.timeout;
        let should_restart = {
            let mut last = self.last_crash.lock().await;
            if timeout != 0 && last.is_some() && last.unwrap().elapsed() < std::time::Duration::from_secs(timeout) {
                let srv = self.clone();
                tokio::spawn(async move {
                    srv.publish_daemon_message(format!(
                        "Aborting automatic restart, last crash occurred less than {timeout} seconds ago."
                    ))
                    .await;
                });
                false
            } else {
                *last = Some(std::time::Instant::now());
                true
            }
        };
        if should_restart {
            let srv = self.clone();
            tokio::spawn(async move {
                srv.publish_daemon_message("Restarting server process after crash...".to_string())
                    .await;
            });
            if let Some(tx) = CRASH_RESTART_TX.get() {
                let _ = tx.send(self);
            } else {
                tracing::warn!(uuid = %self.uuid, "crash restart channel not initialized");
            }
        }
    }

    /// Push a daemon-originated console line (crash notices, etc).
    pub async fn publish_daemon_message(&self, msg: String) {
        self.publish(ServerEvent::DaemonMessage(msg));
    }

    pub async fn power_start(self: &Arc<Self>) -> AppResult<()> {
        let _guard = self.power_lock.lock().await;
        if self.is_installing() {
            return Err(AppError::Conflict("server is currently installing or restoring".into()));
        }
        if self.suspended.load(Ordering::SeqCst) {
            return Err(AppError::BadRequest("server is suspended".into()));
        }
        self.clone().start_unlocked().await
    }

    pub async fn power_stop(&self, wait_seconds: u32) -> AppResult<()> {
        let _guard = self.power_lock.lock().await;
        if !self.is_running() {
            return Ok(());
        }
        self.set_state(ServerState::Stopping).await;
        let result = self.stop_unlocked(wait_seconds).await;
        self.set_state(ServerState::Offline).await;
        result
    }

    pub async fn power_kill(&self) -> AppResult<()> {
        let _guard = self.power_lock.lock().await;
        if !self.is_running() {
            return Ok(());
        }
        self.set_state(ServerState::Stopping).await;
        let result = self.docker.kill(&self.uuid.to_string(), "SIGKILL").await;
        self.set_state(ServerState::Offline).await;
        result
    }

    pub async fn power_restart(self: &Arc<Self>, wait_seconds: u32) -> AppResult<()> {
        let _guard = self.power_lock.lock().await;
        self.stop_unlocked(wait_seconds).await?;
        self.clone().start_unlocked().await
    }

    /// Whether a power action is in flight (used for reinstall's 409).
    pub fn is_power_locked(&self) -> bool {
        !self.power_lock.try_lock().is_ok()
    }

    /// Apply resource limits to the running container in place.
    pub async fn apply_limits(&self) -> AppResult<()> {
        let cfg = self.config.read().await;
        let daemon = self.daemon.read().await.clone();
        let resources = cfg.build.as_container_resources(&daemon.docker, false);
        drop(cfg);
        self.docker.update_resources(&self.uuid.to_string(), &resources).await
    }

    pub fn is_transferring(&self) -> bool {
        self.transferring.load(Ordering::SeqCst)
    }

    pub fn set_transferring(&self, value: bool) {
        self.transferring.store(value, Ordering::SeqCst);
    }

    /// Track the outgoing transfer task so DELETE can abort it.
    pub async fn set_transfer_task(&self, handle: Option<tokio::task::AbortHandle>) {
        *self.transfer_task.lock().await = handle;
    }

    /// Fresh cancellation token for an incoming transfer (replaces any
    /// previous one). The caller drives the transfer with this token.
    pub async fn fresh_incoming_cancel(&self) -> tokio_util::sync::CancellationToken {
        let token = tokio_util::sync::CancellationToken::new();
        *self.incoming_cancel.lock().await = Some(token.clone());
        token
    }

    pub async fn clear_incoming_cancel(&self) {
        *self.incoming_cancel.lock().await = None;
    }

    pub async fn cancel_incoming_transfer(&self) {
        if let Some(token) = self.incoming_cancel.lock().await.take() {
            token.cancel();
        }
    }

    pub async fn cancel_transfer_task(&self) {
        if let Some(handle) = self.transfer_task.lock().await.take() {
            handle.abort();
        }
    }

    /// Kill the process and remove the container (+ data dir optionally).
    pub async fn delete_container(&self, remove_data: bool) -> AppResult<()> {
        let _ = self.power_kill().await;
        self.docker.remove(&self.uuid.to_string()).await?;
        self.docker.remove(&format!("{}_installer", self.uuid)).await?;
        if remove_data {
            let _ = std::fs::remove_dir_all(self.fs.root());
        }
        Ok(())
    }
}

/// Helpers for path/dir operations used elsewhere.
#[allow(dead_code)]
pub fn ensure_dir(path: &Path) -> AppResult<()> {
    std::fs::create_dir_all(path)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot create {}: {e}", path.display())))
}
use std::os::unix::fs::chown as unix_chown;

fn chown_recursive(path: &std::path::Path, uid: u32, gid: u32) {
    let _ = unix_chown(path, Some(uid), Some(gid));
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            let _ = unix_chown(&p, Some(uid), Some(gid));
            if p.is_dir() {
                chown_recursive(&p, uid, gid);
            }
        }
    }
}

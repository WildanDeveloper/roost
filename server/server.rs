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

/// Stateful window for the console throttler (wings `ConsoleThrottle`).
struct ThrottleState {
    count: u64,
    last: std::time::Instant,
    /// True between the first denied line and the next allowed line, so the
    /// "outputting too quickly" notice fires once per episode (wings
    /// `strike` + `locker`).
    struck: bool,
}

impl Default for ThrottleState {
    fn default() -> Self {
        Self {
            count: 0,
            last: std::time::Instant::now(),
            struck: false,
        }
    }
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
    /// True while a server restore is running (blocks power actions, wings
    /// IsRestoring).
    pub restoring: AtomicBool,
    /// Set before an intentional stop/kill so the exit watcher skips crash
    /// detection (wings sets the ProcessStoppingState first for this).
    pub intentional_stop: AtomicBool,

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
    started_at: Mutex<Option<Instant>>,

    /// Serializes power actions for this server.
    power_lock: Mutex<()>,
    /// Stateful console throttler window (wings `ConsoleThrottle` — the
    /// period is continuous, not per-call).
    throttle: Mutex<ThrottleState>,
    /// stdin endpoint for console commands.
    console_tx: RwLock<Option<tokio::sync::mpsc::Sender<String>>>,
    stats_running: AtomicBool,

    /// Connected websocket clients (for the connection cap).
    pub ws_connections: AtomicUsize,
    /// Current websocket cancellation token; new connections hold the token
    /// they read at connect time, so cancelling swaps in a fresh token and
    /// aborts every live socket (wings `Websockets().CancelAll()`).
    pub ws_cancel: tokio::sync::RwLock<tokio_util::sync::CancellationToken>,
    /// Last crash time for wings-style crash detection.
    last_crash: tokio::sync::Mutex<Option<std::time::Instant>>,
    /// True while a server transfer is in progress.
    pub transferring: AtomicBool,
    /// Handle of the background transfer task (outgoing), for cancel.
    transfer_task: tokio::sync::Mutex<Option<tokio::task::AbortHandle>>,
    incoming_cancel: tokio::sync::Mutex<Option<tokio_util::sync::CancellationToken>>,
    /// Cached disk usage with last-check timestamp (disk_check_interval).
    disk_cache: tokio::sync::Mutex<(u64, Instant)>,
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
        // websocket_log_count from the daemon config (wings
        // WebsocketLogCount, default 150).
        let log_count = shared
            .daemon
            .try_read()
            .map(|c| c.system.websocket_log_count)
            .unwrap_or(150);

        Self {
            uuid: data.uuid,
            name: RwLock::new(data.settings.meta.name.clone()),
            state: RwLock::new(ServerState::Offline),
            config: RwLock::new(data.settings),
            process_config: RwLock::new(data.process_configuration.unwrap_or_default()),
            suspended: AtomicBool::new(false),
            installing: AtomicBool::new(false),
            restoring: AtomicBool::new(false),
            intentional_stop: AtomicBool::new(false),
            docker: shared.docker.clone(),
            fs: Filesystem::new(data_dir, denylist),
            daemon: shared.daemon.clone(),
            panel: shared.panel.clone(),
            events,
            logs: RwLock::new(VecDeque::new()),
            log_count,
            usage: RwLock::new(ResourceUsage::offline()),
            started_at: Mutex::new(None),
            power_lock: Mutex::new(()),
            throttle: Mutex::new(ThrottleState::default()),
            console_tx: RwLock::new(None),
            stats_running: AtomicBool::new(false),
            ws_connections: AtomicUsize::new(0),
            ws_cancel: tokio::sync::RwLock::new(tokio_util::sync::CancellationToken::new()),
            last_crash: tokio::sync::Mutex::new(None),
            transferring: AtomicBool::new(false),
            transfer_task: tokio::sync::Mutex::new(None),
            incoming_cancel: tokio::sync::Mutex::new(None),
            disk_cache: tokio::sync::Mutex::new((0, Instant::now())),
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
    /// the broadcast channel. Implements wings console throttling: if
    /// throttles.enabled, limits lines per period with a stateful window
    /// (wings `ConsoleThrottle.Allow` — the count resets only when the
    /// period has elapsed, so flooding across calls is still limited).
    pub async fn push_console_bytes(&self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        let (throttle_enabled, max_lines, reset_ms) = {
            let daemon = self.daemon.try_read().map(|c| c.clone()).unwrap_or_default();
            let t = &daemon.throttles;
            (t.enabled, t.lines, t.line_reset_interval)
        };
        for raw in text.split('\n') {
            let line = raw.trim_end_matches('\r').to_string();
            if line.is_empty() {
                continue;
            }
            if throttle_enabled {
                let mut t = self.throttle.lock().await;
                if t.last.elapsed() >= Duration::from_millis(reset_ms) {
                    t.count = 0;
                    t.last = Instant::now();
                }
                if t.count + 1 > max_lines {
                    // Denied: fire the strike notice once per episode.
                    if !t.struck {
                        t.struck = true;
                        let notice = "Server is outputting console data too quickly -- throttling...";
                        let mut logs = self.logs.write().await;
                        if logs.len() >= self.log_count {
                            logs.pop_front();
                        }
                        logs.push_back(notice.to_string());
                        self.publish(ServerEvent::ConsoleOutput(notice.to_string()));
                    }
                    continue;
                }
                t.count += 1;
                t.struck = false;
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
            return Err(AppError::BadGateway(
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

    /// Cached disk usage; refreshes at most once per disk_check_interval
    /// seconds (wings `DiskUsage` caching behavior).
    pub async fn disk_usage_cached(&self) -> u64 {
        let interval = {
            let daemon = self.daemon.try_read().map(|c| c.clone()).unwrap_or_default();
            daemon.system.disk_check_interval
        };
        // A disk check interval of 0 disables the check entirely (wings
        // DiskUsage returns 0 in that case, so the limit never triggers).
        if interval == 0 {
            return 0;
        }
        let mut cache = self.disk_cache.lock().await;
        if cache.1.elapsed().as_secs() >= interval {
            cache.0 = self.fs.disk_usage();
            cache.1 = Instant::now();
        }
        cache.0
    }

    /// Whether the server has space available for its disk limit
    /// (wings `HasSpaceAvailable`). disk_space <= 0 means unlimited.
    pub async fn has_space_available(&self) -> bool {
        let disk_limit = self.config.read().await.build.disk_space;
        if disk_limit <= 0 {
            return true;
        }
        let used = self.disk_usage_cached().await;
        used <= (disk_limit as u64) * 1024 * 1024
    }

    /// Push the disk limit + chown UID/GID into the filesystem (wings
    /// `SetDiskLimit` / `chownFile`). Called on every panel sync.
    async fn sync_filesystem_ids(&self) {
        let (limit, uid, gid) = {
            let cfg = self.config.read().await;
            let daemon = self.daemon.read().await.clone();
            let limit = (cfg.build.disk_space.max(0) as i64) * 1024 * 1024;
            let u = &daemon.system.user;
            let ids = if u.rootless.enabled {
                (u.rootless.container_uid as i32, u.rootless.container_gid as i32)
            } else if u.uid != 0 && u.gid != 0 {
                (u.uid as i32, u.gid as i32)
            } else {
                (988, 988)
            };
            (limit, ids.0, ids.1)
        };
        self.fs.set_disk_limit(limit);
        self.fs.set_chown_ids(uid, gid);
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

        let cpu_absolute = if system > prev_system && total >= prev_total {
            ((total - prev_total) as f64 / (system - prev_system) as f64) * online as f64 * 100.0
        } else {
            0.0
        };
        // wings rounds CPU to 3 decimals (environment/docker/stats.go).
        let cpu_absolute = (cpu_absolute * 1000.0).round() / 1000.0;

        let uptime = self
            .started_at
            .lock()
            .await
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);

        // wings calculateDockerMemory: subtract cached/inactive pages so the
        // panel shows the same numbers the docker CLI does.
        let raw_usage = stats.memory_stats.usage.unwrap_or(0);
        let (total_inactive_file, inactive_file) =
            match &stats.memory_stats.stats {
                Some(bollard::container::MemoryStatsStats::V1(v)) => {
                    (v.total_inactive_file, v.inactive_file)
                }
                Some(bollard::container::MemoryStatsStats::V2(v)) => (0, v.inactive_file),
                None => (0, 0),
            };
        let memory_bytes = if total_inactive_file > 0 && total_inactive_file < raw_usage {
            raw_usage - total_inactive_file
        } else if inactive_file < raw_usage {
            raw_usage - inactive_file
        } else {
            raw_usage
        };

        let usage = ResourceUsage {
            memory_bytes,
            memory_limit_bytes: stats.memory_stats.limit.unwrap_or(0),
            cpu_absolute,
            network,
            uptime,
            state: state.as_str().to_string(),
            disk_bytes: self.disk_usage_cached().await,
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
            // Wings disk limiter: fires once per boot when usage is over
            // the limit, then stops the server.
            let mut disk_limiter_fired = false;
            loop {
                interval.tick().await;
                if !server.is_running() {
                    server.stats_running.store(false, Ordering::SeqCst);
                    let mut offline = ResourceUsage::offline();
                    offline.disk_bytes = server.disk_usage_cached().await;
                    *server.usage.write().await = offline.clone();
                    server.publish(ServerEvent::Stats(offline));
                    return;
                }
                if let Ok(usage) = server.snapshot_usage().await {
                    if !disk_limiter_fired && server.over_disk_limit(usage.disk_bytes) {
                        disk_limiter_fired = true;
                        server.publish(ServerEvent::DaemonMessage(
                            "Server is exceeding the assigned disk space limit, stopping process now.".into(),
                        ));
                        server.intentional_stop.store(true, Ordering::SeqCst);
                        server.set_state(ServerState::Stopping).await;
                        let name = server.uuid.to_string();
                        let stop = server.stop_unlocked();
                        // Wings waits 1 minute before force terminating.
                        if tokio::time::timeout(Duration::from_secs(60), stop).await.is_err() {
                            let _ = server.docker.kill(&name, "SIGKILL").await;
                        }
                        server.set_state(ServerState::Offline).await;
                    }
                    server.publish(ServerEvent::Stats(usage));
                }
            }
        });
    }

    /// Whether the given disk usage exceeds the server's disk limit.
    pub fn over_disk_limit(&self, disk_bytes: u64) -> bool {
        let limit = self.config.try_read().map(|c| c.build.disk_space).unwrap_or(0);
        limit > 0 && disk_bytes > (limit as u64) * 1024 * 1024
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
        self.sync_filesystem_ids().await;
        tracing::info!(uuid = %self.uuid, "configuration synced from panel");

        // Wings: when a sync marks the server suspended, immediately
        // disconnect all websocket and SFTP clients.
        if self.suspended.load(Ordering::SeqCst) {
            self.cancel_websockets().await;
            crate::sftp::cancel_sessions_for(&self.uuid.to_string()).await;
        }
        Ok(())
    }

    async fn apply_denylist(&self) {
        let denylist = self.config.read().await.egg.file_denylist.clone();
        self.fs.set_denylist(denylist);
    }

    /// Wings always destroys and re-creates the container before every boot
    /// (OnBeforeStart) so synced panel data and mutable image tags are always
    /// applied and the log file is truncated. `remove` tolerates a missing
    /// container; `create_container` pulls the image first.
    async fn ensure_container_fresh(&self) -> AppResult<()> {
        self.docker.remove(&self.uuid.to_string()).await?;
        self.create_container().await
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

    /// Abort every live websocket (wings `Websockets().CancelAll()`).
    pub async fn cancel_websockets(&self) {
        let old = {
            let mut guard = self.ws_cancel.write().await;
            std::mem::replace(&mut *guard, tokio_util::sync::CancellationToken::new())
        };
        old.cancel();
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

    /// Mirror wings HandlePowerAction guard: no power action (including
    /// terminate) while installing, transferring or restoring.
    fn check_power_allowed(&self) -> AppResult<()> {
        if self.is_installing() {
            return Err(AppError::Conflict("server is currently installing".into()));
        }
        if self.is_transferring() {
            return Err(AppError::Conflict("server is currently being transferred".into()));
        }
        if self.is_restoring() {
            return Err(AppError::Conflict("server is currently restoring a backup".into()));
        }
        Ok(())
    }

    /// Wings powerLock: TryAcquire when wait == 0, otherwise keep trying up
    /// to `wait` seconds. Kill bypasses the lock entirely.
    async fn acquire_power_lock(&self, wait: u32) -> AppResult<tokio::sync::MutexGuard<'_, ()>> {
        if wait > 0 {
            let deadline = std::time::Instant::now() + Duration::from_secs(wait.into());
            loop {
                if let Ok(guard) = self.power_lock.try_lock() {
                    return Ok(guard);
                }
                if std::time::Instant::now() >= deadline {
                    return Err(AppError::Conflict(format!(
                        "could not acquire lock on power action after {wait} seconds"
                    )));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        } else {
            self.power_lock.try_lock().map_err(|_| {
                AppError::Conflict("failed to acquire exclusive lock for power actions".into())
            })
        }
    }

    /// Stop the container; afterwards the server is offline. Mirrors wings
    /// Stop + WaitForStop(10min, terminate=true): the request wait_seconds
    /// only bounds the power lock, never the actual stop.
    async fn stop_unlocked(&self) -> AppResult<()> {
        let name = self.uuid.to_string();
        let stop = self.process_config.read().await.stop.clone();

        match stop.r#type.as_str() {
            "signal" => {
                // Wings maps common signals and defaults to SIGKILL.
                let signal = match stop.value.to_uppercase().as_str() {
                    "SIGABRT" => "SIGABRT",
                    "SIGINT" | "C" => "SIGINT",
                    "SIGTERM" => "SIGTERM",
                    "SIGKILL" => "SIGKILL",
                    _ => "SIGKILL",
                };
                self.docker.kill(&name, signal).await?;
            }
            "command" => {
                // Only send the stop command if attached; otherwise fall
                // back to the native docker stop (wings).
                let attached = self.console_tx.read().await.clone();
                if let Some(tx) = attached {
                    if !stop.value.is_empty() {
                        let _ = tx.send(stop.value).await;
                    }
                } else {
                    self.docker.stop(&name, -1).await?;
                }
            }
            _ => {
                // "stop" (and empty): graceful docker stop, waiting as long
                // as the container needs (wings uses timeout -1).
                self.docker.stop(&name, -1).await?;
            }
        }

        // WaitForStop: up to 10 minutes for the container to actually stop,
        // then force-terminate.
        let timed_out = tokio::time::timeout(
            Duration::from_secs(600),
            async {
                use futures_util::StreamExt;
                let mut wait = self.docker.wait_until_stopped(&name);
                while let Some(item) = wait.next().await {
                    if item.is_ok() {
                        break;
                    }
                }
            },
        )
        .await
        .is_err();
        if timed_out {
            tracing::warn!(uuid = %self.uuid, "container stop did not complete in 10 minutes, terminating process");
            let _ = self.docker.kill(&name, "SIGKILL").await;
        }
        Ok(())
    }

    /// Start the container (assumes the power lock is held).
    /// Mirrors wings onBeforeStart: sync, disk check, config update, then boot.
    async fn start_unlocked(self: &Arc<Self>) -> AppResult<()> {
        if self.is_running() {
            return Ok(());
        }
        self.set_state(ServerState::Starting).await;

        // onBeforeStart: sync configuration from panel.
        if let Err(e) = self.sync_from_panel().await {
            tracing::warn!(uuid = %self.uuid, error = %e, "pre-start sync failed");
        }

        // onBeforeStart: disallow start when suspended, checked after sync so
        // we have the most up-to-date information.
        if self.suspended.load(Ordering::SeqCst) {
            self.set_state(ServerState::Offline).await;
            return Err(AppError::BadRequest(
                "Cannot start or restart a server that is suspended.".into(),
            ));
        }

        // onBeforeStart: check disk space before boot (wings HasSpaceErr).
        if !self.has_space_available().await {
            tracing::warn!(uuid = %self.uuid, "aborting start, server has run out of disk space");
            self.set_state(ServerState::Offline).await;
            return Err(AppError::BadRequest(
                "Cannot start server, server has run out of disk space.".into(),
            ));
        }

        std::fs::create_dir_all(self.fs.root())
            .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot create data dir: {e}")))?;

        // Configurable UID/GID chown.
        let (uid, gid) = {
            let daemon = self.daemon.read().await.clone();
            let u = &daemon.system.user;
            if u.rootless.enabled {
                (u.rootless.container_uid as u32, u.rootless.container_gid as u32)
            } else if u.uid != 0 && u.gid != 0 {
                (u.uid as u32, u.gid as u32)
            } else {
                (988, 988)
            }
        };
        chown_recursive(self.fs.root(), uid, gid);

        let image = self.config.read().await.container.image.clone();

        // Always destroy and re-create the container before boot so synced
        // panel data is applied and logs are truncated (wings OnBeforeStart).
        // On any failure the state must return to Offline, like wings
        // Environment.Start's deferred ProcessOffline.
        if let Err(e) = self.ensure_container_fresh().await {
            self.set_state(ServerState::Offline).await;
            return Err(e);
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

        if let Err(e) = self.docker.start(&self.uuid.to_string()).await {
            self.set_state(ServerState::Offline).await;
            return Err(e);
        }
        *self.started_at.lock().await = Some(Instant::now());
        self.set_state(ServerState::Running).await;
        // Apply resource limits in-place after boot (wings InSituUpdate).
        if let Err(e) = self.apply_limits().await {
            tracing::warn!(uuid = %self.uuid, error = %e, "could not apply resource limits");
        }
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
            // Intentional stops mark the Stopping state first (wings); only
            // unexpected exits reach crash detection.
            if watcher.intentional_stop.swap(false, Ordering::SeqCst) {
                tracing::info!(uuid = %name, "server stopped intentionally");
            } else {
                tokio::spawn(Server::handle_server_crash(watcher, code));
            }
        });
        tracing::info!(uuid = %self.uuid, "server started");
        Ok(())
    }

    async fn create_container(&self) -> AppResult<()> {
        let cfg = self.config.read().await.clone();
        let env = self.build_env().await;
        let daemon = self.daemon.read().await.clone();
        let network_ip = daemon.docker.network.interface.clone();
        // Wings server.go CreateEnvironment: write a per-server machine-id
        // (UUID without dashes) for the /etc/machine-id mount.
        if daemon.system.machine_id.enabled {
            let path = std::path::Path::new(&daemon.system.machine_id.directory)
                .join(self.uuid.to_string());
            if let Err(e) = std::fs::write(
                &path,
                format!("{}\n", self.uuid.to_string().replace('-', "")),
            ) {
                tracing::warn!(path = %path.display(), error = %e, "cannot write machine-id file");
            }
        }
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
        self.check_power_allowed()?;
        let _guard = self.acquire_power_lock(0).await?;
        self.clone().start_unlocked().await
    }

    pub async fn power_stop(&self, wait_seconds: u32) -> AppResult<()> {
        self.check_power_allowed()?;
        let _guard = self.acquire_power_lock(wait_seconds).await?;
        if !self.is_running() {
            return Ok(());
        }
        self.intentional_stop.store(true, Ordering::SeqCst);
        self.set_state(ServerState::Stopping).await;
        let result = self.stop_unlocked().await;
        self.set_state(ServerState::Offline).await;
        result
    }

    /// Kill the container immediately. Wings bypasses the power lock for
    /// kill actions so stuck servers can always be terminated (but the
    /// installing/transferring/restoring guards still apply).
    pub async fn power_kill(&self) -> AppResult<()> {
        self.check_power_allowed()?;
        if !self.is_running() {
            return Ok(());
        }
        self.intentional_stop.store(true, Ordering::SeqCst);
        self.set_state(ServerState::Stopping).await;
        let result = self.docker.kill(&self.uuid.to_string(), "SIGKILL").await;
        self.set_state(ServerState::Offline).await;
        result
    }

    pub async fn power_restart(self: &Arc<Self>, wait_seconds: u32) -> AppResult<()> {
        self.check_power_allowed()?;
        let _guard = self.acquire_power_lock(wait_seconds).await?;
        self.intentional_stop.store(true, Ordering::SeqCst);
        self.stop_unlocked().await?;
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
        let cpu_burst = daemon.docker.cpu_burst.clone();
        drop(cfg);
        // The kernel rejects a CFS quota lower than the current burst, so
        // remove the burst before updating the limits and re-apply it after
        // (wings container.go).
        let name = self.uuid.to_string();
        crate::docker::cgroup::clear_cpu_burst(&self.docker, &name).await;
        self.docker.update_resources(&name, &resources).await?;
        if cpu_burst.enabled {
            let quota = resources
                .cpu_quota
                .unwrap_or(0);
            crate::docker::cgroup::set_cpu_burst(
                &self.docker,
                &name,
                quota,
                cpu_burst.enabled,
                cpu_burst.percent,
            )
            .await;
        }
        Ok(())
    }

    pub fn is_transferring(&self) -> bool {
        self.transferring.load(Ordering::SeqCst)
    }

    pub fn set_transferring(&self, value: bool) {
        self.transferring.store(value, Ordering::SeqCst);
    }

    pub fn is_restoring(&self) -> bool {
        self.restoring.load(Ordering::SeqCst)
    }

    pub fn set_restoring(&self, value: bool) {
        self.restoring.store(value, Ordering::SeqCst);
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
    /// Mirrors wings deleteServer: suspend, notify clients (Deleted /
    /// TransferStatus), destroy environment, remove files.
    pub async fn delete_container(&self, remove_data: bool) -> AppResult<()> {
        self.suspended.store(true, Ordering::SeqCst);

        if self.is_transferring() {
            self.publish(ServerEvent::TransferStatus("completed".into()));
        }
        self.publish(ServerEvent::Deleted);
        self.cancel_transfer_task().await;
        crate::router::downloader::cancel_for_server(self.uuid).await;
        // Bypass power guards: panel-initiated cleanup must always work.
        self.intentional_stop.store(true, Ordering::SeqCst);
        self.set_state(ServerState::Stopping).await;
        let _ = self.docker.kill(&self.uuid.to_string(), "SIGKILL").await;
        self.set_state(ServerState::Offline).await;
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

/// Recursively chown a directory tree. Symlinks are chowned as links and
/// never descended or followed (wings WalkDirat + Lchownat with
/// AT_SYMLINK_NOFOLLOW).
fn chown_recursive(path: &std::path::Path, uid: u32, gid: u32) {
    use std::os::unix::fs::lchown as unix_lchown;
    let _ = unix_lchown(path, Some(uid), Some(gid));
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            let _ = unix_lchown(&p, Some(uid), Some(gid));
            if let Ok(meta) = std::fs::symlink_metadata(&p) {
                if meta.is_dir() {
                    chown_recursive(&p, uid, gid);
                }
            }
        }
    }
}

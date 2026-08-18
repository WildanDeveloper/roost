use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bollard::container::AttachContainerResults;
use futures_util::StreamExt;

use crate::error::{AppError, AppResult};
use crate::server::events::ServerEvent;

use super::Server;

/// Removes the installer container and staging directory when the install
/// function exits, even on failure (wings cleans up on abort).
struct InstallerCleanup {
    docker: crate::docker::DockerClient,
    name: String,
    tmp_dir: std::path::PathBuf,
}

impl Drop for InstallerCleanup {
    fn drop(&mut self) {
        let docker = self.docker.clone();
        let name = self.name.clone();
        let tmp_dir = self.tmp_dir.clone();
        tokio::spawn(async move {
            let _ = docker.remove(&name).await;
            let _ = std::fs::remove_dir_all(&tmp_dir);
        });
    }
}

impl Server {
    /// Run the egg install script for this server. Stops the server first,
    /// runs the `<uuid>_installer` container, then reports the outcome to
    /// the panel. Mirrors wings: completion is "container stopped" — the
    /// exit code is not inspected.
    pub async fn install(self: &Arc<Self>, reinstall: bool) {
        if self.installing.swap(true, Ordering::SeqCst) {
            tracing::warn!(uuid = %self.uuid, "install already in progress");
            return;
        }

        // Make sure the server isn't running while installing. Wings
        // aborts the install when the server cannot be stopped
        // (WaitForStop, 2 minutes); installing over a live process would
        // corrupt the data directory.
        if self.is_running() {
            if let Err(e) = self.power_stop(30).await {
                self.publish(ServerEvent::DaemonMessage(format!(
                    "Installation failed; server could not be stopped: {e}"
                )));
                self.publish(ServerEvent::InstallCompleted);
                self.installing.store(false, Ordering::SeqCst);
                self.set_state(crate::server::ServerState::Offline).await;
                return;
            }
        }
        // Abort any active SFTP sessions; the data directory is about to
        // change underneath them (mirrors wings `Sftp().CancelAll()`).
        crate::sftp::cancel_sessions_for(&self.uuid.to_string()).await;
        let _ = self.sync_from_panel().await;

        // Wings publishes InstallStarted before running the script (only
        // when the egg scripts actually run) so the panel can update early.
        if !self.config.read().await.skip_egg_scripts {
            self.publish(ServerEvent::InstallStarted);
        }

        self.publish(ServerEvent::DaemonMessage(
            "Starting installation process, this could take a few minutes...".to_string(),
        ));

        let result = Self::run_install_script(self).await;

        let successful = result.is_ok();
        if let Err(e) = &result {
            tracing::error!(uuid = %self.uuid, error = %e, "install failed");
        }

        // Report to the panel.
        let _ = self
            .panel
            .read()
            .await
            .post_install_status(self.uuid, successful, reinstall)
            .await;

        if successful {
            self.publish(ServerEvent::DaemonMessage(
                "Installation completed successfully.".to_string(),
            ));
        } else {
            self.publish(ServerEvent::DaemonMessage(
                "Installation failed; the panel has been notified.".to_string(),
            ));
        }

        // Wings publishes InstallCompleted on success AND failure (the panel
        // listens for it to stop the "installing" spinner).
        self.publish(ServerEvent::InstallCompleted);

        self.installing.store(false, Ordering::SeqCst);

        // Post-install: ensure server is in offline state (wings does this).
        self.set_state(crate::server::ServerState::Offline).await;
    }

    async fn run_install_script(self: &Arc<Self>) -> AppResult<()> {
        let cfg = self.config.read().await.clone();

        if cfg.skip_egg_scripts {
            tracing::info!(uuid = %self.uuid, "skipping egg install scripts");
            return Ok(());
        }

        // 1. Fetch the script config from the panel.
        let script = self
            .panel
            .read()
            .await
            .get_install_script(self.uuid)
            .await
            .map_err(|e| AppError::Remote(format!("cannot fetch install script: {e}")))?;

        // 2. Stage the install script.
        let daemon = self.daemon.read().await.clone();
        let tmp_dir = daemon.tmp_dir().join(self.uuid.to_string());
        std::fs::create_dir_all(&tmp_dir)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot create install dir: {e}")))?;
        let script_path = tmp_dir.join("install.sh");
        let normalized = script.script.replace("\r\n", "\n");
        std::fs::write(&script_path, normalized)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot write install script: {e}")))?;
        tracing::info!(uuid = %self.uuid, path = %script_path.display(), "install script staged");

        // 3. Pull the installer image (fall back to local copy). Wings
        // publishes a daemon message while the pull runs (listeners.go).
        self.publish(ServerEvent::DaemonMessage(
            "Pulling Docker container image, this could take a few minutes to complete...".to_string(),
        ));
        if let Err(e) = self.docker.pull_image(&script.container_image, &daemon.docker).await {
            tracing::warn!(uuid = %self.uuid, image = %script.container_image, error = %e, "could not pull installer image");
        }

        // 4. Recreate the installer container. The cleanup guard makes
        // sure the container and temp files are removed even when a later
        // step fails (wings removes the installer container on abort).
        let installer_name = format!("{}_installer", self.uuid);
        tracing::info!(uuid = %self.uuid, "creating installer container");
        self.docker.remove(&installer_name).await?;
        let _cleanup = InstallerCleanup {
            docker: self.docker.clone(),
            name: installer_name.clone(),
            tmp_dir: tmp_dir.clone(),
        };

        let env = self.build_env().await;
        let network_ip = daemon.docker.network.interface.clone();
        let data_dir = self.fs.root().to_path_buf();
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot create server data dir: {e}")))?;
        std::fs::create_dir_all(&tmp_dir)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot create install tmp dir: {e}")))?;

        self.docker
            .create_installer_container(
                self.uuid,
                &cfg,
                &data_dir,
                &tmp_dir,
                &script.container_image,
                &script.entrypoint,
                &env,
                &daemon,
                &network_ip,
            )
            .await?;

        // 5. Install log file.
        let log_path = daemon.log_dir().join("install").join(format!("{}.log", self.uuid));
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut log_file = std::fs::File::create(&log_path).ok();

        // Write install metadata header (wings writes UUID, image, entrypoint, env vars).
        if let Some(ref mut file) = log_file {
            let _ = writeln!(file, "─────────────────────────────────────────────────");
            let _ = writeln!(file, "Server UUID: {}", self.uuid);
            let _ = writeln!(file, "Image: {}", script.container_image);
            let _ = writeln!(file, "Entrypoint: {}", script.entrypoint);
            let _ = writeln!(file, "─────────────────────────────────────────────────");
        }

        // 6. Stream output to websockets + the log file.
        if let Ok(attach) = self.docker.attach(&installer_name).await {
            let server = self.clone();
            tokio::spawn(async move {
                Self::stream_install_output(attach, server, log_file).await;
            });
        }

        // 7. Start and wait for the container to stop.
        tracing::info!(uuid = %self.uuid, "starting installer container");
        self.docker.start(&installer_name).await?;
        tracing::info!(uuid = %self.uuid, "installer container started; waiting for exit");

        // Wings applies the CPU burst to the installer container too
        // (SetCpuBurst after ContainerStart).
        if daemon.docker.cpu_burst.enabled {
            let resources = cfg.build.as_container_resources(&daemon.docker, true);
            let quota = resources.cpu_quota.unwrap_or(0);
            crate::docker::cgroup::set_cpu_burst(
                &self.docker,
                &installer_name,
                quota,
                daemon.docker.cpu_burst.enabled,
                daemon.docker.cpu_burst.percent,
            )
            .await;
        }

        {
            use futures_util::StreamExt;
            let mut wait = self.docker.wait_until_stopped(&installer_name);
            let _ = wait.next().await;
        }

        // 8. Cleanup.
        tracing::info!(uuid = %self.uuid, "installer container finished; cleaning up");
        self.docker.remove(&installer_name).await?;
        let _ = std::fs::remove_dir_all(&tmp_dir);

        Ok(())
    }

    async fn stream_install_output(
        attach: AttachContainerResults,
        server: Arc<Server>,
        mut log_file: Option<std::fs::File>,
    ) {
        let mut output = attach.output;
        while let Some(item) = output.next().await {
            let bytes = match item {
                Ok(line) => line.into_bytes(),
                Err(_) => break,
            };
            let text = String::from_utf8_lossy(&bytes);
            for line in text.split_inclusive('\n') {
                let clean = line.trim_end_matches(['\r', '\n']).to_string();
                if clean.is_empty() {
                    continue;
                }
                server.publish(ServerEvent::InstallOutput(clean.clone()));
                if let Some(file) = &mut log_file {
                    let _ = writeln!(file, "{clean}");
                }
            }
        }
    }
}
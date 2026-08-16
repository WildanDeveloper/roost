use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bollard::container::AttachContainerResults;
use futures_util::StreamExt;

use crate::error::{AppError, AppResult};
use crate::server::events::ServerEvent;

use super::Server;

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

        // Make sure the server isn't running while installing.
        if self.is_running() {
            let _ = self.power_stop(30).await;
        }
        let _ = self.sync_from_panel().await;

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
            self.publish(ServerEvent::InstallCompleted);
            self.publish(ServerEvent::DaemonMessage(
                "Installation completed successfully.".to_string(),
            ));
        } else {
            self.publish(ServerEvent::DaemonMessage(
                "Installation failed; the panel has been notified.".to_string(),
            ));
        }

        self.installing.store(false, Ordering::SeqCst);
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

        // 3. Pull the installer image (fall back to local copy).
        if let Err(e) = self.docker.pull_image(&script.container_image, &daemon.docker).await {
            tracing::warn!(uuid = %self.uuid, image = %script.container_image, error = %e, "could not pull installer image");
        }

        // 4. Recreate the installer container.
        let installer_name = format!("{}_installer", self.uuid);
        tracing::info!(uuid = %self.uuid, "creating installer container");
        self.docker.remove(&installer_name).await?;

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

        self.publish(ServerEvent::InstallStarted);

        // 5. Install log file.
        let log_path = daemon.log_dir().join("install").join(format!("{}.log", self.uuid));
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let log_file = std::fs::File::create(&log_path).ok();

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
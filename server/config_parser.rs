//! Egg configuration file application — port of wings
//! `server/config_parser.go` (`UpdateConfigurationFiles`).
//!
//! Runs as part of `onBeforeStart` (and after installs) so every config
//! file shipped by the egg is rewritten with the current panel data before
//! the process boots. One broken file must never prevent the server from
//! starting: errors are logged and the loop continues (wings behavior).

use std::sync::Arc;

use super::Server;
use crate::parser::TemplatableConfig;

impl Server {
    pub async fn update_configuration_files(self: &Arc<Self>) {
        let configs = self.process_config.read().await.configs.clone();
        if configs.is_empty() {
            return;
        }

        let interface = self
            .daemon
            .read()
            .await
            .docker
            .network
            .interface
            .clone();
        let daemon = TemplatableConfig::from_daemon(&interface);

        for config in configs {
            // Resolve inside the server data dir (path traversal guard).
            let path = match self.fs.resolve(&config.file) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(uuid = %self.uuid, file = %config.file, error = %e, "skipping unsafe config file path");
                    continue;
                }
            };

            // wings: plain "file"-parser files are skipped when they do not
            // exist; every other parser type gets the file touched so the
            // egg's config always exists before boot.
            let exists = path.exists();
            if !exists && config.parser == "file" {
                continue;
            }
            if !exists {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = std::fs::File::create(&path) {
                    tracing::warn!(uuid = %self.uuid, file = %path.display(), error = %e, "failed to create missing config file");
                    continue;
                }
            }

            if let Err(e) = crate::parser::apply(&path, &config, &daemon) {
                tracing::error!(uuid = %self.uuid, file = %config.file, error = %e, "failed to parse and update server configuration file");
            }
        }
    }
}

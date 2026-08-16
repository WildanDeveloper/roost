pub mod types;

use std::time::Duration;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use types::{InstallScriptResponse, ServerConfigurationResponse};

/// Client for the Panel's remote (daemon-facing) API.
///
/// Auth: `Authorization: Bearer <token_id>.<token>` — the panel splits on
/// the dot, looks up the node by `token_id` and compares `token` against
/// the stored daemon token (DaemonAuthenticate middleware).
#[derive(Clone)]
pub struct PanelClient {
    inner: reqwest::Client,
    base: String,
    token_id: String,
    token: String,
}

impl PanelClient {
    pub fn new(panel_url: &str, token_id: &str, token: &str) -> anyhow::Result<Self> {
        let inner = reqwest::Client::builder()
            .http1_only()
            .timeout(Duration::from_secs(15))
            .user_agent(format!("Pterodactyl Wings/v1.13.3 (id:{token_id})"))
            .build()?;

        Ok(Self {
            inner,
            base: format!("{}/api/remote", panel_url.trim_end_matches('/')),
            token_id: token_id.to_string(),
            token: token.to_string(),
        })
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}.{}", self.token_id, self.token)
    }

    /// Reset installing/restoring states after a daemon boot.
    pub async fn reset_servers(&self) -> AppResult<()> {
        self.request(reqwest::Method::POST, "/servers/reset", None::<&()>)
            .await
            .map(|_: serde_json::Value| ())
    }

    /// Fetch all servers on this node, following pagination.
    pub async fn list_servers(&self, per_page: u64) -> AppResult<Vec<types::RawServerData>> {
        let mut servers = Vec::new();
        let mut page = 0usize;

        loop {
            let url = format!("/servers?page={page}&per_page={per_page}");
            let resp: types::ServerListResponse = self.request(reqwest::Method::GET, &url, None::<&()>).await?;
            servers.extend(resp.data);

            let last = resp.meta.last_page.max(1);
            let current = resp.meta.current_page.max(1);

            if current >= last || servers.is_empty() {
                break;
            }
            page = (current + 1) as usize;
        }

        Ok(servers)
    }

    /// Fetch the full configuration for one server.
    pub async fn get_server(&self, uuid: Uuid) -> AppResult<ServerConfigurationResponse> {
        self.request(reqwest::Method::GET, &format!("/servers/{uuid}"), None::<&()>)
            .await
    }

    /// Fetch the egg install script descriptor.
    pub async fn get_install_script(&self, uuid: Uuid) -> AppResult<InstallScriptResponse> {
        self.request(reqwest::Method::GET, &format!("/servers/{uuid}/install"), None::<&()>)
            .await
    }

    /// Report the outcome of an install/reinstall to the panel.
    pub async fn post_install_status(
        &self,
        uuid: Uuid,
        successful: bool,
        reinstall: bool,
    ) -> AppResult<()> {
        self.request(
            reqwest::Method::POST,
            &format!("/servers/{uuid}/install"),
            Some(&types::InstallStatusRequest { successful, reinstall }),
        )
        .await
        .map(|_: serde_json::Value| ())
    }

    /// Report the outcome of an archive/restore operation.
    #[allow(dead_code)]
pub async fn post_archive_status(&self, uuid: Uuid, successful: bool) -> AppResult<()> {
        self.request(
            reqwest::Method::POST,
            &format!("/servers/{uuid}/archive"),
            Some(&types::StatusMessage { successful }),
        )
        .await
        .map(|_: serde_json::Value| ())
    }

    /// Report a completed backup to the panel.
    pub async fn post_backup_status(
        &self,
        uuid: Uuid,
        status: &types::BackupRemoteStatus,
    ) -> AppResult<()> {
        self.request(
            reqwest::Method::POST,
            &format!("/backups/{uuid}"),
            Some(status),
        )
        .await
        .map(|_: serde_json::Value| ())
    }

    /// Report a completed backup restore.
    pub async fn post_backup_restore_status(&self, uuid: Uuid, successful: bool) -> AppResult<()> {
        self.request(
            reqwest::Method::POST,
            &format!("/backups/{uuid}/restore"),
            Some(&types::StatusMessage { successful }),
        )
        .await
        .map(|_: serde_json::Value| ())
    }

    /// Fetch the URLs (with tokens) to download a backup archive from the
    /// panel's S3 storage.
    #[allow(dead_code)]
pub async fn get_backup_download_urls(&self, uuid: Uuid) -> AppResult<types::BackupParts> {
        self.request(reqwest::Method::GET, &format!("/backups/{uuid}"), None::<&()>)
            .await
    }

    /// Low-level request with retry. Wings retries 5xx and transport
    /// errors with exponential backoff, capped at ~30s; 4xx is permanent.
    async fn request<T: serde::de::DeserializeOwned, B: serde::Serialize>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> AppResult<T> {
        let mut delay = Duration::from_millis(50);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);

        loop {
            let mut builder = self
                .inner
                .request(method.clone(), format!("{}{}", self.base, path))
                .header("Authorization", self.auth_header())
                .header("Accept", "application/vnd.pterodactyl.v1+json");

            if body.is_some() {
                builder = builder.header("Content-Type", "application/json");
            }

            let result = match body {
                Some(b) => builder.json(b).send().await,
                None => builder.send().await,
            };

            match result {
                Ok(resp) => {
                    let status = resp.status();
                    if status == reqwest::StatusCode::NO_CONTENT {
                        return serde_json::from_str("null").map_err(|e| {
                            AppError::Remote(format!("bad panel response: {e}"))
                        });
                    }
                    if status.is_success() {
                        let len = resp.content_length();
                        let ct = resp
                            .headers()
                            .get(reqwest::header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("?")
                            .to_string();
                        let text = resp.text().await.unwrap_or_default();
                        tracing::debug!(path, status = %status, len, ct, body_len = text.len(), "panel response");
                        return serde_json::from_str::<T>(&text).map_err(|e| {
                            AppError::Remote(format!(
                                "bad panel response: {e}; body: {}",
                                &text[..text.len().min(4000)]
                            ))
                        });
                    }
                    if status.is_client_error() {
                        let text = resp.text().await.unwrap_or_default();
                        return Err(AppError::Remote(format!("panel {status}: {text}")));
                    }
                    // server error -> retry
                }
                Err(e) => {
                    tracing::warn!(path, error = %e, "panel request failed (transport)");
                }
            }

            if std::time::Instant::now() + delay >= deadline {
                return Err(AppError::Remote(format!("panel request {path} failed after retries")));
            }
            tokio::time::sleep(delay).await;
            delay = std::cmp::min(delay * 2, Duration::from_secs(12));
        }
    }
}

/// Helper used by tests/tools: check auth header format matches what the
/// panel expects (`token_id.token`, exactly two non-empty parts).
#[allow(dead_code)]
pub fn is_valid_daemon_auth(header: &str, node_id: &str, node_token: &str) -> bool {
    let mut parts = header.split('.');
    let id = parts.next().unwrap_or("");
    let token = parts.next().unwrap_or("");
    let trailing = parts.next();
    !id.is_empty() && !token.is_empty() && trailing.is_none() && id == node_id && token == node_token
}
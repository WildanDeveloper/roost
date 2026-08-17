mod auth;
mod config;
mod docker;
mod error;
mod jwt;
mod models;
mod remote;
mod router;
mod server;
mod sftp;
mod state;

use std::net::SocketAddr;

fn init_tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use config::Config;
use docker::DockerClient;
use jwt::TokenStore;
use remote::PanelClient;
use server::activity::ActivityCollector;
use server::ServerManager;
use state::{DaemonState, SharedConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config_path = std::env::var("ROOST_CONFIG").unwrap_or_else(|_| "/etc/pterodactyl/config.yml".into());
    let config = Config::load(&config_path)?;
    config.ensure_directories()?;

    let filter = if config.debug {
        "roost=debug,tower_http=debug".to_string()
    } else {
        "roost=info,tower_http=info".to_string()
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    tracing::info!("roost starting (config: {config_path})");

    let docker = DockerClient::connect()?;
    docker.ping().await.map_err(|e| anyhow::anyhow!("docker is not available: {e}"))?;

    let shared: SharedConfig = Arc::new(RwLock::new(config));

    {
        let cfg = shared.read().await;
        docker.ensure_network(&cfg.docker.network).await?;
    }

    let panel_client = {
        let cfg = shared.read().await;
        PanelClient::new(&cfg.remote, &cfg.token_id, &cfg.token)
            .map_err(|e| anyhow::anyhow!("cannot build panel client: {e}"))?
    };
    let panel = Arc::new(RwLock::new(panel_client));

    init_tls();

    let boot_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let tokens = Arc::new(TokenStore::with_ttl(Duration::from_secs(3600)));
    let manager = Arc::new(ServerManager::new(docker, shared.clone(), panel.clone()));
    manager.boot().await?;

    let (crash_tx, mut crash_rx) = tokio::sync::mpsc::unbounded_channel::<Arc<server::Server>>();
    let _ = server::CRASH_RESTART_TX.set(crash_tx);
    tokio::spawn(async move {
        while let Some(srv) = crash_rx.recv().await {
            if let Err(e) = srv.power_start().await {
                tracing::warn!(uuid = %srv.uuid, error = %e, "failed to restart after crash");
            }
        }
    });

    let activity = Arc::new(ActivityCollector::new());
    {
        let cfg = shared.read().await;
        let interval = std::time::Duration::from_secs(cfg.system.activity_send_interval.max(1));
        let count = cfg.system.activity_send_count.max(1);
        tokio::spawn(activity.clone().flush_task(panel.clone(), interval, count));
    }

    {
        let cfg = shared.read().await;
        let bind = format!(
            "{}:{}",
            cfg.system.sftp.bind_address,
            if cfg.system.sftp.bind_port == 0 { 2022 } else { cfg.system.sftp.bind_port }
        );
        let data_dir = std::path::PathBuf::from(cfg.system.data.clone());
        let sftp_server = Arc::new(sftp::server::SftpServer::new(
            manager.clone(),
            activity.clone(),
            &cfg.system.sftp,
            data_dir,
        )?);
        sftp::set_sftp_server(sftp_server.clone());
        tokio::spawn(sftp_server.run(bind));
    }

    let state = DaemonState {
        config: shared.clone(),
        manager,
        panel,
        tokens,
        boot_time,
        activity,
    };

    let app = router::build(state);
    let addr: SocketAddr = {
        let cfg = shared.read().await;
        cfg.bind_address().parse()?
    };

    if shared.read().await.api.ssl.enabled {
        let (cert, key) = {
            let cfg = shared.read().await;
            (cfg.api.ssl.cert.clone(), cfg.api.ssl.key.clone())
        };
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key).await?;
        tracing::info!("roost API listening on https://{addr}");
        axum_server::bind_rustls(addr, tls)
            .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .await?;
    } else {
        tracing::info!("roost API listening on http://{addr}");
        axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;
    }

    Ok(())
}
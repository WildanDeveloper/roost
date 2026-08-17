use bollard::auth::DockerCredentials;
use bollard::container::{
    AttachContainerOptions, AttachContainerResults, Config as ContainerConfig,
    CreateContainerOptions, KillContainerOptions, ListContainersOptions, LogOutput, LogsOptions,
    Stats, StatsOptions, StopContainerOptions, UpdateContainerOptions, WaitContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::models::{
    ContainerInspectResponse, HostConfig, Ipam, IpamConfig, Mount, MountTypeEnum, PortBinding,
};
use bollard::network::{CreateNetworkOptions, InspectNetworkOptions};
use bollard::Docker;
use futures_util::stream::Stream;
use std::collections::HashMap;
use uuid::Uuid;

use crate::config::{Config, DockerConfig, DockerNetworkConfig, RegistryConfig};
use crate::docker::settings::ContainerResources;
use crate::error::{AppError, AppResult};
use crate::models::ServerConfig;

pub const LABEL_SERVICE: &str = "Service";
pub const LABEL_TYPE: &str = "ContainerType";
pub const LABEL_TYPE_SERVER: &str = "server_process";
pub const LABEL_TYPE_INSTALLER: &str = "server_installer";

/// Thin wrapper around the bollard Docker client. Everything server code
/// needs goes through here so it never touches bollard types directly.
#[derive(Clone)]
pub struct DockerClient {
    inner: Docker,
}

impl DockerClient {
    /// Connect using DOCKER_HOST env or the default unix socket.
    pub fn connect() -> anyhow::Result<Self> {
        let docker = Docker::connect_with_local_defaults()?;
        Ok(Self { inner: docker })
    }

    /// Docker engine version (wings GetDockerInfo).
    pub async fn engine_version(&self) -> AppResult<bollard::system::Version> {
        self.inner.version().await.map_err(AppError::Docker)
    }

    /// Docker engine system information (wings GetDockerInfo).
    pub async fn engine_info(&self) -> AppResult<bollard::models::SystemInfo> {
        self.inner.info().await.map_err(AppError::Docker)
    }

    pub async fn ping(&self) -> AppResult<()> {
        self.inner.ping().await.map_err(AppError::Docker)?;
        Ok(())
    }

    #[allow(dead_code)]
pub async fn version(&self) -> AppResult<String> {
        let v = self.inner.version().await.map_err(AppError::Docker)?;
        Ok(v.version.unwrap_or_default())
    }

    #[allow(dead_code)]
pub async fn inspect_container(&self, name: &str) -> AppResult<Option<ContainerInspectResponse>> {
        match self
            .inner
            .inspect_container(name, None::<bollard::container::InspectContainerOptions>)
            .await
        {
            Ok(resp) => Ok(Some(resp)),
            Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => Ok(None),
            Err(e) => Err(AppError::Docker(e)),
        }
    }

    /// Whether the container was killed due to OOM (wings crash.go uses
    /// this to decide if an exit counts as a crash).
    pub async fn container_was_oom_killed(&self, name: &str) -> AppResult<bool> {
        let Some(resp) = self.inspect_container(name).await? else {
            return Ok(false);
        };
        Ok(resp
            .state
            .and_then(|s| s.oom_killed)
            .unwrap_or(false))
    }

    /// Create the daemon bridge network if it does not exist.
    pub async fn ensure_network(&self, net: &DockerNetworkConfig) -> AppResult<()> {
        if !net.name.is_empty() {
            match self
                .inner
                .inspect_network(&net.name, None::<InspectNetworkOptions<String>>)
                .await
            {
                Ok(_) => return Ok(()),
                Err(_) => {}
            }
        }

        let mut ipam_config = Vec::new();
        if !net.interfaces.v4.subnet.is_empty() {
            ipam_config.push(IpamConfig {
                subnet: Some(net.interfaces.v4.subnet.clone()),
                ip_range: None,
                gateway: if net.interfaces.v4.gateway.is_empty() {
                    None
                } else {
                    Some(net.interfaces.v4.gateway.clone())
                },
                auxiliary_addresses: None,
            });
        }
        if !net.interfaces.v6.subnet.is_empty() {
            ipam_config.push(IpamConfig {
                subnet: Some(net.interfaces.v6.subnet.clone()),
                ip_range: None,
                gateway: if net.interfaces.v6.gateway.is_empty() {
                    None
                } else {
                    Some(net.interfaces.v6.gateway.clone())
                },
                auxiliary_addresses: None,
            });
        }

        let name = if net.name.is_empty() {
            "pterodactyl_nw".to_string()
        } else {
            net.name.clone()
        };

        let mut options = HashMap::new();
        options.insert(
            "com.docker.network.bridge.name".to_string(),
            "pterodactyl0".to_string(),
        );
        options.insert(
            "com.docker.network.bridge.enable_icc".to_string(),
            net.enable_icc.to_string(),
        );
        options.insert(
            "com.docker.network.driver.mtu".to_string(),
            net.network_mtu.to_string(),
        );
        options.insert(
            "com.docker.network.bridge.host_binding_ipv4".to_string(),
            "0.0.0.0".to_string(),
        );

        let mut labels = HashMap::new();
        labels.insert("Service".to_string(), "Pterodactyl".to_string());

        let options = CreateNetworkOptions::<String> {
            name: name.clone(),
            driver: net.driver.clone(),
            check_duplicate: true,
            internal: net.is_internal,
            attachable: false,
            ingress: false,
            enable_ipv6: !net.interfaces.v6.subnet.is_empty(),
            ipam: Ipam {
                driver: Some("default".to_string()),
                config: Some(ipam_config),
                options: None,
            },
            options,
            labels,
        };

        match self.inner.create_network(options).await {
            Ok(_) => {
                tracing::info!(name, "docker network created");
                Ok(())
            }
            Err(bollard::errors::Error::DockerResponseServerError { status_code: 409, .. }) => Ok(()),
            Err(e) => Err(AppError::Docker(e)),
        }
    }

    /// Pull an image if it isn't available locally. Mirrors wings: on
    /// failure to pull, fall back to the locally cached image. A `~`
    /// prefix on the image name skips the pull entirely (local-only image).
    pub async fn pull_image(&self, image: &str, config: &DockerConfig) -> AppResult<()> {
        // ~ prefix = local-only image, skip pull.
        if image.starts_with('~') {
            tracing::debug!(image, "skipping pull for local-only image (~ prefix)");
            return Ok(());
        }
        if self.image_exists(image).await? {
            return Ok(());
        }

        let registry = registry_auth_for(image, &config.registries);
        let (from_image, tag) = split_image_tag(image);

        let options = Some(CreateImageOptions {
            from_image: from_image.to_string(),
            from_src: String::new(),
            repo: String::new(),
            tag: tag.map(|t| t.to_string()).unwrap_or_default(),
            platform: String::new(),
            changes: Vec::new(),
        });

        let mut stream = self.inner.create_image(options, None, registry);
        use futures_util::StreamExt;
        while let Some(item) = stream.next().await {
            if let Err(e) = item {
                tracing::warn!(image, error = %e, "image pull failed, using local copy if available");
                if self.image_exists(image).await? {
                    return Ok(());
                }
                return Err(AppError::Docker(e));
            }
        }
        Ok(())
    }

    async fn image_exists(&self, image: &str) -> AppResult<bool> {
        match self.inner.inspect_image(image).await {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// Create the production container for a server. Does not start it.
    pub async fn create_server_container(
        &self,
        server_uuid: Uuid,
        cfg: &ServerConfig,
        data_dir: &std::path::Path,
        daemon: &Config,
        env: &[String],
        network_ip: &str,
    ) -> AppResult<String> {
        let resources = cfg.build.as_container_resources(&daemon.docker, false);

        let (config, _host) = build_container_config(
            server_uuid.to_string(),
            cfg,
            &cfg.build,
            resources,
            daemon,
            env,
            network_ip,
            data_dir,
            false,
        );

        let options = Some(CreateContainerOptions {
            name: server_uuid.to_string(),
            platform: None,
        });

        let result = self.inner.create_container(options, config).await?;
        Ok(result.id)
    }

    /// Create the temporary install container for a server.
    pub async fn create_installer_container(
        &self,
        server_uuid: Uuid,
        cfg: &ServerConfig,
        data_dir: &std::path::Path,
        tmp_dir: &std::path::Path,
        image: &str,
        entrypoint: &str,
        env: &[String],
        daemon: &Config,
        network_ip: &str,
    ) -> AppResult<String> {
        let resources = cfg.build.as_container_resources(&daemon.docker, true);

        let (config, host) = build_container_config(
            server_uuid.to_string(),
            cfg,
            &cfg.build,
            resources,
            daemon,
            env,
            network_ip,
            data_dir,
            true,
        );

        let mut config = config;
        config.image = Some(image.to_string());
        config.cmd = Some(vec![entrypoint.to_string(), "/mnt/install/install.sh".to_string()]);
        if let Some(labels) = &mut config.labels {
            labels.insert(LABEL_TYPE.to_string(), LABEL_TYPE_INSTALLER.to_string());
        }

        // Installer mounts: /mnt/server -> data dir, /mnt/install -> tmp dir.
        let mut mounts = vec![
            mount(tmp_dir, "/mnt/install", false),
            mount(data_dir, "/mnt/server", false),
        ];
        if let Some(existing) = &host.mounts {
            for m in existing {
                mounts.push(m.clone());
            }
        }
        let mut host = host;
        // Wings removes PIDs limit for installer containers.
        host.pids_limit = None;
        host.mounts = Some(mounts);
        config.host_config = Some(host);

        let options = Some(CreateContainerOptions {
            name: format!("{}_installer", server_uuid),
            platform: None,
        });

        let result = self.inner.create_container(options, config).await?;
        Ok(result.id)
    }

    pub async fn start(&self, name: &str) -> AppResult<()> {
        self.inner
            .start_container(name, None::<bollard::container::StartContainerOptions<String>>)
            .await
            .map_err(AppError::Docker)
    }

    pub async fn stop(&self, name: &str, timeout: u32) -> AppResult<()> {
        let options = StopContainerOptions {
            t: timeout as i64,
        };
        self.inner
            .stop_container(name, Some(options))
            .await
            .map_err(AppError::Docker)
    }

    pub async fn kill(&self, name: &str, signal: &str) -> AppResult<()> {
        let options = Some(KillContainerOptions {
            signal: signal.to_string(),
        });
        self.inner.kill_container(name, options).await.map_err(AppError::Docker)
    }

    pub async fn remove(&self, name: &str) -> AppResult<()> {
        let options = Some(bollard::container::RemoveContainerOptions {
            v: true,
            force: true,
            link: false,
        });
        match self.inner.remove_container(name, options).await {
            Ok(_) => Ok(()),
            Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => Ok(()),
            Err(e) => Err(AppError::Docker(e)),
        }
    }

    /// Attach to a container's stdin/stdout. Must be called before start.
    pub async fn attach(&self, name: &str) -> AppResult<AttachContainerResults> {
        let options = Some(AttachContainerOptions::<String> {
            stdin: Some(true),
            stdout: Some(true),
            stderr: Some(true),
            stream: Some(true),
            logs: Some(false),
            detach_keys: None,
        });
        self.inner.attach_container(name, options).await.map_err(AppError::Docker)
    }

    /// Tail container logs without following.
    pub fn logs_tail(
        &self,
        name: &str,
        lines: u32,
    ) -> impl Stream<Item = Result<LogOutput, bollard::errors::Error>> {
        let options = Some(LogsOptions::<String> {
            follow: false,
            stdout: true,
            stderr: true,
            tail: lines.to_string(),
            ..Default::default()
        });
        self.inner.logs(name, options)
    }

    /// One-shot container stats.
    pub async fn stats_one_shot(&self, name: &str) -> AppResult<Stats> {
        use futures_util::StreamExt;
        let options = Some(StatsOptions {
            stream: false,
            one_shot: true,
        });
        let mut stream = self.inner.stats(name, options);
        match stream.next().await {
            Some(Ok(stats)) => Ok(stats),
            Some(Err(e)) => Err(AppError::Docker(e)),
            None => Err(AppError::BadRequest("no stats available".into())),
        }
    }

    /// Stream of wait events; completes when the container stops.
    pub fn wait_until_stopped(
        &self,
        name: &str,
    ) -> impl Stream<Item = Result<bollard::models::ContainerWaitResponse, bollard::errors::Error>> {
        let options = Some(WaitContainerOptions {
            condition: "not-running".to_string(),
        });
        self.inner.wait_container(name, options)
    }

    /// Update resources of a running container in place.
    pub async fn update_resources(&self, name: &str, resources: &ContainerResources) -> AppResult<()> {
        let mut opts = UpdateContainerOptions::<String> {
            memory: resources.memory,
            memory_swap: resources.memory_swap,
            memory_reservation: resources.memory_reservation,
            oom_kill_disable: resources.oom_kill_disable,
            pids_limit: resources.pids_limit,
            blkio_weight: resources.blkio_weight,
            cpu_quota: resources.cpu_quota,
            cpu_period: resources.cpu_period,
            cpu_shares: resources.cpu_shares.map(|v| v as isize),
            cpuset_cpus: resources.cpuset_cpus.clone(),
            ..Default::default()
        };
        if opts.memory.is_none() {
            opts.memory = Some(-1);
        }
        self.inner.update_container(name, opts).await.map_err(AppError::Docker)
    }

    /// List all pterodactyl-managed containers on this node.
    pub async fn list_managed_containers(&self) -> AppResult<Vec<String>> {
        let mut filters = HashMap::new();
        filters.insert("label".to_string(), vec![format!("{LABEL_SERVICE}=Pterodactyl")]);

        let options = Some(ListContainersOptions::<String> {
            all: true,
            filters,
            ..Default::default()
        });

        let containers = self.inner.list_containers(options).await?;
        Ok(containers
            .into_iter()
            .filter_map(|c| {
                c.names
                    .and_then(|n| n.into_iter().next())
                    .map(|n| n.trim_start_matches('/').to_string())
            })
            .collect())
    }
}

/// Rewrite an allocation IP so it can be bound by the container.
/// 127.0.0.1 bindings point at the docker network interface instead.
pub fn rewrite_allocation_ip(ip: &str, network_ip: &str) -> String {
    if ip == "127.0.0.1" || ip == "0.0.0.0" {
        network_ip.to_string()
    } else {
        ip.to_string()
    }
}

fn mount(source: &std::path::Path, target: &str, read_only: bool) -> Mount {
    Mount {
        target: Some(target.to_string()),
        source: Some(source.display().to_string()),
        typ: Some(MountTypeEnum::BIND),
        read_only: Some(read_only),
        consistency: None,
        bind_options: None,
        volume_options: None,
        tmpfs_options: None,
    }
}

/// Core container config shared by server + installer containers.
/// Returns (container config, host config).
#[allow(clippy::too_many_arguments)]
fn build_container_config(
    hostname: String,
    cfg: &ServerConfig,
    _build: &crate::models::ServerBuild,
    resources: ContainerResources,
    daemon: &Config,
    env: &[String],
    network_ip: &str,
    data_dir: &std::path::Path,
    installer: bool,
) -> (ContainerConfig<String>, HostConfig) {
    let docker = &daemon.docker;

    // Exposed ports: both tcp and udp for every allocation.
    let mut exposed_ports = HashMap::new();
    let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();

    for (ip, port) in cfg.allocations() {
        let bind_ip = rewrite_allocation_ip(&ip, network_ip);
        for proto in ["tcp", "udp"] {
            let key = format!("{port}/{proto}");
            let binding = PortBinding {
                host_ip: Some(bind_ip.clone()),
                host_port: Some(port.to_string()),
            };
            port_bindings
                .entry(key.clone())
                .or_insert_with(|| Some(vec![]))
                .as_mut()
                .unwrap()
                .push(binding);
            exposed_ports.insert(key, HashMap::<(), ()>::new());
        }
    }

    let mut labels = HashMap::new();
    labels.insert(LABEL_SERVICE.to_string(), "Pterodactyl".to_string());
    labels.insert(LABEL_TYPE.to_string(), LABEL_TYPE_SERVER.to_string());
    labels.insert("pterodactyl.server_uuid".to_string(), cfg.uuid.to_string());
    for (k, v) in &cfg.labels {
        labels.insert(k.clone(), v.clone());
    }

    let mut mounts = vec![mount(data_dir, "/home/container", false)];
    for m in &cfg.mounts {
        let source = std::path::Path::new(&m.source);
        let allowed = daemon
            .allowed_mounts
            .iter()
            .any(|a| source.starts_with(std::path::Path::new(a)));
        if !allowed {
            tracing::warn!(
                uuid = %cfg.uuid,
                source = %m.source,
                "skipping custom server mount, not in list of allowed mount points"
            );
            continue;
        }
        mounts.push(mount(std::path::Path::new(&m.source), &m.target, m.read_only));
    }

    let mut tmpfs = HashMap::new();
    tmpfs.insert(
        "/tmp".to_string(),
        format!("rw,exec,nosuid,size={}m", docker.tmpfs_size.max(1)),
    );

    let cap_drop = vec![
        "setpcap".to_string(),
        "mknod".to_string(),
        "audit_write".to_string(),
        "net_raw".to_string(),
        "dac_override".to_string(),
        "fowner".to_string(),
        "fsetid".to_string(),
        "net_bind_service".to_string(),
        "sys_chroot".to_string(),
        "setfcap".to_string(),
    ];

    let log_type = if docker.log_config.r#type.is_empty() {
        "local".to_string()
    } else {
        docker.log_config.r#type.clone()
    };

    let host_config = HostConfig {
        memory: resources.memory,
        memory_swap: resources.memory_swap,
        memory_reservation: resources.memory_reservation,
        oom_kill_disable: resources.oom_kill_disable,
        pids_limit: resources.pids_limit,
        blkio_weight: resources.blkio_weight,
        cpu_quota: resources.cpu_quota,
        cpu_period: resources.cpu_period,
        cpu_shares: resources.cpu_shares,
        cpuset_cpus: resources.cpuset_cpus,
        network_mode: Some(docker.network.network_mode.clone()),
        dns: if docker.network.dns.is_empty() {
            None
        } else {
            Some(docker.network.dns.clone())
        },
        port_bindings: Some(port_bindings),
        mounts: Some(mounts),
        tmpfs: Some(tmpfs),
        log_config: Some(bollard::models::HostConfigLogConfig {
            typ: Some(log_type),
            config: if docker.log_config.config.is_empty() {
                None
            } else {
                Some(docker.log_config.config.clone())
            },
        }),
        security_opt: Some(vec!["no-new-privileges".to_string()]),
        readonly_rootfs: Some(true),
        cap_drop: Some(cap_drop),
        userns_mode: if docker.userns_mode.is_empty() {
            None
        } else {
            Some(docker.userns_mode.clone())
        },
        ..Default::default()
    };

    let config = ContainerConfig {
        hostname: Some(hostname),
        domainname: if docker.domainname.is_empty() {
            None
        } else {
            Some(docker.domainname.clone())
        },
        image: Some(cfg.container.image.clone()),
        env: Some(env.to_vec()),
        labels: Some(labels),
        exposed_ports: Some(exposed_ports),
        tty: Some(true),
        open_stdin: Some(true),
        attach_stdin: Some(true),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        stdin_once: Some(false),
        working_dir: Some("/home/container".to_string()),
        user: Some(container_user(daemon)),
        host_config: Some(host_config.clone()),
        ..Default::default()
    };

    let _ = installer;
    (config, host_config)
}

/// Build Docker credentials for the registry the image belongs to.
/// Uses path-aware matching like wings: supports registries with paths
/// (e.g., `ghcr.io/org/repo`) in addition to hostname-only matching.
fn registry_auth_for(image: &str, registries: &[RegistryConfig]) -> Option<DockerCredentials> {
    // Strip tag if present.
    let image = if let Some((base, tag)) = image.rsplit_once(':') {
        if !tag.contains('/') { base } else { image }
    } else {
        image
    };
    // Normalize: docker.io prefix.
    let image_domain = image.split('/').next().unwrap_or("");
    let mut image_path = "";
    if let Some(slash_pos) = image.find('/') {
        let rest = &image[slash_pos + 1..];
        // If it looks like docker.io/user/image, extract the path.
        if image_domain == "docker.io" || !image_domain.contains('.') {
            // Single-component names are library images (no registry).
            if !image_domain.contains('.') {
                return None;
            }
        } else {
            image_path = rest;
        }
    }

    let mut best: Option<(&RegistryConfig, usize)> = None;
    for r in registries {
        let reg_domain = r.name.split('/').next().unwrap_or("");
        let reg_path = if let Some(slash) = r.name.find('/') {
            &r.name[slash + 1..]
        } else {
            ""
        };
        if reg_domain != image_domain {
            continue;
        }
        // Path match: empty registry path matches everything; otherwise
        // image path must equal or be under the registry path.
        if !reg_path.is_empty() && image_path != reg_path && !image_path.starts_with(&format!("{reg_path}/")) {
            continue;
        }
        let score = reg_domain.len() + reg_path.len();
        if best.as_ref().map_or(true, |(_, s)| score > *s) {
            best = Some((r, score));
        }
    }

    best.map(|(r, _)| DockerCredentials {
        username: Some(r.username.clone()),
        password: Some(r.password.clone()),
        serveraddress: Some(r.name.clone()),
        identitytoken: None,
        registrytoken: None,
        auth: None,
        email: None,
    })
}

fn split_image_tag(image: &str) -> (&str, Option<&str>) {
    if let Some((base, tag)) = image.rsplit_once(':') {
        if !tag.contains('/') {
            return (base, Some(tag));
        }
    }
    (image, None)
}

/// Determine the UID:GID string for containers from daemon config.
/// Wings uses system.user.uid/gid or system.user.rootless.container_uid/container_gid.
fn container_user(daemon: &Config) -> String {
    let u = &daemon.system.user;
    if u.rootless.enabled {
        return format!("{}:{}", u.rootless.container_uid, u.rootless.container_gid);
    }
    if u.uid != 0 && u.gid != 0 {
        return format!("{}:{}", u.uid, u.gid);
    }
    "988:988".to_string()
}
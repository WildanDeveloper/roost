use crate::config::DockerConfig;
use crate::models::ServerBuild;

/// Computed Docker resource limits for a container, mirroring
/// wings `environment/settings.go`:
/// - memory overhead multiplier: 1.15 for <=2GiB, 1.10 for <=4GiB, else 1.05
///   (or whatever the `docker.overhead` config says)
/// - memory_swap = swap + bounded memory (swap -1 => unlimited)
/// - cpu_quota = cpu_limit * period / 100 when cpu_limit > 0
#[derive(Debug, Clone, Default)]
pub struct ContainerResources {
    pub memory: Option<i64>,
    pub memory_reservation: Option<i64>,
    pub memory_swap: Option<i64>,
    pub oom_kill_disable: Option<bool>,
    pub pids_limit: Option<i64>,
    pub blkio_weight: Option<u16>,
    pub cpu_quota: Option<i64>,
    pub cpu_period: Option<i64>,
    pub cpu_shares: Option<i64>,
    pub cpuset_cpus: Option<String>,
}

impl ServerBuild {
    /// Memory overhead multiplier from `docker.overhead`.
    fn memory_overhead_multiplier(&self, docker: &DockerConfig) -> f32 {
        let overhead = &docker.overhead;
        if overhead.override_multiplier {
            return overhead.default_multiplier;
        }
        let mut best = overhead.default_multiplier;
        for m in &overhead.multipliers {
            if self.memory_limit >= m.memory {
                best = m.overhead;
            }
        }
        best
    }

    /// Bounded memory limit in bytes (limit * overhead multiplier).
    fn bounded_memory_limit(&self, docker: &DockerConfig) -> i64 {
        if self.memory_limit <= 0 {
            return 0;
        }
        (self.memory_limit as f32 * self.memory_overhead_multiplier(docker)) as i64 * 1024 * 1024
    }

    /// Build the Docker resource constraints for this server's limits.
    /// `installer` is true for install containers, which use the max of
    /// the server limits and `docker.installer_limits`.
    pub fn as_container_resources(&self, docker: &DockerConfig, installer: bool) -> ContainerResources {
        let mut limit = self.clone();

        if installer {
            limit.memory_limit = limit.memory_limit.max(docker.installer_limits.memory);
            limit.cpu_limit = limit.cpu_limit.max(docker.installer_limits.cpu);
        }

        let memory_limit = limit.bounded_memory_limit(docker);
        let memory_swap = if limit.swap < 0 {
            -1
        } else {
            limit.swap * 1024 * 1024 + memory_limit
        };

        let (cpu_quota, cpu_period) = if limit.cpu_limit > 0 {
            let period = docker.cpu_period.clamp(1000, 1_000_000);
            (Some(limit.cpu_limit * period as i64 / 100), Some(period as i64))
        } else {
            // Java reads the processor count; if no CPU limit is set
            // don't touch the CPU fields at all.
            (None, None)
        };

        ContainerResources {
            memory: if memory_limit > 0 { Some(memory_limit) } else { None },
            memory_reservation: if memory_limit > 0 { Some(limit.memory_limit * 1024 * 1024) } else { None },
            memory_swap: Some(memory_swap),
            oom_kill_disable: Some(limit.oom_disabled),
            pids_limit: Some(docker.container_pid_limit),
            blkio_weight: Some(limit.io_weight.clamp(10, 1000) as u16),
            cpu_quota,
            cpu_period,
            cpu_shares: if docker.cpu_shares > 0 { Some(docker.cpu_shares as i64) } else { None },
            cpuset_cpus: if limit.threads.is_empty() { None } else { Some(limit.threads.clone()) },
        }
    }
}
use crate::config::DockerConfig;
use crate::models::ServerBuild;

/// Computed Docker resource limits for a container, mirroring
/// wings `environment/settings.go`:
/// - memory overhead multiplier: built-in tiers (1.15 for <=2GiB, 1.10 for
///   <=4GiB, else 1.05), or the `docker.overhead.multipliers` map when
///   `docker.overhead.override` is set
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
    /// Memory overhead multiplier from `docker.overhead` — mirrors wings
    /// `Overhead.GetMultiplier`: without `override` the built-in tiers
    /// apply (1.15 for ≤2048 MB, 1.10 for ≤4096 MB, else 1.05); with
    /// `override` the configured `multipliers` map is used (smallest key
    /// whose limit covers the server memory), falling back to
    /// `default_multiplier`.
    fn memory_overhead_multiplier(&self, docker: &DockerConfig) -> f32 {
        let overhead = &docker.overhead;
        if !overhead.override_multiplier {
            if self.memory_limit <= 2048 {
                return 1.15;
            } else if self.memory_limit <= 4096 {
                return 1.10;
            }
            return 1.05;
        }
        let mut keys: Vec<i64> = overhead.multipliers.iter().map(|m| m.memory).collect();
        keys.sort_unstable();
        for key in keys {
            if self.memory_limit > key {
                continue;
            }
            if let Some(m) = overhead.multipliers.iter().find(|m| m.memory == key) {
                return m.overhead;
            }
        }
        overhead.default_multiplier
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
            // Wings: an unlimited installer CPU limit (0) forces the
            // container to unlimited regardless of the server's limit.
            if docker.installer_limits.cpu == 0 {
                limit.cpu_limit = 0;
            } else {
                limit.cpu_limit = limit.cpu_limit.max(docker.installer_limits.cpu);
            }
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
            // Wings removes the PID limit for installer containers.
            pids_limit: if installer { None } else { Some(docker.container_pid_limit) },
            // Wings probes the cgroup hierarchy: on cgroup v2 the io.weight
            // knob must exist on the delegated cgroups or runc fails to
            // create the container; cgroup v1/hybrid always supports it.
            blkio_weight: if blkio_weight_supported() {
                Some(limit.io_weight.clamp(10, 1000) as u16)
            } else {
                None
            },
            cpu_quota,
            cpu_period,
            cpu_shares: if docker.cpu_shares > 0 { Some(docker.cpu_shares as i64) } else { None },
            cpuset_cpus: if limit.threads.is_empty() { None } else { Some(limit.threads.clone()) },
        }
    }
}

/// Mirrors wings `blkioWeightSupported`: cgroup v1/hybrid always honors the
/// weight via blkio.weight; on v2 the io.weight knob must be present on the
/// delegated child cgroups (not the root).
fn blkio_weight_supported() -> bool {
    if !std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        return true;
    }
    for p in [
        "/sys/fs/cgroup/system.slice/io.weight",
        "/sys/fs/cgroup/io.weight",
    ] {
        if std::path::Path::new(p).exists() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Multiplier, OverheadConfig};

    fn docker_with(overhead: OverheadConfig) -> DockerConfig {
        DockerConfig {
            overhead,
            ..Default::default()
        }
    }

    fn build(memory_limit: i64) -> ServerBuild {
        ServerBuild {
            memory_limit,
            ..Default::default()
        }
    }

    #[test]
    fn default_tiers_ignore_multipliers() {
        let mut overhead = OverheadConfig::default();
        overhead.multipliers.push(Multiplier {
            memory: 1,
            overhead: 9.9,
        });
        let docker = docker_with(overhead);
        let cases = [
            (1024, 1.15),
            (2048, 1.15),
            (2049, 1.10),
            (4096, 1.10),
            (4097, 1.05),
            (16384, 1.05),
        ];
        for (memory, want) in cases {
            let got = build(memory).memory_overhead_multiplier(&docker);
            assert!((got - want).abs() < 1e-6, "memory={memory} got={got} want={want}");
        }
    }

    #[test]
    fn override_uses_multiplier_map() {
        let mut overhead = OverheadConfig {
            override_multiplier: true,
            default_multiplier: 1.05,
            ..Default::default()
        };
        overhead.multipliers = vec![
            Multiplier {
                memory: 4096,
                overhead: 1.10,
            },
            Multiplier {
                memory: 2048,
                overhead: 1.15,
            },
        ];
        let docker = docker_with(overhead);
        // Unsorted map: smallest key covering the limit wins.
        assert!((build(1024).memory_overhead_multiplier(&docker) - 1.15).abs() < 1e-6);
        assert!((build(3000).memory_overhead_multiplier(&docker) - 1.10).abs() < 1e-6);
        // Above every key: fall back to the default multiplier.
        assert!((build(8192).memory_overhead_multiplier(&docker) - 1.05).abs() < 1e-6);
    }
}
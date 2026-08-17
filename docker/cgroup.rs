use std::path::PathBuf;
use std::sync::OnceLock;

use crate::error::{AppError, AppResult};

/// Whether the host uses the unified cgroup v2 hierarchy (wings cgroupV2).
pub fn cgroup_v2() -> bool {
    static V2: OnceLock<bool> = OnceLock::new();
    *V2.get_or_init(|| std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists())
}

/// Burst allowance in microseconds for the given CFS quota and configured
/// percentage. The kernel rejects a burst larger than the quota, so the value
/// is clamped to it. (wings cpuBurstMicroseconds)
fn cpu_burst_microseconds(quota: i64, percent: i64) -> i64 {
    if quota <= 0 || percent <= 0 {
        return 0;
    }
    let percent = percent.min(100);
    quota * percent / 100
}

/// Absolute path of the CFS burst file for a process's cgroup, parsed from
/// /proc/<pid>/cgroup contents. (wings resolveCgroupCpuFile)
fn resolve_cgroup_cpu_file(proc_cgroup: &str, v2: bool) -> AppResult<PathBuf> {
    for line in proc_cgroup.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(hierarchy), Some(controllers), Some(path)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if !path.starts_with('/') || path.contains("..") {
            continue;
        }
        if v2 {
            if hierarchy == "0" && controllers.is_empty() {
                return Ok(PathBuf::from("/sys/fs/cgroup")
                    .join(path.trim_start_matches('/'))
                    .join("cpu.max.burst"));
            }
            continue;
        }
        if controllers.split(',').any(|c| c == "cpu") {
            return Ok(PathBuf::from("/sys/fs/cgroup/cpu")
                .join(path.trim_start_matches('/'))
                .join("cpu.cfs_burst_us"));
        }
    }
    Err(AppError::Internal(anyhow::anyhow!(
        "no cpu controller found in cgroup file"
    )))
}

/// Write a burst value in microseconds into the cgroup of the given process.
/// (wings writeBurstFile)
pub fn write_burst_file(pid: i64, burst: i64) -> AppResult<()> {
    let contents = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .map_err(|e| AppError::Internal(anyhow::anyhow!("cannot read cgroup of pid {pid}: {e}")))?;
    let file = resolve_cgroup_cpu_file(&contents, cgroup_v2())?;
    std::fs::write(&file, burst.to_string()).map_err(|e| {
        AppError::Internal(anyhow::anyhow!("cannot write burst to {}: {e}", file.display()))
    })
}

/// Apply the configured CFS burst to a running container based on the CFS
/// quota in microseconds it was created with. No-op when bursting is disabled
/// or the container has no CPU limit. (wings SetCpuBurst)
pub async fn set_cpu_burst(
    client: &crate::docker::DockerClient,
    container_id: &str,
    quota: i64,
    enabled: bool,
    percent: i64,
) {
    if !enabled || quota <= 0 {
        return;
    }
    let Ok(Some(container)) = client.inspect_container(container_id).await else {
        return;
    };
    let Some(state) = &container.state else {
        return;
    };
    let Some(pid) = state.pid else {
        return;
    };
    let burst = cpu_burst_microseconds(quota, percent);
    if let Err(e) = write_burst_file(pid, burst) {
        tracing::warn!(
            container_id = %container_id,
            error = %e,
            "failed to set cpu burst, this requires Linux 5.14 or newer and a writable cgroup hierarchy"
        );
    }
}

/// Zero the CFS burst for the given container process. Must happen before a
/// quota change since the kernel rejects a quota lower than the current
/// burst. Runs even when bursting is disabled. (wings clearCpuBurst)
pub async fn clear_cpu_burst(client: &crate::docker::DockerClient, container_id: &str) {
    let Ok(Some(container)) = client.inspect_container(container_id).await else {
        return;
    };
    let Some(state) = &container.state else {
        return;
    };
    let Some(pid) = state.pid else {
        return;
    };
    if let Err(e) = write_burst_file(pid, 0) {
        tracing::debug!(container_id = %container_id, error = %e, "failed to clear cpu burst");
    }
}
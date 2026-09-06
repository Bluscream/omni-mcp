//! Remote system status information matching the verbose telemetry from `ssh-mcp-server-bluscream`.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::warn;

use super::session::SshSessionHandle;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiskSpace {
    pub free: String,
    pub total: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemorySpace {
    pub free: String,
    pub total: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CpuInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GpuInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DriveInfo {
    pub device: String,
    pub mount_point: String,
    pub total: String,
    pub used: String,
    pub free: String,
    pub usage_percent: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub running: u32,
    pub threads: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceInfo {
    pub running: u32,
    pub installed: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerStatus {
    pub reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub ip_addresses: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_space: Option<DiskSpace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemorySpace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CpuInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub gpus: Vec<GpuInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub drives: Vec<DriveInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processes: Option<ProcessInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub services: Option<ServiceInfo>,
    pub last_updated: String,
}

/// Executes status collection commands on the given session and populates a `ServerStatus`.
pub async fn collect_status(
    session: &Arc<Mutex<SshSessionHandle>>,
    server_name: &str,
) -> ServerStatus {
    let now = chrono_now_iso();
    let mut status = ServerStatus { reachable: true, last_updated: now, ..Default::default() };

    probe_system_info(session, server_name, &mut status).await;
    probe_hardware_info(session, server_name, &mut status).await;

    status
}

async fn run_probe(
    session: &Arc<Mutex<SshSessionHandle>>,
    server_name: &str,
    cmd: &'static str,
) -> String {
    let mut guard = session.lock().await;
    match guard.exec(cmd).await {
        Ok((stdout, _stderr, 0)) => stdout.trim().to_string(),
        Ok((stdout, _, _)) => stdout.trim().to_string(),
        Err(e) => {
            warn!("SSH probe failed on [{server_name}]: {e}");
            String::new()
        }
    }
}

async fn probe_system_info(
    session: &Arc<Mutex<SshSessionHandle>>,
    server_name: &str,
    status: &mut ServerStatus,
) {
    let hostname = run_probe(session, server_name, "hostname").await;
    if !hostname.is_empty() {
        status.hostname = Some(hostname);
    }

    let ips_raw = run_probe(
        session,
        server_name,
        "ip -o addr show | awk '{print $4}' | grep -v '^127\\.' | cut -d'/' -f1",
    )
    .await;
    let ips: Vec<String> = ips_raw
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.contains("127.0.0.1"))
        .map(str::to_string)
        .collect();
    if !ips.is_empty() {
        status.ip_addresses = ips;
    }

    let os_name = run_probe(session, server_name, "uname -s").await;
    if !os_name.is_empty() {
        status.os_name = Some(os_name);
    }

    let os_ver = run_probe(session, server_name, "cat /etc/os-release 2>/dev/null | grep '^PRETTY_NAME=' | cut -d'=' -f2 | tr -d '\"' || uname -o").await;
    if !os_ver.is_empty() {
        status.os_version = Some(os_ver);
    }

    let kernel = run_probe(session, server_name, "uname -r").await;
    if !kernel.is_empty() {
        status.kernel_version = Some(kernel);
    }

    let uptime = run_probe(
        session,
        server_name,
        "uptime -p 2>/dev/null || uptime | awk -F'up ' '{print $2}' | awk -F',' '{print $1}'",
    )
    .await;
    if !uptime.is_empty() {
        status.uptime = Some(uptime);
    }

    let procs_raw = run_probe(session, server_name, "ps aux | wc -l").await;
    let threads_raw = run_probe(session, server_name, "ps -eLf | wc -l").await;
    let proc_count = procs_raw.parse::<u32>().unwrap_or(1).saturating_sub(1);
    let thread_count = threads_raw.parse::<u32>().unwrap_or(1).saturating_sub(1);
    status.processes = Some(ProcessInfo { running: proc_count, threads: thread_count });

    let running_svcs = run_probe(session, server_name, "systemctl list-units --type=service --state=running 2>/dev/null | wc -l || service --status-all 2>/dev/null | grep running | wc -l || echo '0'").await;
    let installed_svcs = run_probe(session, server_name, "systemctl list-unit-files --type=service 2>/dev/null | wc -l || ls /etc/init.d/ 2>/dev/null | wc -l || echo '0'").await;
    let run_count = running_svcs.parse::<u32>().unwrap_or(1).saturating_sub(1);
    let inst_count = installed_svcs.parse::<u32>().unwrap_or(1).saturating_sub(1);
    status.services = Some(ServiceInfo { running: run_count, installed: inst_count });
}

async fn probe_hardware_info(
    session: &Arc<Mutex<SshSessionHandle>>,
    server_name: &str,
    status: &mut ServerStatus,
) {
    let disk_raw = run_probe(
        session,
        server_name,
        "df -h / | tail -1 | awk '{print \"free:\" $4 \" total:\" $2}'",
    )
    .await;
    if let Some((free, total)) = parse_kv_pair(&disk_raw) {
        status.disk_space = Some(DiskSpace { free, total });
    }

    let mem_raw = run_probe(
        session,
        server_name,
        "free -h | grep '^Mem:' | awk '{print \"free:\" $7 \" total:\" $2}'",
    )
    .await;
    if let Some((free, total)) = parse_kv_pair(&mem_raw) {
        status.memory = Some(MemorySpace { free, total });
    }

    let cpu_name = run_probe(session, server_name, "sh -c '(lscpu 2>/dev/null | grep \"^Model name:\" | cut -d\":\" -f2 | xargs || cat /proc/cpuinfo 2>/dev/null | grep \"model name\" | head -1 | cut -d\":\" -f2 | xargs || echo \"$(nproc 2>/dev/null || echo \\x27?\\x27)-core $(uname -m 2>/dev/null || echo \\x27unknown\\x27) processor\") || true'").await;
    let cpu_usage = run_probe(session, server_name, "top -bn1 | grep 'Cpu(s)' | sed 's/.*, *\\([0-9.]*\\)%* id.*/\\1/' | awk '{print 100 - $1}'").await;
    if !cpu_name.is_empty() {
        let usage = if !cpu_usage.is_empty() && cpu_usage != "N/A" {
            cpu_usage.parse::<f64>().ok().map(|val| format!("{val:.1}%"))
        } else {
            None
        };
        status.cpu = Some(CpuInfo { name: cpu_name, usage });
    }

    let gpus_raw = run_probe(session, server_name, "sh -c '(nvidia-smi --query-gpu=name,utilization.gpu --format=csv,noheader,nounits 2>/dev/null | while IFS=\",\" read -r name usage; do echo \"NVIDIA|${name}|${usage}\"; done || lspci | grep -iE \"vga|3d|display\" | while read -r line; do gpu_name=$(echo \"$line\" | cut -d\":\" -f3 | xargs); echo \"OTHER|${gpu_name}|\"; done) || true'").await;
    let gpu_paths_raw =
        run_probe(session, server_name, "ls -1 /dev/dri/card* 2>/dev/null | sort -V || echo ''")
            .await;
    let gpu_paths: Vec<String> = gpu_paths_raw
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    let mut gpus = Vec::new();
    for (i, line) in gpus_raw.lines().enumerate() {
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() >= 2 {
            let name = parts[1].trim();
            if !name.is_empty() && name != "N/A" {
                let usage = parts
                    .get(2)
                    .and_then(|u| u.trim().parse::<f64>().ok())
                    .map(|val| format!("{val:.1}%"));
                let path = gpu_paths.get(i).cloned();
                gpus.push(GpuInfo { name: name.to_string(), usage, path });
            }
        }
    }
    if !gpus.is_empty() {
        status.gpus = gpus;
    }

    let drives_raw = run_probe(session, server_name, "df -h | awk 'NR>1 && $1 !~ /^(tmpfs|devtmpfs|overlay|shfs|rootfs)$/ && $6 !~ /^(\\/dev|\\/run|\\/sys|\\/proc|\\/boot|\\/usr|\\/lib)$/ && $6 != \"\" {print $1\"|\"$2\"|\"$3\"|\"$4\"|\"$5\"|\"$6}'").await;
    let mut drives = Vec::new();
    for line in drives_raw.lines() {
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() >= 6 {
            let device = parts[0].trim().to_string();
            let total = parts[1].trim().to_string();
            let used = parts[2].trim().to_string();
            let free = parts[3].trim().to_string();
            let usage_percent = parts[4].trim().to_string();
            let mount_point = parts[5].trim().to_string();
            if !device.is_empty() && !mount_point.is_empty() {
                drives.push(DriveInfo { device, mount_point, total, used, free, usage_percent });
            }
        }
    }
    if !drives.is_empty() {
        status.drives = drives;
    }
}

fn parse_kv_pair(s: &str) -> Option<(String, String)> {
    // Expected format: "free:12G total:100G"
    let mut free = None;
    let mut total = None;
    for part in s.split_whitespace() {
        if let Some(val) = part.strip_prefix("free:") {
            free = Some(val.to_string());
        } else if let Some(val) = part.strip_prefix("total:") {
            total = Some(val.to_string());
        }
    }
    match (free, total) {
        (Some(f), Some(t)) => Some((f, t)),
        _ => None,
    }
}

fn chrono_now_iso() -> String {
    use std::time::SystemTime;
    let now =
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
    format!("{now}")
}

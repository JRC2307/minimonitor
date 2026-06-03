use std::collections::HashSet;
use std::process::Command;
use std::time::{Duration, Instant};

use serde::Serialize;
use sysinfo::{
    Disks, Networks, Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System, UpdateKind, Users,
};

use crate::ai::{self, AiSnapshot};
use crate::util::capture_label;

const MIN_VISIBLE_CPU_PERCENT: f32 = 0.2;
const MIN_VISIBLE_MEMORY_BYTES: u64 = 20 * 1024 * 1024;
const LOCALHOST_REFRESH: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SortMode {
    Cpu,
    Memory,
}

impl SortMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Memory => "RAM",
        }
    }
}

#[derive(Clone, Serialize)]
pub struct ProcessRow {
    pub pid: u32,
    pub name: String,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub current_user: bool,
    pub user_name: String,
    pub localhost: bool,
    pub command: String,
    pub ai_label: Option<String>,
    pub ai_category: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct CoreUsage {
    pub index: usize,
    pub percent: f32,
}

#[derive(Clone, Serialize)]
pub struct DiskVolume {
    pub name: String,
    pub mount: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Clone, Serialize)]
pub struct MonitorSnapshot {
    pub total_memory_bytes: u64,
    pub used_memory_bytes: u64,
    pub total_swap_bytes: u64,
    pub used_swap_bytes: u64,
    pub total_cpu_percent: f32,
    pub cores: Vec<CoreUsage>,
    pub load_average: (f64, f64, f64),
    pub gpu_percent: Option<f32>,
    pub net_rx_bps: u64,
    pub net_tx_bps: u64,
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
    pub ports: Vec<crate::net::PortRow>,
    pub connections: Vec<crate::net::ConnGroup>,
    pub identity: crate::net::NetIdentity,
    pub ai_snapshot: AiSnapshot,
    pub processes: Vec<ProcessRow>,
    pub sort_mode: SortMode,
    pub captured_at: String,
    pub disks: Vec<DiskVolume>,
    pub uptime_secs: u64,
    pub boot_epoch: u64,
}

pub struct Sampler {
    system: System,
    users: Users,
    networks: Networks,
    last_sample: Option<Instant>,
    localhost_pids: HashSet<u32>,
    last_localhost: Instant,
    ports: Vec<crate::net::PortRow>,
    connections: Vec<crate::net::ConnGroup>,
    identity: crate::net::NetIdentity,
    disks: Disks,
}

impl Sampler {
    pub fn new() -> Self {
        let mut system = System::new();
        // Warm up CPU sampling — sysinfo needs two refreshes ≥200ms apart
        // before CPU deltas are meaningful. Without this the first snapshot
        // reads ~0% even when the machine is busy.
        system.refresh_cpu_usage();
        std::thread::sleep(Duration::from_millis(220));

        Self {
            system,
            users: Users::new_with_refreshed_list(),
            networks: Networks::new_with_refreshed_list(),
            last_sample: None,
            localhost_pids: collect_localhost_pids(),
            last_localhost: Instant::now(),
            ports: crate::net::listening_ports(),
            connections: crate::net::established_connections(),
            identity: crate::net::network_identity(
                System::host_name().unwrap_or_default(),
            ),
            disks: Disks::new_with_refreshed_list(),
        }
    }

    pub fn sample(&mut self, sort_mode: SortMode) -> MonitorSnapshot {
        let now = Instant::now();
        let elapsed = self
            .last_sample
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or(1.0)
            .max(0.05);

        if now.duration_since(self.last_localhost) >= LOCALHOST_REFRESH {
            self.localhost_pids = collect_localhost_pids();
            self.users.refresh();
            self.ports = crate::net::listening_ports();
            self.connections = crate::net::established_connections();
            self.identity = crate::net::network_identity(
                System::host_name().unwrap_or_default(),
            );
            self.disks.refresh(true);
            self.last_localhost = now;
        }

        self.system.refresh_memory();
        self.system.refresh_cpu_usage();
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_disk_usage()
                .with_cmd(UpdateKind::OnlyIfNotSet)
                .with_exe(UpdateKind::OnlyIfNotSet),
        );

        self.networks.refresh(true);
        let (net_rx, net_tx) = self.networks.iter().fold((0u64, 0u64), |(rx, tx), (_, n)| {
            (rx + n.received(), tx + n.transmitted())
        });
        let net_rx_bps = (net_rx as f64 / elapsed) as u64;
        let net_tx_bps = (net_tx as f64 / elapsed) as u64;

        let (disk_read, disk_write) =
            self.system
                .processes()
                .values()
                .fold((0u64, 0u64), |(r, w), p| {
                    let u = p.disk_usage();
                    (r + u.read_bytes, w + u.written_bytes)
                });
        let disk_read_bps = (disk_read as f64 / elapsed) as u64;
        let disk_write_bps = (disk_write as f64 / elapsed) as u64;

        self.last_sample = Some(now);

        let cores: Vec<CoreUsage> = self
            .system
            .cpus()
            .iter()
            .enumerate()
            .map(|(index, cpu)| CoreUsage {
                index,
                percent: cpu.cpu_usage(),
            })
            .collect();

        let load = System::load_average();
        let ai_snapshot = ai::build_snapshot(&self.system);
        let current_user = std::env::var("USER").unwrap_or_default();

        let mut processes: Vec<ProcessRow> = self
            .system
            .processes()
            .iter()
            .map(|(pid, process)| {
                let command = ai::os_strings_to_string(process.cmd());
                let user_name = process
                    .user_id()
                    .and_then(|uid| self.users.get_user_by_id(uid))
                    .map(|u| u.name().to_owned())
                    .unwrap_or_else(|| "-".to_owned());
                let ai_match = ai::detect(&process.name().to_string_lossy(), &command);

                ProcessRow {
                    pid: pid.as_u32(),
                    name: process.name().to_string_lossy().into_owned(),
                    cpu_percent: process.cpu_usage(),
                    memory_bytes: process.memory(),
                    current_user: !current_user.is_empty() && user_name == current_user,
                    user_name,
                    localhost: self.localhost_pids.contains(&pid.as_u32()),
                    command,
                    ai_label: ai_match.map(|(label, _)| label.to_owned()),
                    ai_category: ai_match.map(|(_, cat)| cat.to_owned()),
                }
            })
            .collect();

        match sort_mode {
            SortMode::Cpu => processes.sort_by(|a, b| {
                b.cpu_percent
                    .partial_cmp(&a.cpu_percent)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.memory_bytes.cmp(&a.memory_bytes))
            }),
            SortMode::Memory => processes.sort_by(|a, b| {
                b.memory_bytes.cmp(&a.memory_bytes).then_with(|| {
                    b.cpu_percent
                        .partial_cmp(&a.cpu_percent)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
            }),
        }

        let disks: Vec<DiskVolume> = self
            .disks
            .iter()
            .map(|d| DiskVolume {
                name: d.name().to_string_lossy().into_owned(),
                mount: d.mount_point().to_string_lossy().into_owned(),
                total_bytes: d.total_space(),
                available_bytes: d.available_space(),
            })
            .collect();

        MonitorSnapshot {
            total_memory_bytes: self.system.total_memory(),
            used_memory_bytes: self.system.used_memory(),
            total_swap_bytes: self.system.total_swap(),
            used_swap_bytes: self.system.used_swap(),
            total_cpu_percent: self.system.global_cpu_usage(),
            cores,
            load_average: (load.one, load.five, load.fifteen),
            gpu_percent: gpu_percent(),
            net_rx_bps,
            net_tx_bps,
            disk_read_bps,
            disk_write_bps,
            ports: self.ports.clone(),
            connections: self.connections.clone(),
            identity: self.identity.clone(),
            ai_snapshot,
            processes,
            sort_mode,
            captured_at: capture_label(),
            disks,
            uptime_secs: System::uptime(),
            boot_epoch: System::boot_time(),
        }
    }

    pub fn kill(&self, pid: u32) -> Option<bool> {
        self.system.process(Pid::from_u32(pid)).map(|process| {
            process
                .kill_with(Signal::Term)
                .unwrap_or_else(|| process.kill())
        })
    }
}

pub fn is_visible(row: &ProcessRow) -> bool {
    row.cpu_percent >= MIN_VISIBLE_CPU_PERCENT || row.memory_bytes >= MIN_VISIBLE_MEMORY_BYTES
}

fn collect_localhost_pids() -> HashSet<u32> {
    let Ok(output) = Command::new("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN"])
        .output()
    else {
        return HashSet::new();
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .skip(1)
        .filter_map(|line| {
            let parts: Vec<_> = line.split_whitespace().collect();
            if parts.len() < 9 {
                return None;
            }
            let address = parts.last().copied().unwrap_or_default();
            if !(address.contains("127.0.0.1") || address.contains("[::1]")) {
                return None;
            }
            parts.get(1)?.parse::<u32>().ok()
        })
        .collect()
}

fn gpu_percent() -> Option<f32> {
    let output = Command::new("ioreg")
        .args(["-rc", "IOAccelerator", "-d", "1"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let line = line.trim();
        if line.contains("\"Device Utilization %\"") {
            if let Some(eq) = line.find('=') {
                if let Ok(v) = line[eq + 1..].trim().parse::<f32>() {
                    return Some(v);
                }
            }
        }
    }
    None
}

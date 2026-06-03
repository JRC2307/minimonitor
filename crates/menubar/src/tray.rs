use tray_icon::{
    TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
};

use minimonitor_core::snapshot::{MonitorSnapshot, SortMode, is_visible};
use crate::util::{
    format_bytes, format_bytes_pair, format_rate, make_tray_icon, slugify, truncate_name,
};

const MAX_MENU_PROCESSES: usize = 8;

pub fn build_tray(title: &str) -> TrayIcon {
    TrayIconBuilder::new()
        .with_title(title)
        .with_tooltip("MiniMonitor")
        .with_icon(make_tray_icon())
        .with_menu_on_left_click(true)
        .build()
        .expect("failed to create tray icon")
}

pub fn build_menu(
    snapshot: &MonitorSnapshot,
    sort_mode: SortMode,
    status: Option<&str>,
) -> Menu {
    let menu = Menu::new();

    let cpu_ram = MenuItem::new(
        format!(
            "CPU {:.0}% • RAM {}",
            snapshot.total_cpu_percent,
            format_bytes_pair(snapshot.used_memory_bytes, snapshot.total_memory_bytes)
        ),
        false,
        None,
    );
    let swap = MenuItem::new(
        format!(
            "Swap {}",
            format_bytes_pair(snapshot.used_swap_bytes, snapshot.total_swap_bytes)
        ),
        false,
        None,
    );
    let gpu = MenuItem::new(
        match snapshot.gpu_percent {
            Some(v) => format!("GPU {v:.0}%"),
            None => "GPU n/a".to_owned(),
        },
        false,
        None,
    );
    let load = MenuItem::new(
        format!(
            "Load {:.2} {:.2} {:.2}",
            snapshot.load_average.0, snapshot.load_average.1, snapshot.load_average.2
        ),
        false,
        None,
    );
    let net = MenuItem::new(
        format!(
            "Net ↓{}  ↑{}",
            format_rate(snapshot.net_rx_bps),
            format_rate(snapshot.net_tx_bps)
        ),
        false,
        None,
    );
    let disk = MenuItem::new(
        format!(
            "Disk ↓{}  ↑{}",
            format_rate(snapshot.disk_read_bps),
            format_rate(snapshot.disk_write_bps)
        ),
        false,
        None,
    );
    let ai_summary = MenuItem::new(
        if snapshot.ai_snapshot.workload_count == 0 {
            "AI workloads none detected".to_owned()
        } else {
            format!(
                "AI {} workloads • {:.0}% CPU • {}",
                snapshot.ai_snapshot.workload_count,
                snapshot.ai_snapshot.total_cpu_percent,
                format_bytes(snapshot.ai_snapshot.total_memory_bytes)
            )
        },
        false,
        None,
    );
    let captured = MenuItem::new(format!("Snapshot {}", snapshot.captured_at), false, None);

    let show_inspector = MenuItem::with_id("show-inspector", "Show Inspector", true, None);
    let refresh = MenuItem::with_id("refresh-menu", "Refresh snapshot", true, None);
    let sort_cpu = MenuItem::with_id(
        "sort:cpu",
        if sort_mode == SortMode::Cpu {
            "Use CPU sort  •"
        } else {
            "Use CPU sort"
        },
        true,
        None,
    );
    let sort_ram = MenuItem::with_id(
        "sort:ram",
        if sort_mode == SortMode::Memory {
            "Use RAM sort  •"
        } else {
            "Use RAM sort"
        },
        true,
        None,
    );

    let processes = build_processes_submenu(snapshot);
    let ai_sub = build_ai_submenu(snapshot);
    let quit = MenuItem::with_id("quit", "Quit MiniMonitor", true, None);
    let sep1 = PredefinedMenuItem::separator();
    let sep2 = PredefinedMenuItem::separator();

    let _ = menu.append_items(&[
        &cpu_ram,
        &swap,
        &gpu,
        &load,
        &net,
        &disk,
        &ai_summary,
        &captured,
        &sep1,
        &show_inspector,
        &refresh,
        &sort_cpu,
        &sort_ram,
        &ai_sub,
        &processes,
    ]);

    if let Some(msg) = status {
        let s = PredefinedMenuItem::separator();
        let item = MenuItem::new(msg, false, None);
        let _ = menu.append(&s);
        let _ = menu.append(&item);
    }

    let _ = menu.append(&sep2);
    let _ = menu.append(&quit);
    menu
}

fn build_ai_submenu(snapshot: &MonitorSnapshot) -> Submenu {
    let submenu = Submenu::new("AI workloads", true);
    if snapshot.ai_snapshot.top_workloads.is_empty() {
        let _ = submenu.append(&MenuItem::new(
            "No AI runtimes or agent tools inferred",
            false,
            None,
        ));
        return submenu;
    }

    for w in &snapshot.ai_snapshot.top_workloads {
        let child = Submenu::with_id(
            format!("ai:{}", slugify(&w.label)),
            format!(
                "{} • {:.0}% CPU • {}",
                w.label,
                w.total_cpu_percent,
                format_bytes(w.total_memory_bytes)
            ),
            true,
        );
        let cat = MenuItem::new(format!("Kind {}", w.category), false, None);
        let count = MenuItem::new(format!("Processes {}", w.process_count), false, None);
        let cmd = MenuItem::new(
            format!("Cmd {}", truncate_name(&w.example_command, 42)),
            false,
            None,
        );
        let _ = child.append_items(&[&cat, &count, &cmd]);
        let _ = submenu.append(&child);
    }
    submenu
}

fn build_processes_submenu(snapshot: &MonitorSnapshot) -> Submenu {
    let submenu = Submenu::new("Top processes", true);
    for p in snapshot
        .processes
        .iter()
        .filter(|p| is_visible(p))
        .take(MAX_MENU_PROCESSES)
    {
        let child = Submenu::with_id(
            format!("process:{}", p.pid),
            format!(
                "{} • {:>5.1}% CPU • {}",
                truncate_name(&p.name, 24),
                p.cpu_percent,
                format_bytes(p.memory_bytes)
            ),
            true,
        );
        let pid_item = MenuItem::new(format!("PID {}", p.pid), false, None);
        let cpu = MenuItem::new(format!("CPU {:>5.1}%", p.cpu_percent), false, None);
        let ram = MenuItem::new(format!("RAM {}", format_bytes(p.memory_bytes)), false, None);
        let local = MenuItem::new(
            if p.localhost {
                "Localhost yes"
            } else {
                "Localhost no"
            },
            false,
            None,
        );
        let sep = PredefinedMenuItem::separator();
        let kill = MenuItem::with_id(format!("kill:{}", p.pid), "Kill process", true, None);
        let _ = child.append_items(&[&pid_item, &cpu, &ram, &local, &sep, &kill]);
        let _ = submenu.append(&child);
    }
    submenu
}

pub fn parse_pid_suffix(id: &str, prefix: &str) -> Option<u32> {
    id.strip_prefix(prefix)?.parse().ok()
}

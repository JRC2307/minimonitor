use tray_icon::{
    TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
};

use crate::actions::DisplayPreset;
use crate::util::{
    format_bytes, format_bytes_pair, format_rate, make_tray_icon, slugify, truncate_name,
};
use minimonitor_core::snapshot::{MonitorSnapshot, SortMode, is_visible};

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
    active_display: Option<DisplayPreset>,
    status: Option<&str>,
) -> Menu {
    let menu = Menu::new();

    let line1 = MenuItem::new(
        format!(
            "CPU {:.0}%   RAM {}   {}",
            snapshot.total_cpu_percent,
            format_bytes_pair(snapshot.used_memory_bytes, snapshot.total_memory_bytes),
            match snapshot.gpu_percent {
                Some(v) => format!("GPU {v:.0}%"),
                None => "GPU n/a".into(),
            },
        ),
        false,
        None,
    );
    let line2 = MenuItem::new(
        format!(
            "Net ↓{} ↑{}   Disk ↓{} ↑{}",
            format_rate(snapshot.net_rx_bps),
            format_rate(snapshot.net_tx_bps),
            format_rate(snapshot.disk_read_bps),
            format_rate(snapshot.disk_write_bps),
        ),
        false,
        None,
    );
    let line3 = MenuItem::new(
        format!(
            "Load {:.2} {:.2} {:.2}   Up {}",
            snapshot.load_average.0,
            snapshot.load_average.1,
            snapshot.load_average.2,
            format_uptime(snapshot.uptime_secs),
        ),
        false,
        None,
    );

    let ports = build_ports_submenu(snapshot);
    let processes = build_processes_submenu(snapshot);
    let ai_sub = build_ai_submenu(snapshot);
    let display = build_display_submenu(active_display);
    let quick = build_quick_actions_submenu();

    let show_inspector = MenuItem::with_id("show-inspector", "Show Inspector", true, None);
    let refresh = MenuItem::with_id("refresh-menu", "Refresh snapshot", true, None);
    let sort_cpu = MenuItem::with_id(
        "sort:cpu",
        if sort_mode == SortMode::Cpu {
            "Sort: CPU •"
        } else {
            "Sort: CPU"
        },
        true,
        None,
    );
    let sort_ram = MenuItem::with_id(
        "sort:ram",
        if sort_mode == SortMode::Memory {
            "Sort: RAM •"
        } else {
            "Sort: RAM"
        },
        true,
        None,
    );
    let quit = MenuItem::with_id("quit", "Quit MiniMonitor", true, None);
    let sep1 = PredefinedMenuItem::separator();
    let sep2 = PredefinedMenuItem::separator();
    let sep3 = PredefinedMenuItem::separator();

    let _ = menu.append_items(&[
        &line1,
        &line2,
        &line3,
        &sep1,
        &ports,
        &processes,
        &ai_sub,
        &display,
        &quick,
        &sep2,
        &show_inspector,
        &refresh,
        &sort_cpu,
        &sort_ram,
    ]);

    if let Some(msg) = status {
        let s = PredefinedMenuItem::separator();
        let item = MenuItem::new(msg, false, None);
        let _ = menu.append(&s);
        let _ = menu.append(&item);
    }
    let _ = menu.append(&sep3);
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

fn format_uptime(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3_600;
    let m = (secs % 3_600) / 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

fn build_ports_submenu(snapshot: &MonitorSnapshot) -> Submenu {
    let submenu = Submenu::new(format!("Listening ports ({})", snapshot.ports.len()), true);
    if snapshot.ports.is_empty() {
        let _ = submenu.append(&MenuItem::new("No listening TCP ports", false, None));
        return submenu;
    }
    let mut ports = snapshot.ports.clone();
    ports.sort_by_key(|p| p.port);
    for p in ports.iter().take(MAX_MENU_PROCESSES * 2) {
        let child = Submenu::with_id(
            format!("port:{}", p.pid),
            format!(
                "{} • {} • {}",
                p.port,
                truncate_name(&p.process, 18),
                p.bind
            ),
            true,
        );
        let pid_item = MenuItem::new(format!("PID {}", p.pid), false, None);
        let bind = MenuItem::new(format!("Bind {} {}", p.proto, p.bind), false, None);
        let sep = PredefinedMenuItem::separator();
        let kill = MenuItem::with_id(format!("kill:{}", p.pid), "Kill owner", true, None);
        let _ = child.append_items(&[&pid_item, &bind, &sep, &kill]);
        let _ = submenu.append(&child);
    }
    submenu
}

fn build_display_submenu(active: Option<DisplayPreset>) -> Submenu {
    let submenu = Submenu::new("Display (RustDesk)", true);
    for preset in DisplayPreset::ALL {
        let label = if active == Some(preset) {
            format!("{} •", preset.label())
        } else {
            preset.label().to_owned()
        };
        let item = MenuItem::with_id(preset.menu_id(), label, true, None);
        let _ = submenu.append(&item);
    }
    submenu
}

fn build_quick_actions_submenu() -> Submenu {
    let submenu = Submenu::new("Quick actions", true);
    let caffeinate = MenuItem::with_id("action:caffeinate", "Toggle keep-awake", true, None);
    let flush = MenuItem::with_id("action:flush-dns", "Flush DNS (admin)", true, None);
    let _ = submenu.append_items(&[&caffeinate, &flush]);
    submenu
}

pub fn parse_pid_suffix(id: &str, prefix: &str) -> Option<u32> {
    id.strip_prefix(prefix)?.parse().ok()
}

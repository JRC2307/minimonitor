use serde::{Deserialize, Serialize};
use tao::{
    dpi::LogicalSize,
    event_loop::{EventLoopProxy, EventLoopWindowTarget},
    window::{Window, WindowBuilder},
};
use wry::{WebView, WebViewBuilder, http::Request};

use minimonitor_core::ai::AiWorkload;
use crate::app::UserEvent;
use minimonitor_core::net::{ConnGroup, PortRow};
use minimonitor_core::snapshot::{CoreUsage, DiskVolume, MonitorSnapshot, ProcessRow, is_visible};
use crate::util::{format_bytes_pair, format_rate, make_window_icon, percentage};

const MAX_INSPECTOR_PROCESSES: usize = 200;

pub struct InspectorWindow {
    pub window: Window,
    pub webview: WebView,
}

#[derive(Clone)]
pub struct FilterState {
    pub current_user_only: bool,
    pub localhost_only: bool,
}

impl Default for FilterState {
    fn default() -> Self {
        Self {
            current_user_only: true,
            localhost_only: false,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum InspectorCommand {
    Refresh,
    Close,
    Kill { pid: u32 },
    SetSort { value: String },
    ActionCaffeinate,
    ActionFlushDns,
}

#[derive(Serialize)]
pub struct InspectorView {
    pub summary: SummaryView,
    pub filters: FilterView,
    pub processes: Vec<ProcessRow>,
    pub ai_workloads: Vec<AiWorkload>,
    pub cores: Vec<CoreUsage>,
    pub ports: Vec<PortRow>,
    pub connections: Vec<ConnGroup>,
    pub disks: Vec<DiskVolume>,
    pub status_message: Option<String>,
    pub captured_at: String,
}

#[derive(Serialize)]
pub struct SummaryView {
    pub ram_percent: f32,
    pub cpu_percent: f32,
    pub ram_label: String,
    pub swap_label: String,
    pub ai_label: String,
    pub gpu_percent: Option<f32>,
    pub load_average: (f64, f64, f64),
    pub net_rx: String,
    pub net_tx: String,
    pub disk_read: String,
    pub disk_write: String,
    pub uptime_secs: u64,
    pub hostname: String,
    pub lan_ip: Option<String>,
    pub tailnet_ip: Option<String>,
}

#[derive(Serialize)]
pub struct FilterView {
    pub current_user_only: bool,
    pub localhost_only: bool,
    pub sort_mode: &'static str,
}

pub fn open(
    target: &EventLoopWindowTarget<UserEvent>,
    proxy: EventLoopProxy<UserEvent>,
) -> InspectorWindow {
    let window = WindowBuilder::new()
        .with_title("MiniMonitor Inspector")
        .with_inner_size(LogicalSize::new(1180.0, 820.0))
        .with_window_icon(Some(make_window_icon()))
        .build(target)
        .expect("failed to build inspector window");
    window.set_visible(true);

    let webview = WebViewBuilder::new()
        .with_html(HTML)
        .with_clipboard(true)
        .with_ipc_handler(move |request: Request<String>| {
            if let Ok(command) = serde_json::from_str::<InspectorCommand>(request.body()) {
                let _ = proxy.send_event(UserEvent::Inspector(command));
            }
        })
        .build(&window)
        .expect("failed to build inspector webview");

    InspectorWindow { window, webview }
}

pub fn build_view(
    snapshot: &MonitorSnapshot,
    filters: &FilterState,
    status_message: Option<String>,
) -> InspectorView {
    let processes = snapshot
        .processes
        .iter()
        .filter(|p| is_visible(p))
        .filter(|p| !filters.current_user_only || p.current_user)
        .filter(|p| !filters.localhost_only || p.localhost)
        .take(MAX_INSPECTOR_PROCESSES)
        .cloned()
        .collect();

    InspectorView {
        summary: SummaryView {
            ram_percent: percentage(snapshot.used_memory_bytes, snapshot.total_memory_bytes),
            cpu_percent: snapshot.total_cpu_percent,
            ram_label: format_bytes_pair(snapshot.used_memory_bytes, snapshot.total_memory_bytes),
            swap_label: format_bytes_pair(snapshot.used_swap_bytes, snapshot.total_swap_bytes),
            ai_label: if snapshot.ai_snapshot.workload_count == 0 {
                "No inferred AI workloads".to_owned()
            } else {
                format!(
                    "{} workloads • {:.0}% CPU",
                    snapshot.ai_snapshot.workload_count, snapshot.ai_snapshot.total_cpu_percent
                )
            },
            gpu_percent: snapshot.gpu_percent,
            load_average: snapshot.load_average,
            net_rx: format_rate(snapshot.net_rx_bps),
            net_tx: format_rate(snapshot.net_tx_bps),
            disk_read: format_rate(snapshot.disk_read_bps),
            disk_write: format_rate(snapshot.disk_write_bps),
            uptime_secs: snapshot.uptime_secs,
            hostname: snapshot.identity.hostname.clone(),
            lan_ip: snapshot.identity.lan_ip.clone(),
            tailnet_ip: snapshot.identity.tailnet_ip.clone(),
        },
        filters: FilterView {
            current_user_only: filters.current_user_only,
            localhost_only: filters.localhost_only,
            sort_mode: snapshot.sort_mode.label(),
        },
        processes,
        ai_workloads: snapshot.ai_snapshot.top_workloads.clone(),
        cores: snapshot.cores.clone(),
        ports: snapshot.ports.clone(),
        connections: snapshot.connections.clone(),
        disks: snapshot.disks.clone(),
        status_message,
        captured_at: snapshot.captured_at.clone(),
    }
}

pub fn push_state(inspector: &InspectorWindow, view: &InspectorView) {
    let payload = serde_json::to_string(view).unwrap_or_else(|_| "{}".to_owned());
    let escaped = serde_json::to_string(&payload).unwrap();
    let script = format!("window.updateFromRust(JSON.parse({escaped}));");
    let _ = inspector.webview.evaluate_script(&script);
}

const HTML: &str = include_str!("inspector.html");

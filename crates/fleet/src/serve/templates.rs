//! Askama view-models for the `fleet serve` HTML pages (spec §3.8).
//!
//! askama 0.13 dropped the built-in axum integration: each template implements
//! [`askama::Template`] (a compile-time-checked `render() -> Result<String>`).
//! [`render`] wraps that into an axum [`Html`] response, mapping a render error
//! to `500`. Template files live under `src/serve/templates/` (configured via
//! `askama.toml`). A bad field reference fails `cargo build`, not a request.

use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

/// Render any askama template into an axum HTML response.
pub fn render<T: Template>(tpl: &T) -> Response {
    match tpl.render() {
        Ok(body) => Html(body).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("template render error: {e}"),
        )
            .into_response(),
    }
}

// ── Inventory (`/`, mirrors `fleet list`) ────────────────────────────────────

/// One inventory row. `online` is **derived** (recomputed from `last_seen`
/// freshness at request time, never the stale stored flag). `fuzzy` drives the
/// `~` marker for fuzzy-merged nodes.
pub struct InventoryRow {
    pub fleet_id: String,
    pub hostname: String,
    pub tier: String,
    pub online: bool,
    pub site: String,
    pub role: String,
    pub owner: String,
    pub last_seen: String,
    pub fuzzy: bool,
}

#[derive(Template)]
#[template(path = "inventory.html")]
pub struct InventoryPage {
    pub rows: Vec<InventoryRow>,
}

/// The HTMX partial-refresh fragment: just the `<table>` (same loop as the full
/// page, served when `?partial=1`).
#[derive(Template)]
#[template(path = "inventory_table.html")]
pub struct InventoryTable {
    pub rows: Vec<InventoryRow>,
}

// ── Host snapshot section (on /node/{id}) ────────────────────────────────────

pub struct HostPortRow {
    pub port: u16,
    pub proto: String,
    pub process: String,
    pub pid: i64,
    pub bind: String,
}

pub struct HostWorkloadRow {
    pub label: String,
    pub category: String,
    pub process_count: i64,
    pub cpu_pct: String,   // pre-formatted "42.1%"
    pub mem_human: String, // pre-formatted "1.2 GB"
    pub example_command: String,
}

pub struct HostSnapshotView {
    pub collected_at: String,
    pub stale: bool,
    pub cpu_pct: String,         // pre-formatted "42.1%"
    pub mem_used: String,        // pre-formatted "1.2 GB"
    pub mem_total: String,       // pre-formatted "8.0 GB"
    pub gpu_pct: Option<String>, // pre-formatted "35.0%" or None
    pub ports: Vec<HostPortRow>,
    pub workloads: Vec<HostWorkloadRow>,
    pub workload_count: i64, // true total (may exceed workloads.len())
    pub showing_top_n_note: Option<String>,
}

// ── /ports page ───────────────────────────────────────────────────────────────

pub struct FleetPortViewRow {
    pub fleet_id: String,
    pub hostname: String,
    pub service: String, // resolved friendly name (spec: port service naming)
    pub port: u16,
    pub proto: String,
    pub process: String,
    pub pid: i64,
    pub bind: String,
    pub collected_at: String,
    pub stale: bool,
}

#[derive(Template)]
#[template(path = "ports.html")]
pub struct PortsPage {
    pub rows: Vec<FleetPortViewRow>,
}

// ── /workloads page ───────────────────────────────────────────────────────────

pub struct FleetWorkloadViewRow {
    pub fleet_id: String,
    pub hostname: String,
    pub label: String,
    pub category: String,
    pub process_count: i64,
    pub cpu_pct: String,         // pre-formatted
    pub mem_human: String,       // pre-formatted
    pub example_command: String, // already truncated to ~80 chars
    pub collected_at: String,
    pub stale: bool,
    pub showing_top_n_note: Option<String>,
}

#[derive(Template)]
#[template(path = "workloads.html")]
pub struct WorkloadsPage {
    pub rows: Vec<FleetWorkloadViewRow>,
}

// ── Node detail (`/node/{id}`, mirrors `fleet show`) ─────────────────────────

pub struct SeenInRow {
    pub account: String,
    pub device_id: String,
}

#[derive(Template)]
#[template(path = "node.html")]
pub struct NodePage {
    pub fleet_id: String,
    pub hostname: String,
    pub fqdn: String,
    pub os: String,
    pub tier: String,
    pub online: bool,
    pub dedupe_key_kind: String,
    pub role: String,
    pub owner: String,
    pub site: String,
    pub gpu: String,
    pub last_seen: String,
    pub addresses: Vec<String>,
    pub seen_in: Vec<SeenInRow>,
    pub raw_tags: Vec<String>,
    pub notes: Option<String>,
    pub host_snapshot: Option<HostSnapshotView>,
}

// ── Paths (`/paths`, MTR path health) ────────────────────────────────────────

pub struct PathRow {
    pub target_name: String,
    pub target_addr: String,
    pub path_type: String,
    pub dest_host: Option<String>,
    pub dest_loss_pct: f64,
    pub dest_avg_ms: f64,
    pub dest_severity: String,
}

#[derive(Template)]
#[template(path = "paths.html")]
pub struct PathsPage {
    pub paths: Vec<PathRow>,
}

// ── Observability (`/observability`) ─────────────────────────────────────────

pub struct ZoneRow {
    pub name: String,
    pub status: String,
    pub healthy: bool,
    pub cert_expiry: String,
}

#[derive(Template)]
#[template(path = "observability.html")]
pub struct ObservabilityPage {
    pub online_count: usize,
    pub offline_count: usize,
    pub total_count: usize,
    pub beszel_ui_url: String,
    pub kuma_ui_url: String,
    pub zones: Vec<ZoneRow>,
}

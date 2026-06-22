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

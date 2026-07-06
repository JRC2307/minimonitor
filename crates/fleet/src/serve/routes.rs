//! Axum route handlers for `fleet serve` (spec §3.8).
//!
//! Each handler opens a read-only SQLite connection, loads data, and
//! serializes via the `export::build_*` builders — same shapes as
//! `fleet list --json`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::model::{DedupeKind, Tier, is_online};
use crate::serve::templates;
use crate::{db, export};

/// Shared state for the handlers.
#[derive(Clone)]
pub struct AppState {
    /// Path to the SQLite registry file (opened read-only per request).
    pub db_path: PathBuf,
    /// Freshness window for the **derived** `online` field (spec §3.3). The HTML
    /// views recompute online at request time rather than trusting the stored flag.
    pub online_threshold: Duration,
    /// Age threshold after which a host snapshot is considered stale (spec §6.5).
    /// Derived from `Config::snapshot_stale_secs` in `run_with`; defaults to 3 h.
    pub snapshot_stale_threshold: Duration,
    /// Deep-drill-down link target for `/observability` (NOT polled — R-10).
    pub beszel_ui_url: String,
    /// Deep-drill-down link target for `/observability` (NOT polled — R-10).
    pub kuma_ui_url: String,
    /// Curated port→service-name overrides, loaded once at startup (spec: port
    /// service naming). Wrapped in `Arc` so `AppState` stays cheap to `Clone`.
    pub labels: std::sync::Arc<crate::service_label::Labels>,
    /// The caguastore app catalog (built-in default or `store.toml` override),
    /// loaded once at startup.
    pub store: std::sync::Arc<crate::store::Catalog>,
}

// ── format helpers ────────────────────────────────────────────────────────────

fn fmt_bytes(bytes: i64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let kb = bytes as f64 / 1024.0;
    if kb < 1024.0 {
        return format!("{kb:.1} KB");
    }
    let mb = kb / 1024.0;
    if mb < 1024.0 {
        return format!("{mb:.1} MB");
    }
    let gb = mb / 1024.0;
    format!("{gb:.1} GB")
}

fn fmt_pct(pct: f64) -> String {
    format!("{pct:.1}%")
}

fn truncate80(s: &str) -> String {
    if s.len() <= 80 {
        s.to_owned()
    } else {
        // Char-safe: slice on a UTF-8 boundary so a multi-byte char near byte 79
        // can't panic (a malicious/odd command line could carry non-ASCII).
        let end = s.char_indices().nth(79).map(|(i, _)| i).unwrap_or(s.len());
        format!("{}…", &s[..end])
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Open a read-only connection and return a 500 on failure.
fn ro_conn(state: &AppState) -> Result<rusqlite::Connection, (StatusCode, String)> {
    super::open_readonly(&state.db_path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("db open failed: {e:#}"),
        )
    })
}

// ── GET /api/fleet ────────────────────────────────────────────────────────────

pub async fn get_fleet(State(state): State<AppState>) -> impl IntoResponse {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    match db::nodes::list(&conn) {
        Ok(nodes) => Json(export::build_fleet_json(&nodes)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("db query failed: {e:#}"),
        )
            .into_response(),
    }
}

// ── GET /api/node/{id} ────────────────────────────────────────────────────────

pub async fn get_node(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    match db::nodes::get(&conn, &id) {
        Ok(Some(node)) => {
            // Reuse the per-node projection from build_fleet_json
            let fleet = export::build_fleet_json(&[node]);
            let node_export = fleet.nodes.into_iter().next().unwrap();
            Json(node_export).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("db query failed: {e:#}"),
        )
            .into_response(),
    }
}

// ── GET /api/path-health ──────────────────────────────────────────────────────

pub async fn get_path_health(State(state): State<AppState>) -> impl IntoResponse {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    match db::probe::latest_paths(&conn) {
        Ok(paths) => {
            let hops: Vec<serde_json::Value> = paths
                .into_iter()
                .map(|p| {
                    serde_json::json!({
                        "target_name": p.target_name,
                        "target_addr": p.target_addr,
                        "path_type": p.path_type,
                        "dest_host": p.dest_host,
                        "dest_loss_pct": p.dest_loss_pct,
                        "dest_avg_ms": p.dest_avg_ms,
                        "dest_severity": p.dest_severity,
                    })
                })
                .collect();
            Json(export::build_path_health_json(&hops)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("db query failed: {e:#}"),
        )
            .into_response(),
    }
}

// ── GET /api/cf ──────────────────────────────────────────────────────────────

pub async fn get_cf(State(state): State<AppState>) -> impl IntoResponse {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    match db::cf::list_cf_zones(&conn) {
        Ok(zones) => {
            let zone_values: Vec<serde_json::Value> = zones
                .into_iter()
                .map(|z| {
                    serde_json::json!({
                        "id": z.id,
                        "name": z.name,
                        "status": z.status,
                        "paused": z.paused,
                        "healthy": z.healthy,
                        "min_cert_expiry": z.min_cert_expiry
                            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
                    })
                })
                .collect();
            Json(export::build_cf_json(&zone_values)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("db query failed: {e:#}"),
        )
            .into_response(),
    }
}

// ════════════════════════════════════════════════════════════════════════════
//  HTML views (askama, server-rendered) — spec §3.8
// ════════════════════════════════════════════════════════════════════════════

/// 500 helper for HTML handlers (DB errors).
fn html_500(e: impl std::fmt::Display) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("error: {e:#}")).into_response()
}

fn tier_str(t: Tier) -> &'static str {
    match t {
        Tier::Agent => "agent",
        Tier::Agentless => "agentless",
    }
}

fn dedupe_str(k: DedupeKind) -> &'static str {
    match k {
        DedupeKind::Machinekey => "machinekey",
        DedupeKind::Alias => "alias",
        DedupeKind::Fuzzy => "fuzzy",
    }
}

/// Build the inventory rows from the DB, recomputing `online` from `last_seen`
/// freshness (never the stored flag) and flagging fuzzy-merged rows.
fn inventory_rows(
    state: &AppState,
    conn: &rusqlite::Connection,
) -> anyhow::Result<Vec<templates::InventoryRow>> {
    let nodes = db::nodes::list(conn)?;
    Ok(nodes
        .into_iter()
        .map(|n| templates::InventoryRow {
            online: is_online(n.last_seen, state.online_threshold),
            fuzzy: n.dedupe_key_kind == DedupeKind::Fuzzy,
            tier: tier_str(n.tier).to_owned(),
            site: n.tags.site.unwrap_or_default(),
            role: n.tags.role.unwrap_or_default(),
            owner: n.tags.owner.unwrap_or_default(),
            last_seen: n.last_seen.format("%Y-%m-%d %H:%M").to_string(),
            fleet_id: n.fleet_id,
            hostname: n.hostname,
        })
        .collect())
}

// ── GET / (caguastore launcher) ──────────────────────────────────────────────

/// Glyph keys present in the `store.html` sprite. A catalog entry with any
/// other `icon` value renders the generic `app` glyph instead of a broken ref.
const STORE_ICONS: &[&str] = &[
    "spade", "mountain", "hold", "cap", "kanban", "coin", "pulse", "gauge", "bell", "app",
];

/// The launcher home screen. Liveness LED per app: its catalog `port` appears
/// in a **non-stale** host_port row (any node — every catalog app lives on
/// caguaserver today; revisit if apps spread across hosts).
pub async fn get_store(State(state): State<AppState>) -> Response {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    // Fresh listening ports across the fleet.
    let fresh_ports: std::collections::HashSet<u16> = db::host::all_ports(&conn)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| !crate::model::is_stale(&r.collected_at, state.snapshot_stale_threshold))
        .map(|r| r.port)
        .collect();

    let tiles: Vec<templates::StoreTile> = state
        .store
        .apps
        .iter()
        .map(|a| {
            let icon = if STORE_ICONS.contains(&a.icon.as_str()) {
                a.icon.clone()
            } else {
                "app".to_owned()
            };
            templates::StoreTile {
                slug: a.slug.clone(),
                name: a.name.clone(),
                tagline: a.tagline.clone(),
                url: a.url.clone(),
                icon,
                hue: a.hue,
                has_led: a.port.is_some(),
                up: a.port.is_some_and(|p| fresh_ports.contains(&p)),
            }
        })
        .collect();

    let led_count = tiles.iter().filter(|t| t.has_led).count();
    let up_count = tiles.iter().filter(|t| t.up).count();
    templates::render(&templates::StorePage {
        tiles,
        up_count,
        led_count,
    })
}

// ── GET /inventory (mirrors `fleet list`) ────────────────────────────────────

/// `?partial=1` returns just the `<table>` fragment for HTMX `hx-get` refresh.
pub async fn get_index(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let rows = match inventory_rows(&state, &conn) {
        Ok(r) => r,
        Err(e) => return html_500(e),
    };

    if params.get("partial").is_some_and(|v| v == "1") {
        templates::render(&templates::InventoryTable { rows })
    } else {
        templates::render(&templates::InventoryPage { rows })
    }
}

// ── GET /node/{id} (detail, mirrors `fleet show`) ────────────────────────────

pub async fn get_node_html(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    let node = match db::nodes::get(&conn, &id) {
        Ok(Some(n)) => n,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return html_500(e),
    };

    // `get` does not populate seen_in — load it from node_seen.
    let seen_in = match db::nodes::load_seen_in(&conn, &node.fleet_id) {
        Ok(s) => s,
        Err(e) => return html_500(e),
    };

    let host_snapshot = match db::host::latest_for_node(&conn, &node.fleet_id) {
        Ok(Some(hs)) => {
            let node_cmds =
                db::host::commands_by_pid_for_node(&conn, &node.fleet_id).unwrap_or_default();
            let ports = db::host::ports_for_node(&conn, &node.fleet_id)
                .unwrap_or_default()
                .into_iter()
                .map(|p| templates::HostPortRow {
                    service: crate::service_label::resolve_service(
                        p.port,
                        node_cmds.get(&p.pid).map(String::as_str),
                        &p.process,
                        &state.labels,
                    ),
                    port: p.port,
                    proto: p.proto,
                    process: p.process,
                    pid: p.pid,
                    bind: p.bind,
                })
                .collect();
            let workloads_db =
                db::host::workloads_for_node(&conn, &node.fleet_id).unwrap_or_default();
            let rendered_count = workloads_db.len() as i64;
            let workloads = workloads_db
                .into_iter()
                .map(|w| templates::HostWorkloadRow {
                    label: w.label,
                    category: w.category,
                    process_count: w.process_count,
                    cpu_pct: fmt_pct(w.total_cpu_percent),
                    mem_human: fmt_bytes(w.total_memory_bytes),
                    example_command: truncate80(&w.example_command),
                })
                .collect();
            let showing_top_n_note = if hs.workload_count > rendered_count {
                Some(format!(
                    "showing top {} of {}",
                    rendered_count, hs.workload_count
                ))
            } else {
                None
            };
            Some(templates::HostSnapshotView {
                collected_at: hs.collected_at.clone(),
                stale: crate::model::is_stale(&hs.collected_at, state.snapshot_stale_threshold),
                cpu_pct: fmt_pct(hs.total_cpu_percent),
                mem_used: fmt_bytes(hs.used_memory_bytes),
                mem_total: fmt_bytes(hs.total_memory_bytes),
                gpu_pct: hs.gpu_percent.map(fmt_pct),
                ports,
                workloads,
                workload_count: hs.workload_count,
                showing_top_n_note,
            })
        }
        Ok(None) => None,
        Err(e) => return html_500(e),
    };

    let page = templates::NodePage {
        online: is_online(node.last_seen, state.online_threshold),
        tier: tier_str(node.tier).to_owned(),
        dedupe_key_kind: dedupe_str(node.dedupe_key_kind).to_owned(),
        role: node.tags.role.clone().unwrap_or_default(),
        owner: node.tags.owner.clone().unwrap_or_default(),
        site: node.tags.site.clone().unwrap_or_default(),
        gpu: node.tags.gpu.clone().unwrap_or_default(),
        last_seen: node.last_seen.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        addresses: node.addresses.clone(),
        seen_in: seen_in
            .into_iter()
            .map(|s| templates::SeenInRow {
                account: s.account,
                device_id: s.device_id,
            })
            .collect(),
        raw_tags: node.tags.raw.clone(),
        notes: node.notes.clone(),
        fleet_id: node.fleet_id,
        hostname: node.hostname,
        fqdn: node.fqdn,
        os: node.os,
        host_snapshot,
    };
    templates::render(&page)
}

// ── GET /paths (MTR path health) ─────────────────────────────────────────────

pub async fn get_paths_html(State(state): State<AppState>) -> Response {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    let paths = match db::probe::latest_paths(&conn) {
        Ok(p) => p,
        Err(e) => return html_500(e),
    };

    let page = templates::PathsPage {
        paths: paths
            .into_iter()
            .map(|p| templates::PathRow {
                target_name: p.target_name,
                target_addr: p.target_addr,
                path_type: p.path_type,
                dest_host: p.dest_host,
                dest_loss_pct: p.dest_loss_pct,
                dest_avg_ms: p.dest_avg_ms,
                dest_severity: p.dest_severity,
            })
            .collect(),
    };
    templates::render(&page)
}

// ── GET /ports (fleet-wide listening ports) ──────────────────────────────────

pub async fn get_ports_html(State(state): State<AppState>) -> Response {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    let rows = match db::host::all_ports(&conn) {
        Ok(r) => r,
        Err(e) => return html_500(e),
    };
    let cmds = db::host::commands_by_pid_all(&conn).unwrap_or_default();

    let page = templates::PortsPage {
        rows: rows
            .into_iter()
            .map(|r| {
                let command = cmds
                    .get(&r.node_id)
                    .and_then(|m| m.get(&r.pid))
                    .map(String::as_str);
                templates::FleetPortViewRow {
                    service: crate::service_label::resolve_service(
                        r.port,
                        command,
                        &r.process,
                        &state.labels,
                    ),
                    fleet_id: r.node_id,
                    hostname: r.hostname,
                    port: r.port,
                    proto: r.proto,
                    process: r.process,
                    pid: r.pid,
                    bind: r.bind,
                    collected_at: r.collected_at.clone(),
                    stale: crate::model::is_stale(&r.collected_at, state.snapshot_stale_threshold),
                }
            })
            .collect(),
    };
    templates::render(&page)
}

// ── GET /workloads (fleet-wide AI workloads) ──────────────────────────────────

pub async fn get_workloads_html(State(state): State<AppState>) -> Response {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    let rows = match db::host::all_workloads(&conn) {
        Ok(r) => r,
        Err(e) => return html_500(e),
    };

    // For each node, we need to know how many workload rows were rendered
    // to decide if "showing top N of M" note applies.
    // Group by node_id to count rendered rows per node.
    let mut node_rendered_counts: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    for r in &rows {
        *node_rendered_counts.entry(r.node_id.clone()).or_insert(0) += 1;
    }

    let page = templates::WorkloadsPage {
        rows: rows
            .into_iter()
            .map(|r| {
                let rendered_for_node = node_rendered_counts.get(&r.node_id).copied().unwrap_or(0);
                let showing_top_n_note = if r.workload_count > rendered_for_node {
                    Some(format!(
                        "showing top {} of {}",
                        rendered_for_node, r.workload_count
                    ))
                } else {
                    None
                };
                templates::FleetWorkloadViewRow {
                    fleet_id: r.node_id,
                    hostname: r.hostname,
                    label: r.label,
                    category: r.category,
                    process_count: r.process_count,
                    cpu_pct: fmt_pct(r.total_cpu_percent),
                    mem_human: fmt_bytes(r.total_memory_bytes),
                    example_command: truncate80(&r.example_command),
                    collected_at: r.collected_at.clone(),
                    stale: crate::model::is_stale(&r.collected_at, state.snapshot_stale_threshold),
                    showing_top_n_note,
                }
            })
            .collect(),
    };
    templates::render(&page)
}

// ── GET /api/ports ────────────────────────────────────────────────────────────

pub async fn get_api_ports(State(state): State<AppState>) -> impl IntoResponse {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    match db::host::all_ports(&conn) {
        Ok(rows) => Json(export::build_ports_json(
            &rows,
            state.snapshot_stale_threshold,
        ))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("db query failed: {e:#}"),
        )
            .into_response(),
    }
}

// ── GET /api/workloads ────────────────────────────────────────────────────────

pub async fn get_api_workloads(State(state): State<AppState>) -> impl IntoResponse {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    match db::host::all_workloads(&conn) {
        Ok(rows) => Json(export::build_workloads_json(
            &rows,
            state.snapshot_stale_threshold,
        ))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("db query failed: {e:#}"),
        )
            .into_response(),
    }
}

// ── GET /observability (CF zones + links-out + online rollup) ────────────────

pub async fn get_observability_html(State(state): State<AppState>) -> Response {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    // Registry-derived online rollup (R-10: NEVER from Kuma socket.io).
    let nodes = match db::nodes::list(&conn) {
        Ok(n) => n,
        Err(e) => return html_500(e),
    };
    let total_count = nodes.len();
    let online_count = nodes
        .iter()
        .filter(|n| is_online(n.last_seen, state.online_threshold))
        .count();
    let offline_count = total_count - online_count;

    let zones = match db::cf::list_cf_zones(&conn) {
        Ok(z) => z,
        Err(e) => return html_500(e),
    };
    let zone_rows = zones
        .into_iter()
        .map(|z| templates::ZoneRow {
            cert_expiry: z
                .min_cert_expiry
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "—".to_owned()),
            name: z.name,
            status: z.status,
            healthy: z.healthy,
        })
        .collect();

    let page = templates::ObservabilityPage {
        online_count,
        offline_count,
        total_count,
        beszel_ui_url: state.beszel_ui_url.clone(),
        kuma_ui_url: state.kuma_ui_url.clone(),
        zones: zone_rows,
    };
    templates::render(&page)
}

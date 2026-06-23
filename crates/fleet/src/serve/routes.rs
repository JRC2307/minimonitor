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

// ── GET / (inventory, mirrors `fleet list`) ──────────────────────────────────

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

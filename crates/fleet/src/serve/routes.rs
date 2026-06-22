//! Axum route handlers for `fleet serve` (spec §3.8).
//!
//! Each handler opens a read-only SQLite connection, loads data, and
//! serializes via the `export::build_*` builders — same shapes as
//! `fleet list --json`.

use std::path::PathBuf;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};

use crate::{db, export};

/// Shared state: the path to the SQLite registry file.
#[derive(Clone)]
pub struct AppState {
    pub db_path: PathBuf,
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

pub async fn get_path_health(State(_state): State<AppState>) -> impl IntoResponse {
    // Hops populated by `fleet probe` (Task 10); stub until then.
    Json(export::build_path_health_json(&[]))
}

// ── GET /api/cf ──────────────────────────────────────────────────────────────

pub async fn get_cf(State(_state): State<AppState>) -> impl IntoResponse {
    // Zones populated by `fleet cf-sync` (Task 9); stub until then.
    Json(export::build_cf_json(&[]))
}

//! Uptime-Kuma enroll — the agentless-tier half of `fleet enroll` (spec §3.7).
//!
//! Kuma has **no REST API** for monitor CRUD; management is its internal
//! **socket.io v4** API, which is version-coupled and breaking across releases.
//! Decision (R-1): run **pinned `louislam/uptime-kuma:1.23.16`** and speak native
//! Rust socket.io via `rust_socketio` behind the [`KumaClient`] trait.
//!
//! ## Layering — the trait is the ONLY unstable boundary
//!
//! - [`mod.rs`](self): the [`KumaClient`] trait, the **pure** [`reconcile`]
//!   (delete-guard + idempotent add/edit/delete planning, tested against a fake
//!   client), and the `enrollment`-table DB helpers. No wire protocol here.
//! - [`sio`]: the ONE place the socket.io wire protocol lives — connect, the
//!   pre-armed `monitorList` oneshot, `emit_with_ack` login/add/edit/delete.
//!   Kept thin and isolated so a Kuma protocol bump touches one file.
//!
//! ## Idempotency
//!
//! The monitor `name` = the node's `fleet_id` is the idempotency key. Because
//! Kuma's `editMonitor` needs the **full** object, [`reconcile`] always sends a
//! complete [`MonitorSpec`] on edit (never a partial patch). The resolved
//! `monitorID` is stored in `enrollment(system='kuma')`.

pub mod sio;

use crate::model::{MonitorSpec, RemoteMonitor};
use anyhow::Context;
use std::collections::HashMap;

/// Constant percent threshold for the decommission delete-guard (R-12).
/// Hardcoded — not a config knob (spec §3.7 note); matches the Beszel half.
pub const DELETE_GUARD_PCT: usize = 40;

/// The Kuma management surface — the ONLY unstable boundary, faked in tests.
///
/// `list` resolves against the **pushed** `monitorList` broadcast (the oneshot
/// armed before connect); `add`/`edit`/`delete` are `emit_with_ack` calls.
#[async_trait::async_trait]
pub trait KumaClient {
    /// All monitors currently on the server (from the pushed `monitorList`).
    async fn list(&self) -> anyhow::Result<Vec<RemoteMonitor>>;
    /// Create a monitor; returns the new `monitorID`.
    async fn add(&self, m: &MonitorSpec) -> anyhow::Result<i64>;
    /// Replace a monitor — Kuma needs the **full** object, so `m` is complete.
    async fn edit(&self, id: i64, m: &MonitorSpec) -> anyhow::Result<()>;
    /// Delete a monitor by id.
    async fn delete(&self, id: i64) -> anyhow::Result<()>;
}

/// True if the server monitor has drifted from the desired spec.
///
/// Compares the operator-meaningful fields reconcile manages: type, target
/// (url/hostname/port), interval, maxretries, and the notification wiring.
/// Server-defaulted fields we don't set are ignored.
fn drifted(have: &RemoteMonitor, want: &MonitorSpec) -> bool {
    have.monitor_type != want.monitor_type
        || have.url != want.url
        || have.hostname != want.hostname
        || have.port != want.port
        || have.interval != want.interval
        || have.maxretries != want.maxretries
        || have.notification_id_list != want.notification_id_list
}

/// **Pure** reconcile (spec §3.7) — drives `c` to match `want`.
///
/// - `name` (= `fleet_id`) is the idempotency key.
/// - absent → `add`; present + drifted → `edit` with the FULL object;
///   present + in-sync → no-op; undesired present → `delete`.
/// - **Delete-guard:** if `have` is non-empty and strictly more than `guard_pct`
///   of it would be deleted, abort **before any delete** (and before any
///   add/edit, so a partial-fleet blip changes nothing).
///
/// Boundary handling: empty `have` (nothing to delete/guard), empty `want`
/// (everything would delete → guard trips unless `have` also empty), exactly
/// `guard_pct` (does NOT trip — threshold is strictly greater), just over (trips).
pub async fn reconcile(
    c: &impl KumaClient,
    want: &[MonitorSpec],
    guard_pct: u8,
) -> anyhow::Result<()> {
    let have = c.list().await?;
    let by_name: HashMap<&str, &RemoteMonitor> =
        have.iter().map(|m| (m.name.as_str(), m)).collect();

    let to_delete: Vec<&RemoteMonitor> = have
        .iter()
        .filter(|m| !want.iter().any(|w| w.name == m.name))
        .collect();

    // Delete-guard FIRST — abort before any mutation if a too-large fraction of
    // the existing monitors would be removed (likely a partial-fleet blip).
    if !have.is_empty() && to_delete.len() * 100 / have.len() > guard_pct as usize {
        anyhow::bail!(
            "kuma delete-guard: {} of {} monitors would be removed (> {}%); aborting",
            to_delete.len(),
            have.len(),
            guard_pct
        );
    }

    for spec in want {
        match by_name.get(spec.name.as_str()) {
            Some(rm) if drifted(rm, spec) => c.edit(rm.id, spec).await?,
            Some(_) => {} // in sync — no-op
            None => {
                c.add(spec).await?; // never blind-add when present → no dupes
            }
        }
    }

    for m in to_delete {
        c.delete(m.id).await?;
    }

    Ok(())
}

// ─── enrollment-table DB helpers (system='kuma') ─────────────────────────────

/// A Kuma enrollment row from the local `enrollment` table.
#[derive(Debug, Clone, PartialEq)]
pub struct KumaEnrollmentRow {
    pub fleet_id: String,
    /// The Kuma `monitorID` (stored as text in `enrollment.remote_id`).
    pub remote_id: String,
}

/// Upsert a Kuma enrollment row (`system='kuma'`).
pub fn upsert_enrollment(
    conn: &rusqlite::Connection,
    fleet_id: &str,
    remote_id: &str,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO enrollment (fleet_id, system, remote_id, last_enrolled)
         VALUES (?1, 'kuma', ?2, ?3)
         ON CONFLICT(fleet_id, system) DO UPDATE SET
             remote_id     = excluded.remote_id,
             last_enrolled = excluded.last_enrolled",
        rusqlite::params![fleet_id, remote_id, now],
    )
    .context("upsert enrollment (kuma)")?;
    Ok(())
}

/// List all Kuma enrollment rows from the local DB.
pub fn list_enrollments(conn: &rusqlite::Connection) -> anyhow::Result<Vec<KumaEnrollmentRow>> {
    let mut stmt =
        conn.prepare("SELECT fleet_id, remote_id FROM enrollment WHERE system='kuma'")?;
    let rows: anyhow::Result<Vec<KumaEnrollmentRow>> = stmt
        .query_map([], |row| {
            Ok(KumaEnrollmentRow {
                fleet_id: row.get(0)?,
                remote_id: row.get(1)?,
            })
        })?
        .map(|r| r.context("kuma enrollment row"))
        .collect();
    rows
}

/// Delete a Kuma enrollment row from the local DB.
pub fn delete_enrollment(conn: &rusqlite::Connection, fleet_id: &str) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM enrollment WHERE fleet_id=?1 AND system='kuma'",
        rusqlite::params![fleet_id],
    )
    .context("delete enrollment (kuma)")?;
    Ok(())
}

#[cfg(test)]
mod tests;

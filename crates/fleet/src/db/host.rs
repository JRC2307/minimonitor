//! Write and read helpers for the M003 host-snapshot tables.
//!
//! All write functions go through `db::open`-returned connections
//! (which have `PRAGMA foreign_keys=ON`) so cascade deletes fire correctly.
//!
//! Read helpers use `WHERE hs.id IN (SELECT MAX(id) FROM host_snapshot GROUP BY node_id)`
//! to select only each node's newest snapshot — never GROUP BY node_id directly,
//! which would collapse multiple ports per node.

use anyhow::Context;
use minimonitor_core::snapshot::MonitorSnapshot;
use rusqlite::Connection;

use crate::secrets;

// ─── Row structs for read helpers ─────────────────────────────────────────────

/// The detail row returned by `latest_for_node` (spec §6.1).
/// Rollup columns only — no blob deserialization.
#[derive(Debug, Clone)]
pub struct HostSnapshotDetail {
    pub id: i64,
    pub node_id: String,
    pub collected_at: String,
    pub hostname: String,
    pub tailnet_ip: Option<String>,
    pub boot_epoch: i64,
    pub uptime_secs: i64,
    pub total_cpu_percent: f64,
    pub used_memory_bytes: i64,
    pub total_memory_bytes: i64,
    pub gpu_percent: Option<f64>,
    pub workload_count: i64,
    pub port_count: i64,
}

/// One port row from a node's newest snapshot (spec §6.2).
/// Includes the node hostname for fleet-wide `/ports` aggregation.
#[derive(Debug, Clone)]
pub struct FleetPortRow {
    pub node_id: String,
    pub hostname: String,
    pub collected_at: String,
    pub port: u16,
    pub proto: String,
    pub process: String,
    pub pid: i64,
    pub bind: String,
}

/// One workload row from a node's newest snapshot (spec §6.3).
/// Includes the node hostname for fleet-wide `/workloads` aggregation.
#[derive(Debug, Clone)]
pub struct FleetWorkloadRow {
    pub node_id: String,
    pub hostname: String,
    pub collected_at: String,
    pub label: String,
    pub category: String,
    pub process_count: i64,
    pub total_cpu_percent: f64,
    pub total_memory_bytes: i64,
    pub example_command: String,
    /// Total workload count from the parent `host_snapshot` row.
    /// Used by `/workloads` to display "showing top N of M" notes.
    pub workload_count: i64,
}

/// Insert one host snapshot (parent + child rows + status upsert) as a single
/// transaction.
///
/// Steps:
/// 1. Clone the snapshot and scrub all `ProcessRow.command` and
///    `AiWorkload.example_command` values via `secrets::scrub_command`.
/// 2. Serialize the scrubbed clone as `snapshot_json`.
/// 3. INSERT `host_snapshot` parent row with rollup columns.
/// 4. INSERT one `host_port` row per `snap.ports` entry.
/// 5. INSERT one `host_workload` row per `snap.ai_snapshot.top_workloads` entry
///    (using the scrubbed `example_command`).
/// 6. UPSERT `host_collect_status` success: `last_attempt_at = last_success_at = now`,
///    `last_error = NULL`.
/// 7. Commit. Returns the `host_snapshot.id` of the new row.
///
/// `collected_at` is the collector's `Utc::now().to_rfc3339()` — NOT the
/// payload's `captured_at`, which is a label (not rfc3339-comparable).
pub fn insert_snapshot(
    conn: &mut Connection,
    node_id: &str,
    _raw: &[u8],
    snap: &MonitorSnapshot,
    collected_at: &str,
) -> anyhow::Result<i64> {
    // 1. Clone and scrub the snapshot.
    let mut scrubbed = snap.clone();
    for p in &mut scrubbed.processes {
        p.command = secrets::scrub_command(&p.command);
    }
    for w in &mut scrubbed.ai_snapshot.top_workloads {
        w.example_command = secrets::scrub_command(&w.example_command);
    }

    // 2. Serialize the SCRUBBED clone as snapshot_json.
    let snapshot_json =
        serde_json::to_string(&scrubbed).context("serializing scrubbed snapshot")?;

    let tx = conn.transaction().context("begin insert_snapshot txn")?;

    // 3. INSERT host_snapshot parent.
    tx.execute(
        "INSERT INTO host_snapshot
             (node_id, collected_at, hostname, tailnet_ip, boot_epoch, uptime_secs,
              total_cpu_percent, used_memory_bytes, total_memory_bytes, gpu_percent,
              workload_count, port_count, snapshot_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        rusqlite::params![
            node_id,
            collected_at,
            snap.identity.hostname,
            snap.identity.tailnet_ip,
            snap.boot_epoch as i64,
            snap.uptime_secs as i64,
            snap.total_cpu_percent,
            snap.used_memory_bytes as i64,
            snap.total_memory_bytes as i64,
            snap.gpu_percent,
            snap.ai_snapshot.workload_count as i64,
            snap.ports.len() as i64,
            snapshot_json,
        ],
    )
    .context("insert host_snapshot")?;

    let snapshot_id = tx.last_insert_rowid();

    // 4. INSERT host_port rows.
    for port_row in &snap.ports {
        tx.execute(
            "INSERT INTO host_port (snapshot_id, node_id, port, proto, process, pid, bind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                snapshot_id,
                node_id,
                port_row.port as i64,
                port_row.proto,
                port_row.process,
                port_row.pid as i64,
                port_row.bind,
            ],
        )
        .context("insert host_port")?;
    }

    // 5. INSERT host_workload rows (scrubbed example_command from scrubbed clone).
    for wl in &scrubbed.ai_snapshot.top_workloads {
        tx.execute(
            "INSERT INTO host_workload
                 (snapshot_id, node_id, label, category, process_count,
                  total_cpu_percent, total_memory_bytes, example_command)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                snapshot_id,
                node_id,
                wl.label,
                wl.category,
                wl.process_count as i64,
                wl.total_cpu_percent,
                wl.total_memory_bytes as i64,
                wl.example_command,
            ],
        )
        .context("insert host_workload")?;
    }

    // 6. UPSERT host_collect_status success.
    tx.execute(
        "INSERT INTO host_collect_status (node_id, last_attempt_at, last_success_at, last_error)
         VALUES (?1, ?2, ?2, NULL)
         ON CONFLICT(node_id) DO UPDATE SET
             last_attempt_at = excluded.last_attempt_at,
             last_success_at = excluded.last_success_at,
             last_error      = NULL",
        rusqlite::params![node_id, collected_at],
    )
    .context("upsert host_collect_status (success)")?;

    // 7. Commit.
    tx.commit().context("commit insert_snapshot txn")?;

    Ok(snapshot_id)
}

/// Delete old host snapshots, but keep each node's latest (highest `id`).
///
/// SQL:
/// ```sql
/// DELETE FROM host_snapshot
///  WHERE collected_at < ?cutoff
///    AND id NOT IN (SELECT MAX(id) FROM host_snapshot GROUP BY node_id)
/// ```
///
/// `cutoff` must be a valid RFC3339 string (same format as `collected_at`).
/// Child rows (`host_port`, `host_workload`) cascade-delete automatically.
/// Returns the count of deleted parent rows.
pub fn retention_sweep(conn: &mut Connection, cutoff: &str) -> anyhow::Result<usize> {
    let tx = conn.transaction().context("begin retention_sweep txn")?;
    let deleted = tx
        .execute(
            "DELETE FROM host_snapshot
              WHERE collected_at < ?1
                AND id NOT IN (SELECT MAX(id) FROM host_snapshot GROUP BY node_id)",
            rusqlite::params![cutoff],
        )
        .context("retention_sweep DELETE")?;
    tx.commit().context("commit retention_sweep txn")?;
    Ok(deleted)
}

/// Record a collect failure for a node.
///
/// UPSERTs `host_collect_status`, touching ONLY `last_attempt_at` and
/// `last_error`. `last_success_at` is LEFT INTACT so the prior good
/// snapshot's freshness is preserved.
///
/// `attempt_at` should be the caller's `Utc::now().to_rfc3339()`.
pub fn record_collect_failure(
    conn: &Connection,
    node_id: &str,
    attempt_at: &str,
    error: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO host_collect_status (node_id, last_attempt_at, last_error)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(node_id) DO UPDATE SET
             last_attempt_at = excluded.last_attempt_at,
             last_error      = excluded.last_error",
        rusqlite::params![node_id, attempt_at, error],
    )
    .context("upsert host_collect_status (failure)")?;
    Ok(())
}

// ─── Read helpers (spec §6) ───────────────────────────────────────────────────
//
// All helpers select only each node's NEWEST snapshot via:
//   WHERE hs.id IN (SELECT MAX(id) FROM host_snapshot GROUP BY node_id)
//
// This correctly handles multiple snapshots per node and returns ALL child rows
// (ports, workloads) for the newest snapshot — never collapsed by GROUP BY node.

/// Return the newest `host_snapshot` row for `node_id`, or `None` if no snapshot exists.
/// "Newest" = highest `id` (AUTOINCREMENT — monotonically increasing).
/// Returns rollup columns only; no blob deserialization.
pub fn latest_for_node(
    conn: &Connection,
    node_id: &str,
) -> anyhow::Result<Option<HostSnapshotDetail>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, node_id, collected_at, hostname, tailnet_ip,
                    boot_epoch, uptime_secs, total_cpu_percent,
                    used_memory_bytes, total_memory_bytes, gpu_percent,
                    workload_count, port_count
             FROM host_snapshot
             WHERE node_id = ?1
             ORDER BY id DESC
             LIMIT 1",
        )
        .context("prepare latest_for_node")?;

    let mut rows = stmt
        .query_map(rusqlite::params![node_id], |r| {
            Ok(HostSnapshotDetail {
                id: r.get(0)?,
                node_id: r.get(1)?,
                collected_at: r.get(2)?,
                hostname: r.get(3)?,
                tailnet_ip: r.get(4)?,
                boot_epoch: r.get(5)?,
                uptime_secs: r.get(6)?,
                total_cpu_percent: r.get(7)?,
                used_memory_bytes: r.get(8)?,
                total_memory_bytes: r.get(9)?,
                gpu_percent: r.get(10)?,
                workload_count: r.get(11)?,
                port_count: r.get(12)?,
            })
        })
        .context("query latest_for_node")?;

    match rows.next() {
        Some(row) => Ok(Some(row.context("map latest_for_node row")?)),
        None => Ok(None),
    }
}

/// Return ALL ports for EACH node's newest snapshot, across all nodes.
///
/// Uses `WHERE hp.snapshot_id IN (SELECT MAX(id) FROM host_snapshot GROUP BY node_id)`
/// to join on the newest snapshot per node. Every port of the newest snapshot is
/// returned — never collapsed by GROUP BY node.
pub fn all_ports(conn: &Connection) -> anyhow::Result<Vec<FleetPortRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT hp.node_id, hs.hostname, hs.collected_at,
                    hp.port, hp.proto, hp.process, hp.pid, hp.bind
             FROM host_port hp
             JOIN host_snapshot hs ON hs.id = hp.snapshot_id
             WHERE hs.id IN (SELECT MAX(id) FROM host_snapshot GROUP BY node_id)
             ORDER BY hp.node_id, hp.port",
        )
        .context("prepare all_ports")?;

    let rows = stmt
        .query_map([], |r| {
            Ok(FleetPortRow {
                node_id: r.get(0)?,
                hostname: r.get(1)?,
                collected_at: r.get(2)?,
                port: r.get::<_, i64>(3)? as u16,
                proto: r.get(4)?,
                process: r.get(5)?,
                pid: r.get(6)?,
                bind: r.get(7)?,
            })
        })
        .context("query all_ports")?;

    rows.map(|r| r.context("map all_ports row"))
        .collect::<anyhow::Result<Vec<_>>>()
}

/// Return ALL workloads for EACH node's newest snapshot, ordered by `total_cpu_percent DESC`.
pub fn all_workloads(conn: &Connection) -> anyhow::Result<Vec<FleetWorkloadRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT hw.node_id, hs.hostname, hs.collected_at,
                    hw.label, hw.category, hw.process_count,
                    hw.total_cpu_percent, hw.total_memory_bytes, hw.example_command,
                    hs.workload_count
             FROM host_workload hw
             JOIN host_snapshot hs ON hs.id = hw.snapshot_id
             WHERE hs.id IN (SELECT MAX(id) FROM host_snapshot GROUP BY node_id)
             ORDER BY hw.total_cpu_percent DESC",
        )
        .context("prepare all_workloads")?;

    let rows = stmt
        .query_map([], |r| {
            Ok(FleetWorkloadRow {
                node_id: r.get(0)?,
                hostname: r.get(1)?,
                collected_at: r.get(2)?,
                label: r.get(3)?,
                category: r.get(4)?,
                process_count: r.get(5)?,
                total_cpu_percent: r.get(6)?,
                total_memory_bytes: r.get(7)?,
                example_command: r.get(8)?,
                workload_count: r.get(9)?,
            })
        })
        .context("query all_workloads")?;

    rows.map(|r| r.context("map all_workloads row"))
        .collect::<anyhow::Result<Vec<_>>>()
}

/// Return ALL ports for `node_id`'s newest snapshot.
pub fn ports_for_node(conn: &Connection, node_id: &str) -> anyhow::Result<Vec<FleetPortRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT hp.node_id, hs.hostname, hs.collected_at,
                    hp.port, hp.proto, hp.process, hp.pid, hp.bind
             FROM host_port hp
             JOIN host_snapshot hs ON hs.id = hp.snapshot_id
             WHERE hp.node_id = ?1
               AND hs.id IN (SELECT MAX(id) FROM host_snapshot WHERE node_id = ?1)
             ORDER BY hp.port",
        )
        .context("prepare ports_for_node")?;

    let rows = stmt
        .query_map(rusqlite::params![node_id], |r| {
            Ok(FleetPortRow {
                node_id: r.get(0)?,
                hostname: r.get(1)?,
                collected_at: r.get(2)?,
                port: r.get::<_, i64>(3)? as u16,
                proto: r.get(4)?,
                process: r.get(5)?,
                pid: r.get(6)?,
                bind: r.get(7)?,
            })
        })
        .context("query ports_for_node")?;

    rows.map(|r| r.context("map ports_for_node row"))
        .collect::<anyhow::Result<Vec<_>>>()
}

/// Return ALL workloads for `node_id`'s newest snapshot.
pub fn workloads_for_node(
    conn: &Connection,
    node_id: &str,
) -> anyhow::Result<Vec<FleetWorkloadRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT hw.node_id, hs.hostname, hs.collected_at,
                    hw.label, hw.category, hw.process_count,
                    hw.total_cpu_percent, hw.total_memory_bytes, hw.example_command,
                    hs.workload_count
             FROM host_workload hw
             JOIN host_snapshot hs ON hs.id = hw.snapshot_id
             WHERE hw.node_id = ?1
               AND hs.id IN (SELECT MAX(id) FROM host_snapshot WHERE node_id = ?1)
             ORDER BY hw.total_cpu_percent DESC",
        )
        .context("prepare workloads_for_node")?;

    let rows = stmt
        .query_map(rusqlite::params![node_id], |r| {
            Ok(FleetWorkloadRow {
                node_id: r.get(0)?,
                hostname: r.get(1)?,
                collected_at: r.get(2)?,
                label: r.get(3)?,
                category: r.get(4)?,
                process_count: r.get(5)?,
                total_cpu_percent: r.get(6)?,
                total_memory_bytes: r.get(7)?,
                example_command: r.get(8)?,
                workload_count: r.get(9)?,
            })
        })
        .context("query workloads_for_node")?;

    rows.map(|r| r.context("map workloads_for_node row"))
        .collect::<anyhow::Result<Vec<_>>>()
}

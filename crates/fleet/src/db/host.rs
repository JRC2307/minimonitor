//! Write helpers for the M003 host-snapshot tables.
//!
//! All three public functions go through `db::open`-returned connections
//! (which have `PRAGMA foreign_keys=ON`) so cascade deletes fire correctly.

use anyhow::Context;
use minimonitor_core::snapshot::MonitorSnapshot;
use rusqlite::Connection;

use crate::secrets;

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

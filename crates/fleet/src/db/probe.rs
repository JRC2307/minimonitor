//! Database operations for `probe_run` + `probe_hop`, plus the retention sweep.
//!
//! Aggregation (spec §5): one `probe_run` row per (target, run) carrying the
//! `path_type`, and N `probe_hop` rows with the mtr-style loss%/RTT stats and a
//! precomputed `severity` string.
//!
//! Retention (R-13): [`retention_sweep`] runs in **its own transaction at the
//! very start of the command**, so even a breach early-return cannot skip GC of
//! `probe_run` rows older than `retention_days` (cascading to their hops).

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use rusqlite::Connection;

use crate::probe::{HopStat, PathType};

/// Metadata for one probe run (everything but the hops).
#[derive(Debug, Clone)]
pub struct RunMeta<'a> {
    pub target_name: &'a str,
    pub target_addr: &'a str,
    pub path_type: PathType,
    pub cycles: u32,
    /// Whether the destination-hop policy fired for this run.
    pub breached: bool,
    /// Run-start timestamp (stored RFC3339 UTC).
    pub ts: DateTime<Utc>,
}

/// Insert a `probe_run` and its `probe_hop` rows in one transaction; returns the
/// new run id.
pub fn insert_run(
    conn: &mut Connection,
    meta: &RunMeta<'_>,
    hops: &[HopStat],
) -> anyhow::Result<i64> {
    let tx = conn.transaction().context("probe insert_run: begin txn")?;
    tx.execute(
        "INSERT INTO probe_run (ts, target_name, target_addr, path_type, cycles, breached)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            meta.ts.to_rfc3339(),
            meta.target_name,
            meta.target_addr,
            meta.path_type.as_str(),
            meta.cycles,
            meta.breached as i64,
        ],
    )
    .context("insert probe_run")?;
    let run_id = tx.last_insert_rowid();

    for h in hops {
        tx.execute(
            "INSERT INTO probe_hop
                (run_id, ttl, host, sent, recv, loss_pct,
                 last_ms, avg_ms, best_ms, wrst_ms, stdev_ms, severity)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                run_id,
                h.ttl,
                h.host,
                h.sent,
                h.recv,
                h.loss_pct,
                h.last_ms,
                h.avg_ms,
                h.best_ms,
                h.worst_ms,
                h.stddev_ms,
                h.severity.as_str(),
            ],
        )
        .with_context(|| format!("insert probe_hop ttl={}", h.ttl))?;
    }

    tx.commit().context("probe insert_run: commit")?;
    Ok(run_id)
}

/// Delete every `probe_run` (and, via cascade, its `probe_hop` rows) older than
/// `retention_days` — in **its own transaction** (R-13). Returns the number of
/// runs deleted.
///
/// Called FIRST in the probe command, before any tracing, so a later breach
/// early-return can never skip GC.
pub fn retention_sweep(conn: &mut Connection, retention_days: u32) -> anyhow::Result<usize> {
    let cutoff = (Utc::now() - Duration::days(i64::from(retention_days))).to_rfc3339();
    let tx = conn.transaction().context("retention_sweep: begin txn")?;
    let n = tx
        .execute("DELETE FROM probe_run WHERE ts < ?1", [&cutoff])
        .context("retention_sweep: delete old runs")?;
    tx.commit().context("retention_sweep: commit")?;
    Ok(n)
}

/// A persisted hop row (read back for tests / `path-health.json`).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredHop {
    pub ttl: u8,
    pub host: Option<String>,
    pub loss_pct: f64,
    pub avg_ms: f64,
    pub severity: String,
}

/// Read all hops of a run, ordered by ttl.
pub fn hops_for_run(conn: &Connection, run_id: i64) -> anyhow::Result<Vec<StoredHop>> {
    let mut stmt = conn.prepare(
        "SELECT ttl, host, loss_pct, avg_ms, severity
         FROM probe_hop WHERE run_id = ?1 ORDER BY ttl",
    )?;
    let rows = stmt
        .query_map([run_id], |r| {
            Ok(StoredHop {
                ttl: r.get(0)?,
                host: r.get(1)?,
                loss_pct: r.get(2)?,
                avg_ms: r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                severity: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .context("reading probe_hop rows")?;
    Ok(rows)
}

/// Count `probe_run` rows (test helper / health).
pub fn count_runs(conn: &Connection) -> anyhow::Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM probe_run", [], |r| r.get(0))?)
}

/// The latest probe run per target, summarized to its **destination hop**
/// (the last responding hop) — what `fleet serve`'s `/paths` view renders
/// (spec §3.8 / §5). Targets without any responding hop fall back to the
/// highest-ttl hop. Ordered by target name.
#[derive(Debug, Clone, PartialEq)]
pub struct LatestPath {
    pub target_name: String,
    pub target_addr: String,
    pub path_type: String,
    pub dest_host: Option<String>,
    pub dest_loss_pct: f64,
    pub dest_avg_ms: f64,
    pub dest_severity: String,
}

/// For each target, find its most recent `probe_run` and return the
/// destination-hop summary (last responding hop, else highest ttl).
pub fn latest_paths(conn: &Connection) -> anyhow::Result<Vec<LatestPath>> {
    let mut stmt = conn.prepare(
        "SELECT pr.id, pr.target_name, pr.target_addr, pr.path_type
         FROM probe_run pr
         JOIN (
            SELECT target_name, MAX(ts) AS max_ts
            FROM probe_run GROUP BY target_name
         ) latest
         ON pr.target_name = latest.target_name AND pr.ts = latest.max_ts
         GROUP BY pr.target_name
         ORDER BY pr.target_name",
    )?;
    let runs = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()
        .context("latest_paths: runs")?;

    let mut out = Vec::with_capacity(runs.len());
    for (run_id, target_name, target_addr, path_type) in runs {
        let hops = hops_for_run(conn, run_id)?;
        // Destination = last RESPONDING hop; fallback = last hop overall.
        let dest = hops
            .iter()
            .rev()
            .find(|h| h.host.is_some())
            .or_else(|| hops.last());
        let (dest_host, dest_loss_pct, dest_avg_ms, dest_severity) = match dest {
            Some(h) => (h.host.clone(), h.loss_pct, h.avg_ms, h.severity.clone()),
            None => (None, 0.0, 0.0, "ok".to_owned()),
        };
        out.push(LatestPath {
            target_name,
            target_addr,
            path_type,
            dest_host,
            dest_loss_pct,
            dest_avg_ms,
            dest_severity,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::probe::{Severity, tests::hop};
    use tempfile::NamedTempFile;

    fn open_temp() -> (NamedTempFile, Connection) {
        let f = NamedTempFile::new().unwrap();
        let conn = db::open(f.path()).unwrap();
        (f, conn)
    }

    fn meta<'a>(name: &'a str, addr: &'a str, pt: PathType, ts: DateTime<Utc>) -> RunMeta<'a> {
        RunMeta {
            target_name: name,
            target_addr: addr,
            path_type: pt,
            cycles: 10,
            breached: false,
            ts,
        }
    }

    #[test]
    fn insert_run_persists_run_and_hops_with_path_type() {
        let (_f, mut conn) = open_temp();
        let mut hops = vec![
            hop(1, Some("192.168.1.1"), 0.0, 2.0),
            hop(2, None, 100.0, 0.0),
            hop(3, Some("1.1.1.1"), 0.0, 12.0),
        ];
        hops[2].severity = Severity::Ok;

        let run_id = insert_run(
            &mut conn,
            &meta("cloudflare-dns", "1.1.1.1", PathType::Overlay, Utc::now()),
            &hops,
        )
        .unwrap();
        assert!(run_id >= 1);

        // one run, path_type stored
        let (pt, cycles): (String, i64) = conn
            .query_row(
                "SELECT path_type, cycles FROM probe_run WHERE id=?1",
                [run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(pt, "overlay", "path_type stored per target");
        assert_eq!(cycles, 10);

        // N hop rows
        let stored = hops_for_run(&conn, run_id).unwrap();
        assert_eq!(stored.len(), 3);
        assert_eq!(stored[0].host.as_deref(), Some("192.168.1.1"));
        assert_eq!(stored[1].host, None, "??? hop stored with NULL host");
        assert!((stored[1].loss_pct - 100.0).abs() < f64::EPSILON);
        assert_eq!(stored[2].host.as_deref(), Some("1.1.1.1"));
    }

    #[test]
    fn deleting_run_cascades_to_hops() {
        let (_f, mut conn) = open_temp();
        let hops = vec![hop(1, Some("1.1.1.1"), 0.0, 5.0)];
        let run_id = insert_run(
            &mut conn,
            &meta("t", "1.1.1.1", PathType::Underlay, Utc::now()),
            &hops,
        )
        .unwrap();
        conn.execute("DELETE FROM probe_run WHERE id=?1", [run_id])
            .unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM probe_hop WHERE run_id=?1",
                [run_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "probe_hop must cascade-delete with its run");
    }

    #[test]
    fn retention_sweep_removes_old_runs_keeps_recent() {
        let (_f, mut conn) = open_temp();
        let hops = vec![hop(1, Some("1.1.1.1"), 0.0, 5.0)];

        // An OLD run (40 days ago) and a fresh run.
        let old_ts = Utc::now() - Duration::days(40);
        let old_id = insert_run(
            &mut conn,
            &meta("old", "1.1.1.1", PathType::Underlay, old_ts),
            &hops,
        )
        .unwrap();
        let fresh_id = insert_run(
            &mut conn,
            &meta("fresh", "1.1.1.1", PathType::Underlay, Utc::now()),
            &hops,
        )
        .unwrap();

        let deleted = retention_sweep(&mut conn, 30).unwrap();
        assert_eq!(deleted, 1, "exactly the >30d run is swept");

        assert_eq!(count_runs(&conn).unwrap(), 1);
        // old gone, fresh kept
        let old_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM probe_run WHERE id=?1",
                [old_id],
                |r| r.get(0),
            )
            .unwrap();
        let fresh_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM probe_run WHERE id=?1",
                [fresh_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_exists, 0);
        assert_eq!(fresh_exists, 1);
        // and the old run's hops cascaded
        let orphan_hops: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM probe_hop WHERE run_id=?1",
                [old_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphan_hops, 0);
    }
}

//! `fleet probe` — the MTR path prober pipeline (spec §5).
//!
//! Order of operations (the load-bearing sequence):
//!   1. **Retention sweep FIRST, in its own txn (R-13)** — so a later breach
//!      early-return can never skip GC of old `probe_run` rows.
//!   2. Resolve targets: pinned `[[probe.target]]` + registry-derived
//!      `[[probe.selector]]` (tag-matched nodes).
//!   3. Per target: `trace()` via `spawn_blocking` (trippy + SQLite are sync) →
//!      stamp severities → persist one `probe_run` + N `probe_hop` → evaluate the
//!      destination-hop-only policy → ntfy at priority 4 on breach.
//!   4. Rebuild `path-health.json` from the latest run per target.
//!
//! Live tracing is never exercised in tests; the DB-backed retention-on-breach
//! ordering and the path-health rebuild are.

use crate::alert;
use crate::config::{Config, NtfyConfig, ProbeConfig};
use crate::db;
use crate::db::probe as dbprobe;
use crate::export::{PathHealthExport, build_path_health_json};
use crate::probe::{self, HopStat, PathType, RealSocketChecker, SocketChecker};
use anyhow::Context;
use chrono::Utc;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// A resolved probe target (pinned or registry-derived).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedTarget {
    pub name: String,
    pub addr: IpAddr,
    pub path_type: PathType,
}

/// Resolve the configured pinned `[[probe.target]]` entries into typed targets.
///
/// Registry-derived `[[probe.selector]]` targets are resolved separately (they
/// need a DB lookup); see [`resolve_selector_targets`]. Invalid addresses are
/// skipped with a warning rather than aborting the whole run.
pub fn resolve_pinned_targets(cfg: &ProbeConfig) -> Vec<ResolvedTarget> {
    cfg.target
        .iter()
        .filter_map(|t| match t.addr.parse::<IpAddr>() {
            Ok(addr) => Some(ResolvedTarget {
                name: t.name.clone(),
                addr,
                path_type: PathType::parse(&t.path),
            }),
            Err(e) => {
                eprintln!(
                    "probe: skipping target {} — bad addr {:?}: {e}",
                    t.name, t.addr
                );
                None
            }
        })
        .collect()
}

/// Resolve registry-derived `[[probe.selector]]` targets: for each selector,
/// match nodes by `match_tag` (`facet:value`) and probe each matching node's
/// first v4 tailnet address.
pub fn resolve_selector_targets(
    conn: &rusqlite::Connection,
    cfg: &ProbeConfig,
) -> anyhow::Result<Vec<ResolvedTarget>> {
    let mut out = Vec::new();
    for sel in &cfg.selector {
        let Some((facet, value)) = sel.match_tag.split_once(':') else {
            eprintln!(
                "probe: skipping selector — bad match_tag {:?}",
                sel.match_tag
            );
            continue;
        };
        let filter = db::nodes::ListFilter {
            tag_facet: Some(facet.to_owned()),
            tag_value: Some(value.to_owned()),
            tier: None,
        };
        let nodes = db::nodes::list_filtered(conn, &filter)?;
        let path_type = PathType::parse(&sel.path);
        for n in nodes {
            // first parseable v4 address
            if let Some(addr) = n
                .addresses
                .iter()
                .filter_map(|a| a.parse::<IpAddr>().ok())
                .find(IpAddr::is_ipv4)
            {
                out.push(ResolvedTarget {
                    name: n.fleet_id.clone(),
                    addr,
                    path_type,
                });
            }
        }
    }
    Ok(out)
}

/// Persist one trace result: stamp severities, evaluate the destination-hop-only
/// policy, write the run + hops, and return whether it breached (so the caller
/// can ntfy). Pure-ish — only touches the DB, no network/tracing.
pub fn persist_and_evaluate(
    conn: &mut rusqlite::Connection,
    target: &ResolvedTarget,
    cfg: &ProbeConfig,
    mut hops: Vec<HopStat>,
) -> anyhow::Result<(i64, Option<probe::Alert>)> {
    probe::apply_severities(&mut hops, cfg.loss_threshold_pct, cfg.rtt_threshold_ms);
    let alert = probe::evaluate(&hops, cfg.loss_threshold_pct, cfg.rtt_threshold_ms);
    let run_id = dbprobe::insert_run(
        conn,
        &dbprobe::RunMeta {
            target_name: &target.name,
            target_addr: &target.addr.to_string(),
            path_type: target.path_type,
            cycles: cfg.cycles,
            breached: alert.is_some(),
            ts: Utc::now(),
        },
        &hops,
    )?;
    Ok((run_id, alert))
}

/// Rebuild the path-health summary from the **latest run per target**: each
/// target contributes its destination-hop summary with precomputed `severity`.
/// Returns the typed export (also serialized to `path-health.json` by the caller).
pub fn build_path_health(conn: &rusqlite::Connection) -> anyhow::Result<PathHealthExport> {
    // latest run id per target_name
    let mut stmt = conn.prepare(
        "SELECT pr.id, pr.target_name, pr.target_addr, pr.path_type, pr.ts, pr.breached
         FROM probe_run pr
         JOIN (SELECT target_name, MAX(ts) AS mts FROM probe_run GROUP BY target_name) latest
           ON pr.target_name = latest.target_name AND pr.ts = latest.mts
         ORDER BY pr.target_name",
    )?;
    let runs = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, bool>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()
        .context("path-health: querying latest runs")?;

    let mut summaries = Vec::new();
    for (run_id, name, addr, path_type, ts, breached) in runs {
        let hops = dbprobe::hops_for_run(conn, run_id)?;
        // destination = last responding hop (host present)
        let dest = hops.iter().rev().find(|h| h.host.is_some());
        let summary = serde_json::json!({
            "target": name,
            "addr": addr,
            "path_type": path_type,
            "ts": ts,
            "breached": breached,
            "dest_ttl": dest.map(|d| d.ttl),
            "dest_host": dest.and_then(|d| d.host.clone()),
            "dest_loss_pct": dest.map(|d| d.loss_pct),
            "dest_avg_ms": dest.map(|d| d.avg_ms),
            "severity": dest.map_or("ok", |d| d.severity.as_str()),
            "hop_count": hops.len(),
        });
        summaries.push(summary);
    }
    Ok(build_path_health_json(&summaries))
}

/// Default path for the path-health export: alongside the YAML export.
pub fn path_health_path(cfg: &Config) -> PathBuf {
    Path::new(&cfg.export_yaml_path).parent().map_or_else(
        || PathBuf::from("path-health.json"),
        |p| p.join("path-health.json"),
    )
}

/// Write the path-health export to `path` as pretty JSON.
pub fn write_path_health(export: &PathHealthExport, path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating dir {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(export).context("serializing path-health.json")?;
    std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Full `fleet probe` pipeline against real sockets (production entry point).
pub async fn run(cfg: &Config, db_path: &Path) -> anyhow::Result<()> {
    run_with_checker(cfg, db_path, &RealSocketChecker).await
}

/// Like [`run`] but with an injectable [`SocketChecker`] (the trippy self-check
/// seam). Still traces live — NOT for tests; the seam exists so the binary can
/// be wired without privileges in a doctor-style preflight.
pub async fn run_with_checker(
    cfg: &Config,
    db_path: &Path,
    checker: &(dyn SocketChecker + Sync),
) -> anyhow::Result<()> {
    let probe_cfg = cfg
        .probe
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("fleet probe: [probe] section missing from config"))?;

    let mut conn = db::open(db_path)?;

    // 1. Retention FIRST, own txn (R-13) — even a later breach can't skip this.
    let swept = dbprobe::retention_sweep(&mut conn, probe_cfg.retention_days)?;
    if swept > 0 {
        eprintln!(
            "probe: retention swept {swept} run(s) older than {} days",
            probe_cfg.retention_days
        );
    }

    // 2. Resolve targets (pinned + registry-derived).
    let mut targets = resolve_pinned_targets(probe_cfg);
    targets.extend(resolve_selector_targets(&conn, probe_cfg)?);

    // 3. Per target: trace (blocking) → persist → evaluate → ntfy on breach.
    for target in &targets {
        let addr = target.addr;
        let cycles = probe_cfg.cycles as usize;
        // trippy + the checker are sync/blocking; run off the async runtime.
        let hops = match trace_blocking(addr, cycles, checker).await {
            Ok(h) => h,
            Err(e) => {
                eprintln!("probe: trace {} ({}) failed: {e}", target.name, addr);
                continue;
            }
        };
        let (_run_id, alert) = persist_and_evaluate(&mut conn, target, probe_cfg, hops)?;
        if let Some(a) = alert
            && let Some(ntfy_cfg) = cfg.ntfy.as_ref()
        {
            emit_breach(ntfy_cfg, target, &a, None).await;
        }
    }

    // 4. Rebuild path-health.json from the latest run per target.
    let export = build_path_health(&conn)?;
    let out = path_health_path(cfg);
    write_path_health(&export, &out)?;
    eprintln!("probe: wrote {}", out.display());
    Ok(())
}

/// Run a blocking trace without blocking the async runtime.
///
/// `checker` is borrowed across `spawn_blocking`, so we trace on the current
/// thread via `block_in_place` instead of moving it — fine for a short-lived CLI.
async fn trace_blocking(
    addr: IpAddr,
    cycles: usize,
    checker: &(dyn SocketChecker + Sync),
) -> anyhow::Result<Vec<HopStat>> {
    tokio::task::block_in_place(|| probe::trace(addr, cycles, checker))
}

/// Publish a breach to ntfy at priority 4 (spec §5).
pub async fn emit_breach(
    ntfy_cfg: &NtfyConfig,
    target: &ResolvedTarget,
    alert: &probe::Alert,
    ntfy_base: Option<&str>,
) {
    let host = alert.host.as_deref().unwrap_or("???");
    let title = format!("Fleet: path degraded — {}", target.name);
    let msg = format!(
        "{} ({}, {}) hop {} [{}]: loss {:.0}%, avg {:.0}ms",
        target.name,
        target.addr,
        target.path_type.as_str(),
        alert.ttl,
        host,
        alert.loss_pct,
        alert.avg_ms,
    );
    if let Err(e) = alert::ntfy_with_base(ntfy_cfg, &title, &msg, 4, ntfy_base).await {
        eprintln!("probe: ntfy breach alert failed for {}: {e}", target.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProbeConfig, ProbeTarget};
    use crate::probe::tests::hop;
    use chrono::Duration;
    use tempfile::NamedTempFile;

    fn probe_cfg() -> ProbeConfig {
        ProbeConfig {
            cycles: 10,
            per_hop_timeout_ms: 1500,
            loss_threshold_pct: 20.0,
            rtt_threshold_ms: 250.0,
            retention_days: 30,
            target: vec![],
            selector: vec![],
        }
    }

    fn open_temp() -> (NamedTempFile, rusqlite::Connection) {
        let f = NamedTempFile::new().unwrap();
        let conn = db::open(f.path()).unwrap();
        (f, conn)
    }

    fn target(name: &str, addr: &str, pt: PathType) -> ResolvedTarget {
        ResolvedTarget {
            name: name.to_owned(),
            addr: addr.parse().unwrap(),
            path_type: pt,
        }
    }

    // ── pinned target resolution ─────────────────────────────────────────────

    #[test]
    fn resolve_pinned_parses_addrs_and_path_type() {
        let mut cfg = probe_cfg();
        cfg.target = vec![
            ProbeTarget {
                name: "dns".into(),
                addr: "1.1.1.1".into(),
                path: "underlay".into(),
            },
            ProbeTarget {
                name: "ov".into(),
                addr: "100.64.0.1".into(),
                path: "overlay".into(),
            },
            ProbeTarget {
                name: "bad".into(),
                addr: "not-an-ip".into(),
                path: "underlay".into(),
            },
        ];
        let resolved = resolve_pinned_targets(&cfg);
        assert_eq!(resolved.len(), 2, "the bad addr is skipped, not fatal");
        assert_eq!(resolved[0].path_type, PathType::Underlay);
        assert_eq!(resolved[1].path_type, PathType::Overlay);
    }

    // ── persist + evaluate: breach detection & severity stamping ─────────────

    #[test]
    fn persist_breaching_dest_marks_run_breached_and_returns_alert() {
        let (_f, mut conn) = open_temp();
        let t = target("dns", "1.1.1.1", PathType::Underlay);
        let hops = vec![
            hop(1, Some("192.168.1.1"), 0.0, 2.0),
            hop(2, Some("1.1.1.1"), 60.0, 30.0), // breaching dest
        ];
        let (run_id, alert) = persist_and_evaluate(&mut conn, &t, &probe_cfg(), hops).unwrap();
        assert!(alert.is_some(), "breaching dest must yield an alert");
        let breached: bool = conn
            .query_row(
                "SELECT breached FROM probe_run WHERE id=?1",
                [run_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(breached, "probe_run.breached must be set");
        // dest hop stored as breach severity
        let stored = dbprobe::hops_for_run(&conn, run_id).unwrap();
        assert_eq!(stored[1].severity, "breach");
    }

    #[test]
    fn persist_middle_hop_loss_does_not_breach() {
        let (_f, mut conn) = open_temp();
        let t = target("dns", "1.1.1.1", PathType::Underlay);
        let hops = vec![
            hop(1, Some("192.168.1.1"), 0.0, 2.0),
            hop(2, None, 100.0, 0.0),           // dead middle hop
            hop(3, Some("1.1.1.1"), 0.0, 12.0), // healthy dest
        ];
        let (run_id, alert) = persist_and_evaluate(&mut conn, &t, &probe_cfg(), hops).unwrap();
        assert!(alert.is_none(), "middle-hop 100% loss must NOT breach");
        let breached: bool = conn
            .query_row(
                "SELECT breached FROM probe_run WHERE id=?1",
                [run_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!breached);
    }

    // ── retention runs even on a breach-path invocation (R-13) ───────────────

    #[test]
    fn retention_sweeps_old_run_even_when_a_breach_persists() {
        let (_f, mut conn) = open_temp();
        let cfg = probe_cfg();

        // Seed an OLD run (40 days) that retention(30) must remove.
        let old_id = dbprobe::insert_run(
            &mut conn,
            &dbprobe::RunMeta {
                target_name: "old",
                target_addr: "9.9.9.9",
                path_type: PathType::Underlay,
                cycles: 10,
                breached: false,
                ts: Utc::now() - Duration::days(40),
            },
            &[hop(1, Some("9.9.9.9"), 0.0, 5.0)],
        )
        .unwrap();

        // Simulate the command order: retention FIRST...
        let swept = dbprobe::retention_sweep(&mut conn, cfg.retention_days).unwrap();
        assert_eq!(swept, 1, "old run swept at command start");

        // ...then a breaching run persists (the "early-return" path).
        let t = target("dns", "1.1.1.1", PathType::Underlay);
        let hops = vec![hop(1, Some("1.1.1.1"), 90.0, 30.0)];
        let (_id, alert) = persist_and_evaluate(&mut conn, &t, &cfg, hops).unwrap();
        assert!(alert.is_some());

        // The old run is gone regardless of the breach.
        let old_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM probe_run WHERE id=?1",
                [old_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_exists, 0, "old run GC'd even though the run breached");
    }

    // ── path-health carries latest dest-hop summary + severity ───────────────

    #[test]
    fn path_health_uses_latest_run_dest_hop_and_severity() {
        let (_f, mut conn) = open_temp();
        let cfg = probe_cfg();
        let t = target("dns", "1.1.1.1", PathType::Underlay);

        // An older healthy run, then a newer breaching run for the same target.
        let mut older = vec![
            hop(1, Some("192.168.1.1"), 0.0, 2.0),
            hop(2, Some("1.1.1.1"), 0.0, 10.0),
        ];
        probe::apply_severities(&mut older, cfg.loss_threshold_pct, cfg.rtt_threshold_ms);
        dbprobe::insert_run(
            &mut conn,
            &dbprobe::RunMeta {
                target_name: &t.name,
                target_addr: "1.1.1.1",
                path_type: PathType::Underlay,
                cycles: 10,
                breached: false,
                ts: Utc::now() - Duration::hours(2),
            },
            &older,
        )
        .unwrap();

        let newer = vec![
            hop(1, Some("192.168.1.1"), 0.0, 2.0),
            hop(2, None, 100.0, 0.0),            // dead middle
            hop(3, Some("1.1.1.1"), 80.0, 40.0), // breaching dest
        ];
        persist_and_evaluate(&mut conn, &t, &cfg, newer).unwrap();

        let export = build_path_health(&conn).unwrap();
        assert_eq!(export.hops.len(), 1, "one summary per target");
        let s = &export.hops[0];
        assert_eq!(s["target"], "dns");
        assert_eq!(s["dest_host"], "1.1.1.1", "dest is the last RESPONDING hop");
        assert_eq!(s["dest_ttl"], 3);
        assert_eq!(
            s["severity"], "breach",
            "latest run's dest severity surfaced"
        );
        assert_eq!(s["breached"], true);
    }

    #[test]
    fn path_health_all_dead_hops_yields_ok_severity_no_panic() {
        let (_f, mut conn) = open_temp();
        let cfg = probe_cfg();
        let t = target("void", "1.1.1.1", PathType::Underlay);
        let dead = vec![hop(1, None, 100.0, 0.0), hop(2, None, 100.0, 0.0)];
        persist_and_evaluate(&mut conn, &t, &cfg, dead).unwrap();

        let export = build_path_health(&conn).unwrap();
        let s = &export.hops[0];
        assert_eq!(s["dest_host"], serde_json::Value::Null);
        assert_eq!(s["severity"], "ok", "no dest → default ok, no panic");
    }

    #[test]
    fn write_path_health_round_trips_to_disk() {
        let (_f, mut conn) = open_temp();
        let cfg = probe_cfg();
        let t = target("dns", "1.1.1.1", PathType::Underlay);
        persist_and_evaluate(&mut conn, &t, &cfg, vec![hop(1, Some("1.1.1.1"), 0.0, 5.0)]).unwrap();
        let export = build_path_health(&conn).unwrap();

        let tmp = NamedTempFile::new().unwrap();
        write_path_health(&export, tmp.path()).unwrap();
        let back: PathHealthExport =
            serde_json::from_str(&std::fs::read_to_string(tmp.path()).unwrap()).unwrap();
        assert_eq!(back.hops.len(), 1);
    }
}

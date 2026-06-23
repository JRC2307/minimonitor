//! `fleet collect` — resilient hourly pull loop.
//!
//! Pulls `GET /snapshot` from every `tier:agent` node in the DB,
//! persists results, records per-host failures, and NEVER propagates a
//! per-host error to the caller.  The loop is retention-first: old snapshots
//! are pruned before any HTTP is attempted.
//!
//! Spec references: §4.1–§4.5

use std::net::IpAddr;
use std::time::Duration;

use anyhow::Context;
use chrono::Utc;
use futures_util::StreamExt;

use crate::{
    agent_client::AgentClient,
    config::Config,
    db::{self, host as dbhost, nodes},
    model::Tier,
    secrets,
};

/// Run the collect sweep once.
///
/// Steps:
/// 1. Open DB.
/// 2. `retention_sweep` (own txn) before any HTTP.
/// 3. Select `tier:agent` nodes; skip those with no parseable IPv4 address.
/// 4. Resolve bearer token once (optional).
/// 5. Pull snapshots with bounded concurrency.
/// 6. Persist results sequentially: success → `insert_snapshot`, failure → `record_collect_failure`.
/// 7. Return `Ok(())` always (per-host failures are logged, never propagated).
pub async fn run(cfg: &Config, db_path: &str) -> anyhow::Result<()> {
    let cc = &cfg.collect;

    // 1. Open DB.
    let mut conn = db::open(std::path::Path::new(db_path)).context("opening DB for collect")?;

    // 2. Retention sweep FIRST (own txn), before any HTTP.
    let cutoff = retention_cutoff(cc.retention_days);
    let deleted = dbhost::retention_sweep(&mut conn, &cutoff).context("retention_sweep failed")?;
    if deleted > 0 {
        eprintln!(
            "fleet collect: retention_sweep removed {deleted} old snapshot(s) (cutoff {cutoff})"
        );
    }

    // 3. Select tier:agent nodes.
    let filter = nodes::ListFilter {
        tier: Some(Tier::Agent),
        ..Default::default()
    };
    let agent_nodes = nodes::list_filtered(&conn, &filter).context("list_filtered(Agent)")?;

    // Build (node_id, base_url) pairs — skip nodes with no IPv4 address.
    let targets: Vec<(String, String)> = agent_nodes
        .into_iter()
        .filter_map(|n| {
            let ip = n
                .addresses
                .iter()
                .filter_map(|a| a.parse::<IpAddr>().ok())
                .find(|ip| ip.is_ipv4())?;
            let base_url = format!("http://{}:{}", ip, cc.agent_port);
            Some((n.fleet_id, base_url))
        })
        .collect();

    if targets.is_empty() {
        eprintln!("fleet collect: no tier:agent nodes with IPv4 addresses; nothing to do");
        return Ok(());
    }

    // 4. Resolve bearer token once.
    let token: Option<String> = cc
        .token_env
        .as_deref()
        .map(|e| secrets::resolve(e, e))
        .transpose()
        .context("resolving collect token")?;
    let token_ref: Option<&str> = token.as_deref();

    // 5. Bounded-concurrency pulls.
    // Single per-host wall-clock bound via tokio::time::timeout below (spec §4.4).
    // AgentClient carries no reqwest-level timeout — one bound, one error arm.
    let timeout = Duration::from_millis(cc.per_host_timeout_ms);
    let client = AgentClient::new();

    // Collect results: Vec<(node_id, base_url, Result<(raw, snap), anyhow::Error>)>
    let results: Vec<(String, String, anyhow::Result<_>)> = futures_util::stream::iter(targets)
        .map(|(node_id, base_url)| {
            let client = &client;
            async move {
                let fetch_result =
                    tokio::time::timeout(timeout, client.fetch_snapshot(&base_url, token_ref))
                        .await;
                let result = match fetch_result {
                    Ok(inner) => inner,
                    Err(_elapsed) => Err(anyhow::anyhow!(
                        "per-host timeout after {}ms",
                        timeout.as_millis()
                    )),
                };
                (node_id, base_url, result)
            }
        })
        .buffer_unordered(cc.concurrency)
        .collect()
        .await;

    // 6. Persist results sequentially (one DB conn).
    let now = Utc::now().to_rfc3339(); // yields +00:00 — NOT Z
    for (node_id, base_url, result) in results {
        match result {
            Ok((raw, snap)) => {
                if let Err(e) = dbhost::insert_snapshot(&mut conn, &node_id, &raw, &snap, &now)
                    .context("insert_snapshot")
                {
                    let redacted = secrets::redact(e);
                    eprintln!("fleet collect: [{}] persist failed: {redacted:#}", node_id);
                    // Still record as a failure so the status row is honest.
                    let _ = dbhost::record_collect_failure(
                        &conn,
                        &node_id,
                        &now,
                        &format!("{redacted:#}"),
                    );
                } else {
                    eprintln!("fleet collect: [{}] ok ({base_url})", node_id);
                }
            }
            Err(e) => {
                let redacted = secrets::redact(e);
                eprintln!(
                    "fleet collect: [{}] FAILED ({}): {redacted:#}",
                    node_id, base_url
                );
                let _ =
                    dbhost::record_collect_failure(&conn, &node_id, &now, &format!("{redacted:#}"));
            }
        }
    }

    // 7. Always Ok.
    Ok(())
}

/// Compute the retention cutoff timestamp (RFC3339, +00:00 form).
///
/// Anything with `collected_at < cutoff` is eligible for pruning.
fn retention_cutoff(retention_days: u32) -> String {
    let cutoff = Utc::now() - chrono::Duration::days(retention_days as i64);
    cutoff.to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::retention_cutoff;

    #[test]
    fn retention_cutoff_uses_plus00_form() {
        let cutoff = retention_cutoff(14);
        assert!(
            cutoff.ends_with("+00:00"),
            "cutoff must use +00:00 form: {cutoff}"
        );
        assert!(
            !cutoff.ends_with('Z'),
            "cutoff must NOT end with Z: {cutoff}"
        );
    }
}

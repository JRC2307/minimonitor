//! `fleet show <node>` — full detail for one node.
//!
//! Resolves `<node>` by fleet_id → hostname → fqdn.
//! An **ambiguous** hostname (matches >1 row) prints candidates and exits non-zero.

use crate::db::nodes::{ResolveResult, get_by_ref, load_seen_in};
use crate::model::{Node, Tier};
use anyhow::Result;
use std::time::Duration;

/// Show full details for a single node resolved by `target`.
/// Returns `Ok(true)` when the node was found, `Ok(false)` on ambiguous (caller
/// must exit non-zero), and `Err` on DB / not-found errors.
pub fn run(
    conn: &rusqlite::Connection,
    target: &str,
    online_threshold: Duration,
) -> Result<ShowResult> {
    match get_by_ref(conn, target)? {
        ResolveResult::Found(mut node) => {
            // Load seen_in from node_seen table
            node.seen_in = load_seen_in(conn, &node.fleet_id)?;
            let enrollment = load_enrollment(conn, &node.fleet_id)?;
            let probe_summary = load_last_probe(conn, &node.fleet_id)?;
            print_detail(
                &node,
                &enrollment,
                probe_summary.as_deref(),
                online_threshold,
            );
            Ok(ShowResult::Found)
        }
        ResolveResult::Ambiguous(candidates) => {
            eprintln!(
                "fleet show: ambiguous target {:?} — matches multiple nodes:",
                target
            );
            for c in &candidates {
                eprintln!(
                    "  {} (hostname={}, fqdn={})",
                    c.fleet_id, c.hostname, c.fqdn
                );
            }
            Ok(ShowResult::Ambiguous)
        }
        ResolveResult::NotFound => {
            anyhow::bail!("no node found for {:?}", target);
        }
    }
}

pub enum ShowResult {
    Found,
    Ambiguous,
}

fn load_enrollment(conn: &rusqlite::Connection, fleet_id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn
        .prepare("SELECT system, remote_id FROM enrollment WHERE fleet_id = ?1 ORDER BY system")?;
    let rows: Result<Vec<_>, _> = stmt
        .query_map(rusqlite::params![fleet_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect();
    Ok(rows?)
}

fn load_last_probe(conn: &rusqlite::Connection, fleet_id: &str) -> Result<Option<String>> {
    // probe_run uses target_name — match by fleet_id or hostname
    let r = conn.query_row(
        "SELECT id, ts, target_addr, cycles, breached FROM probe_run
         WHERE target_name = ?1 ORDER BY ts DESC LIMIT 1",
        rusqlite::params![fleet_id],
        |row| {
            Ok(format!(
                "run_id={} ts={} addr={} cycles={} breached={}",
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        },
    );
    match r {
        Ok(s) => Ok(Some(s)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn print_detail(
    node: &Node,
    enrollment: &[(String, String)],
    probe_summary: Option<&str>,
    threshold: Duration,
) {
    let online = crate::model::is_online(node.last_seen, threshold);
    println!("fleet_id:        {}", node.fleet_id);
    println!("hostname:        {}", node.hostname);
    println!("fqdn:            {}", node.fqdn);
    println!("os:              {}", node.os);
    println!(
        "tier:            {}",
        match node.tier {
            Tier::Agent => "agent",
            Tier::Agentless => "agentless",
        }
    );
    println!("online:          {}", if online { "●" } else { "○" });
    println!("last_seen:       {}", node.last_seen.to_rfc3339());
    println!("dedupe_key_kind: {:?}", node.dedupe_key_kind);
    println!("addresses:");
    for a in &node.addresses {
        println!("  - {a}");
    }
    println!("tags:");
    println!("  role:  {:?}", node.tags.role);
    println!("  owner: {:?}", node.tags.owner);
    println!("  site:  {:?}", node.tags.site);
    println!("  gpu:   {:?}", node.tags.gpu);
    println!("  raw:   {:?}", node.tags.raw);
    println!("seen_in:");
    if node.seen_in.is_empty() {
        println!("  (none)");
    }
    for s in &node.seen_in {
        println!("  account={} device_id={}", s.account, s.device_id);
    }
    println!("enrollment:");
    if enrollment.is_empty() {
        println!("  (none)");
    }
    for (sys, rid) in enrollment {
        println!("  system={sys} remote_id={rid}");
    }
    println!("last_probe: {}", probe_summary.unwrap_or("(none)"));
    if let Some(n) = &node.notes {
        println!("notes:           {n}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::nodes::{upsert_node, upsert_node_seen};
    use crate::db::open;
    use crate::model::{DedupeKind, Node, Tags, Tier};
    use chrono::Utc;
    use tempfile::NamedTempFile;

    fn base_node(id: &str, hostname: &str, fqdn: &str) -> Node {
        let now = Utc::now();
        Node {
            fleet_id: id.to_owned(),
            hostname: hostname.to_owned(),
            fqdn: fqdn.to_owned(),
            seen_in: vec![],
            addresses: vec!["100.10.20.30".to_owned()],
            os: "linux".to_owned(),
            online: true,
            last_seen: now,
            tags: Tags::default(),
            tier: Tier::Agentless,
            dedupe_key_kind: DedupeKind::Machinekey,
            notes: None,
            first_seen: now,
            updated_at: now,
            fuzzy_hint: None,
        }
    }

    #[test]
    fn resolve_by_fleet_id() {
        let f = NamedTempFile::new().unwrap();
        let conn = open(f.path()).unwrap();
        upsert_node(
            &conn,
            &base_node("fleet-01", "myhost", "myhost.tail.ts.net"),
        )
        .unwrap();

        let result = get_by_ref(&conn, "fleet-01").unwrap();
        assert!(matches!(result, ResolveResult::Found(_)));
    }

    #[test]
    fn resolve_by_hostname() {
        let f = NamedTempFile::new().unwrap();
        let conn = open(f.path()).unwrap();
        upsert_node(
            &conn,
            &base_node("fleet-02", "uniquehost", "uniquehost.ts.net"),
        )
        .unwrap();

        let result = get_by_ref(&conn, "uniquehost").unwrap();
        assert!(matches!(result, ResolveResult::Found(ref n) if n.fleet_id == "fleet-02"));
    }

    #[test]
    fn resolve_by_fqdn() {
        let f = NamedTempFile::new().unwrap();
        let conn = open(f.path()).unwrap();
        upsert_node(
            &conn,
            &base_node("fleet-03", "fqdnhost", "fqdnhost.internal.ts.net"),
        )
        .unwrap();

        let result = get_by_ref(&conn, "fqdnhost.internal.ts.net").unwrap();
        assert!(matches!(result, ResolveResult::Found(ref n) if n.fleet_id == "fleet-03"));
    }

    #[test]
    fn ambiguous_hostname_returns_candidates() {
        let f = NamedTempFile::new().unwrap();
        let conn = open(f.path()).unwrap();
        upsert_node(
            &conn,
            &base_node("fleet-04a", "dupehost", "dupehost.a.ts.net"),
        )
        .unwrap();
        upsert_node(
            &conn,
            &base_node("fleet-04b", "dupehost", "dupehost.b.ts.net"),
        )
        .unwrap();

        let result = get_by_ref(&conn, "dupehost").unwrap();
        match result {
            ResolveResult::Ambiguous(candidates) => {
                assert_eq!(candidates.len(), 2);
                let ids: Vec<_> = candidates.iter().map(|n| n.fleet_id.as_str()).collect();
                assert!(ids.contains(&"fleet-04a"));
                assert!(ids.contains(&"fleet-04b"));
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn seen_in_loaded_from_node_seen() {
        let f = NamedTempFile::new().unwrap();
        let conn = open(f.path()).unwrap();
        upsert_node(&conn, &base_node("fleet-05", "seenhost", "seenhost.ts.net")).unwrap();
        upsert_node_seen(
            &conn, "personal", "dev123", "fleet-05", "mk:abc", None, "t1", 1,
        )
        .unwrap();
        upsert_node_seen(
            &conn, "client-x", "dev456", "fleet-05", "mk:abc", None, "t2", 1,
        )
        .unwrap();

        let seen = load_seen_in(&conn, "fleet-05").unwrap();
        assert_eq!(seen.len(), 2);
        let accounts: Vec<_> = seen.iter().map(|s| s.account.as_str()).collect();
        assert!(accounts.contains(&"personal"));
        assert!(accounts.contains(&"client-x"));
    }

    #[test]
    fn dedupe_key_kind_included_in_show() {
        let f = NamedTempFile::new().unwrap();
        let conn = open(f.path()).unwrap();
        let mut n = base_node("fleet-06", "fuzzyhost", "fuzzyhost.ts.net");
        n.dedupe_key_kind = DedupeKind::Fuzzy;
        upsert_node(&conn, &n).unwrap();

        let result = get_by_ref(&conn, "fleet-06").unwrap();
        match result {
            ResolveResult::Found(node) => {
                assert_eq!(node.dedupe_key_kind, DedupeKind::Fuzzy);
            }
            _ => panic!("expected Found"),
        }
    }

    #[test]
    fn enrollment_status_joined() {
        let f = NamedTempFile::new().unwrap();
        let conn = open(f.path()).unwrap();
        upsert_node(
            &conn,
            &base_node("fleet-07", "enrolledhost", "enrolledhost.ts.net"),
        )
        .unwrap();

        // Insert enrollment record directly
        conn.execute(
            "INSERT INTO enrollment (fleet_id, system, remote_id, last_enrolled) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["fleet-07", "beszel", "bsz-123", "2026-01-01T00:00:00Z"],
        )
        .unwrap();

        let enrollment = load_enrollment(&conn, "fleet-07").unwrap();
        assert_eq!(enrollment.len(), 1);
        assert_eq!(enrollment[0].0, "beszel");
        assert_eq!(enrollment[0].1, "bsz-123");
    }
}

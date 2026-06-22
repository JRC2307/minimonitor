use crate::model::{DedupeKind, Node, Tags, Tier};
use anyhow::Context;
use chrono::Utc;
use rusqlite::{Connection, params};

pub fn upsert_node(conn: &Connection, node: &Node) -> anyhow::Result<()> {
    let addresses_json = serde_json::to_string(&node.addresses).context("serializing addresses")?;
    let raw_tags_json = serde_json::to_string(&node.tags.raw).context("serializing raw_tags")?;

    let tier_str = match node.tier {
        Tier::Agent => "agent",
        Tier::Agentless => "agentless",
    };
    let dedupe_str = match node.dedupe_key_kind {
        DedupeKind::Machinekey => "machinekey",
        DedupeKind::Alias => "alias",
        DedupeKind::Fuzzy => "fuzzy",
    };

    let updated_at = Utc::now().to_rfc3339();

    conn.execute(
        "INSERT INTO node (
            fleet_id, hostname, fqdn, os, addresses, online, last_seen,
            tier, role, owner, site, gpu, raw_tags, dedupe_key_kind,
            notes, first_seen, updated_at
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7,
            ?8, ?9, ?10, ?11, ?12, ?13, ?14,
            ?15, ?16, ?17
        )
        ON CONFLICT(fleet_id) DO UPDATE SET
            hostname        = excluded.hostname,
            fqdn            = excluded.fqdn,
            os              = excluded.os,
            addresses       = excluded.addresses,
            online          = excluded.online,
            last_seen       = excluded.last_seen,
            tier            = excluded.tier,
            role            = excluded.role,
            owner           = excluded.owner,
            site            = excluded.site,
            gpu             = excluded.gpu,
            raw_tags        = excluded.raw_tags,
            dedupe_key_kind = excluded.dedupe_key_kind,
            notes           = excluded.notes,
            first_seen      = first_seen,
            updated_at      = excluded.updated_at,
            stale           = 0",
        params![
            node.fleet_id,
            node.hostname,
            node.fqdn,
            node.os,
            addresses_json,
            node.online as i32,
            node.last_seen.to_rfc3339(),
            tier_str,
            node.tags.role,
            node.tags.owner,
            node.tags.site,
            node.tags.gpu,
            raw_tags_json,
            dedupe_str,
            node.notes,
            node.first_seen.to_rfc3339(),
            updated_at,
        ],
    )
    .context("upsert_node execute")?;

    Ok(())
}

/// Insert or refresh a `node_seen` provenance row, stamping it with the current
/// sync `run_id` (used by the epoch sweep to detect vanished devices).
#[allow(clippy::too_many_arguments)]
pub fn upsert_node_seen(
    conn: &Connection,
    account: &str,
    device_id: &str,
    node_id: &str,
    machine_key: &str,
    fuzzy_hint: Option<&str>,
    last_seen: &str,
    run_id: i64,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO node_seen
            (account, device_id, node_id, machine_key, fuzzy_hint, last_seen, last_confirmed_run)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(account, device_id) DO UPDATE SET
            node_id            = excluded.node_id,
            machine_key        = excluded.machine_key,
            fuzzy_hint         = excluded.fuzzy_hint,
            last_seen          = excluded.last_seen,
            last_confirmed_run = excluded.last_confirmed_run",
        params![
            account,
            device_id,
            node_id,
            machine_key,
            fuzzy_hint.unwrap_or(""),
            last_seen,
            run_id,
        ],
    )
    .context("upsert_node_seen")?;
    Ok(())
}

/// Epoch-scoped sweep: delete `node_seen` rows for `account` whose
/// `last_confirmed_run` is not the current `run_id` — i.e. devices that were
/// present in a prior run but absent from this (successful) one. Scoped to the
/// account so a failed/skipped account never wipes its provenance (additive).
pub fn sweep_epoch(conn: &Connection, account: &str, run_id: i64) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM node_seen WHERE account = ?1 AND last_confirmed_run != ?2",
        params![account, run_id],
    )
    .context("sweep_epoch")?;
    Ok(())
}

/// Mark every node whose provenance has fully vanished (no `node_seen` rows
/// remain) as `stale = 1`, and clear the flag on nodes that still have
/// provenance. Stale nodes are kept (history/enrollment preserved), not deleted.
pub fn mark_stale_nodes(conn: &Connection) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE node SET stale = CASE
            WHEN (SELECT COUNT(*) FROM node_seen WHERE node_seen.node_id = node.fleet_id) = 0
            THEN 1 ELSE 0 END",
        [],
    )
    .context("mark_stale_nodes")?;
    Ok(())
}

/// Read the `stale` flag for a node (test/inspection helper).
pub fn is_stale(conn: &Connection, fleet_id: &str) -> anyhow::Result<Option<bool>> {
    let r = conn.query_row(
        "SELECT stale FROM node WHERE fleet_id = ?1",
        params![fleet_id],
        |row| row.get::<_, i64>(0),
    );
    match r {
        Ok(v) => Ok(Some(v != 0)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e).context("is_stale"),
    }
}

pub fn get(conn: &Connection, fleet_id: &str) -> anyhow::Result<Option<Node>> {
    let result = conn.query_row(
        "SELECT fleet_id, hostname, fqdn, os, addresses, online, last_seen,
                tier, role, owner, site, gpu, raw_tags, dedupe_key_kind,
                notes, first_seen, updated_at
         FROM node WHERE fleet_id = ?1",
        params![fleet_id],
        row_to_node,
    );

    match result {
        Ok(node) => Ok(Some(node)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e).context("get node"),
    }
}

pub fn list(conn: &Connection) -> anyhow::Result<Vec<Node>> {
    let mut stmt = conn.prepare(
        "SELECT fleet_id, hostname, fqdn, os, addresses, online, last_seen,
                tier, role, owner, site, gpu, raw_tags, dedupe_key_kind,
                notes, first_seen, updated_at
         FROM node ORDER BY fleet_id",
    )?;

    let nodes: Result<Vec<Node>, _> = stmt.query_map([], row_to_node)?.collect();

    nodes.context("list nodes")
}

fn row_to_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
    let fleet_id: String = row.get(0)?;
    let hostname: String = row.get(1)?;
    let fqdn: String = row.get(2)?;
    let os: String = row.get(3)?;
    let addresses_json: String = row.get(4)?;
    let online_int: i32 = row.get(5)?;
    let last_seen_str: String = row.get(6)?;
    let tier_str: String = row.get(7)?;
    let role: Option<String> = row.get(8)?;
    let owner: Option<String> = row.get(9)?;
    let site: Option<String> = row.get(10)?;
    let gpu: Option<String> = row.get(11)?;
    let raw_tags_json: String = row.get(12)?;
    let dedupe_str: String = row.get(13)?;
    let notes: Option<String> = row.get(14)?;
    let first_seen_str: String = row.get(15)?;
    let updated_at_str: String = row.get(16)?;

    let addresses: Vec<String> = serde_json::from_str(&addresses_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;

    let raw: Vec<String> = serde_json::from_str(&raw_tags_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(12, rusqlite::types::Type::Text, Box::new(e))
    })?;

    let tier = match tier_str.as_str() {
        "agent" => Tier::Agent,
        _ => Tier::Agentless,
    };

    let dedupe_key_kind = match dedupe_str.as_str() {
        "machinekey" => DedupeKind::Machinekey,
        "alias" => DedupeKind::Alias,
        _ => DedupeKind::Fuzzy,
    };

    let parse_dt = |s: &str| -> rusqlite::Result<chrono::DateTime<chrono::Utc>> {
        s.parse::<chrono::DateTime<chrono::Utc>>().map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
    };

    Ok(Node {
        fleet_id,
        hostname,
        fqdn,
        seen_in: vec![],
        addresses,
        os,
        online: online_int != 0,
        last_seen: parse_dt(&last_seen_str)?,
        tags: Tags {
            role,
            owner,
            site,
            gpu,
            raw,
        },
        tier,
        dedupe_key_kind,
        notes,
        first_seen: parse_dt(&first_seen_str)?,
        updated_at: parse_dt(&updated_at_str)?,
        fuzzy_hint: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open;
    use chrono::Utc;
    use tempfile::NamedTempFile;

    fn make_node(fleet_id: &str) -> Node {
        let now = Utc::now();
        Node {
            fleet_id: fleet_id.to_owned(),
            hostname: "test-host".to_owned(),
            fqdn: "test-host.local".to_owned(),
            seen_in: vec![],
            addresses: vec!["100.1.2.3".to_owned(), "fd7a::1".to_owned()],
            os: "linux".to_owned(),
            online: true,
            last_seen: now,
            tags: Tags {
                role: Some("worker".to_owned()),
                owner: Some("caguabot".to_owned()),
                site: None,
                gpu: None,
                raw: vec!["tag:worker".to_owned(), "tag:prod".to_owned()],
            },
            tier: Tier::Agent,
            dedupe_key_kind: DedupeKind::Machinekey,
            notes: Some("test node".to_owned()),
            first_seen: now,
            updated_at: now,
            fuzzy_hint: None,
        }
    }

    #[test]
    fn upsert_then_get_round_trips() {
        let f = NamedTempFile::new().unwrap();
        let conn = open(f.path()).unwrap();
        let node = make_node("test-01");

        upsert_node(&conn, &node).unwrap();

        let retrieved = get(&conn, "test-01").unwrap().expect("node should exist");

        assert_eq!(retrieved.fleet_id, node.fleet_id);
        assert_eq!(retrieved.hostname, node.hostname);
        assert_eq!(retrieved.addresses, node.addresses);
        assert_eq!(retrieved.tags.raw, node.tags.raw);
        assert_eq!(retrieved.tags.role, node.tags.role);
        assert_eq!(retrieved.tags.owner, node.tags.owner);
        assert_eq!(retrieved.tier, node.tier);
        assert_eq!(retrieved.dedupe_key_kind, node.dedupe_key_kind);
        assert_eq!(retrieved.notes, node.notes);
        assert_eq!(retrieved.online, node.online);
    }

    #[test]
    fn first_seen_not_overwritten_on_second_upsert() {
        let f = NamedTempFile::new().unwrap();
        let conn = open(f.path()).unwrap();
        let node = make_node("test-02");

        upsert_node(&conn, &node).unwrap();

        let first = get(&conn, "test-02").unwrap().unwrap();
        let first_seen_1 = first.first_seen;

        // Small delay to ensure time progresses
        std::thread::sleep(std::time::Duration::from_millis(10));

        let mut node2 = node.clone();
        node2.hostname = "updated-host".to_owned();
        node2.first_seen = Utc::now(); // Would overwrite if bug exists

        upsert_node(&conn, &node2).unwrap();

        let second = get(&conn, "test-02").unwrap().unwrap();

        assert_eq!(
            second.first_seen, first_seen_1,
            "first_seen should not change on re-upsert"
        );
        assert_eq!(
            second.hostname, "updated-host",
            "hostname should be updated"
        );
    }

    #[test]
    fn updated_at_bumps_on_second_upsert() {
        let f = NamedTempFile::new().unwrap();
        let conn = open(f.path()).unwrap();
        let node = make_node("test-03");

        upsert_node(&conn, &node).unwrap();
        let first = get(&conn, "test-03").unwrap().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(10));

        upsert_node(&conn, &node).unwrap();
        let second = get(&conn, "test-03").unwrap().unwrap();

        assert!(
            second.updated_at > first.updated_at,
            "updated_at should advance on re-upsert"
        );
    }

    #[test]
    fn get_returns_none_for_missing() {
        let f = NamedTempFile::new().unwrap();
        let conn = open(f.path()).unwrap();
        let result = get(&conn, "nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn list_returns_all_nodes() {
        let f = NamedTempFile::new().unwrap();
        let conn = open(f.path()).unwrap();

        upsert_node(&conn, &make_node("alpha")).unwrap();
        upsert_node(&conn, &make_node("beta")).unwrap();
        upsert_node(&conn, &make_node("gamma")).unwrap();

        let nodes = list(&conn).unwrap();
        assert_eq!(nodes.len(), 3);
    }

    #[test]
    fn node_seen_upsert_and_epoch_sweep() {
        let f = NamedTempFile::new().unwrap();
        let conn = open(f.path()).unwrap();
        upsert_node(&conn, &make_node("n1")).unwrap();

        // Run 1: device present.
        upsert_node_seen(
            &conn,
            "personal",
            "devX",
            "n1",
            "mk:1",
            Some("fz:x"),
            "t1",
            1,
        )
        .unwrap();
        sweep_epoch(&conn, "personal", 1).unwrap();
        let cnt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM node_seen WHERE account='personal'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt, 1, "device present after run 1");

        // Run 2: personal succeeds but devX is absent (not re-upserted) → swept.
        sweep_epoch(&conn, "personal", 2).unwrap();
        let cnt2: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM node_seen WHERE account='personal'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt2, 0, "absent device swept in run 2");

        mark_stale_nodes(&conn).unwrap();
        assert_eq!(
            is_stale(&conn, "n1").unwrap(),
            Some(true),
            "orphaned node stale"
        );
    }

    #[test]
    fn stale_cleared_when_still_seen() {
        let f = NamedTempFile::new().unwrap();
        let conn = open(f.path()).unwrap();
        upsert_node(&conn, &make_node("n1")).unwrap();
        upsert_node_seen(&conn, "personal", "devX", "n1", "mk:1", None, "t1", 1).unwrap();
        mark_stale_nodes(&conn).unwrap();
        assert_eq!(is_stale(&conn, "n1").unwrap(), Some(false));
    }
}

pub mod cf;
pub mod nodes;

use anyhow::Context;
use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

const M001: &str = "
CREATE TABLE node (
    fleet_id        TEXT PRIMARY KEY,
    hostname        TEXT NOT NULL,
    fqdn            TEXT NOT NULL DEFAULT '',
    os              TEXT NOT NULL DEFAULT '',
    addresses       TEXT NOT NULL DEFAULT '[]',
    online          INTEGER NOT NULL DEFAULT 0,
    last_seen       TEXT NOT NULL,
    tier            TEXT NOT NULL DEFAULT 'agentless',
    role TEXT, owner TEXT, site TEXT, gpu TEXT,
    raw_tags        TEXT NOT NULL DEFAULT '[]',
    dedupe_key_kind TEXT NOT NULL DEFAULT 'fuzzy',
    notes           TEXT,
    first_seen      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
CREATE INDEX idx_node_tier   ON node(tier);
CREATE INDEX idx_node_online ON node(online);

CREATE TABLE node_seen (
    account            TEXT NOT NULL,
    device_id          TEXT NOT NULL,
    node_id            TEXT NOT NULL REFERENCES node(fleet_id) ON DELETE CASCADE,
    node_key           TEXT NOT NULL DEFAULT '',
    machine_key        TEXT NOT NULL DEFAULT '',
    fuzzy_hint         TEXT NOT NULL DEFAULT '',
    last_seen          TEXT NOT NULL,
    last_confirmed_run INTEGER NOT NULL,
    PRIMARY KEY (account, device_id)
);
CREATE INDEX idx_seen_node ON node_seen(node_id);
CREATE INDEX idx_seen_mk   ON node_seen(machine_key);

CREATE TABLE sync_run (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    ts        TEXT NOT NULL,
    accounts_ok TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE enrollment (
    fleet_id      TEXT NOT NULL REFERENCES node(fleet_id) ON DELETE CASCADE,
    system        TEXT NOT NULL,
    remote_id     TEXT NOT NULL,
    last_enrolled TEXT NOT NULL,
    PRIMARY KEY (fleet_id, system)
);

CREATE TABLE probe_run (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          TEXT NOT NULL,
    target_name TEXT NOT NULL,
    target_addr TEXT NOT NULL,
    path_type   TEXT NOT NULL DEFAULT 'underlay',
    cycles      INTEGER NOT NULL,
    breached    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_run_target ON probe_run(target_name, ts);

CREATE TABLE probe_hop (
    run_id   INTEGER NOT NULL REFERENCES probe_run(id) ON DELETE CASCADE,
    ttl      INTEGER NOT NULL,
    host     TEXT,
    sent     INTEGER NOT NULL,
    recv     INTEGER NOT NULL,
    loss_pct REAL NOT NULL,
    last_ms  REAL, avg_ms REAL, best_ms REAL, wrst_ms REAL, stdev_ms REAL,
    severity TEXT NOT NULL DEFAULT 'ok',
    PRIMARY KEY (run_id, ttl)
);

CREATE TABLE cf_zone (
    zone_id         TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    status          TEXT NOT NULL,
    paused          INTEGER NOT NULL DEFAULT 0,
    healthy         INTEGER NOT NULL DEFAULT 0,
    min_cert_expiry TEXT,
    synced_at       TEXT NOT NULL
);
";

/// M002: add a `stale` flag to `node`. A node goes stale when all of its
/// `node_seen` provenance rows have been swept (the box vanished from every
/// tailnet) — it is kept, not deleted, so its history/enrollments survive.
const M002: &str = "ALTER TABLE node ADD COLUMN stale INTEGER NOT NULL DEFAULT 0;";

pub fn open(path: &std::path::Path) -> anyhow::Result<Connection> {
    let mut conn =
        Connection::open(path).with_context(|| format!("opening sqlite at {}", path.display()))?;

    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
        .context("setting PRAGMAs")?;

    let migrations = Migrations::new(vec![M::up(M001), M::up(M002)]);
    migrations
        .to_latest(&mut conn)
        .context("running migrations")?;

    Ok(conn)
}

/// Insert a new sync run row (with the current timestamp) and return its id.
pub fn insert_sync_run(conn: &Connection) -> anyhow::Result<i64> {
    conn.execute(
        "INSERT INTO sync_run (ts) VALUES (?1)",
        [chrono::Utc::now().to_rfc3339()],
    )
    .context("insert sync_run")?;
    Ok(conn.last_insert_rowid())
}

/// Record which accounts succeeded for a sync run (JSON array in `accounts_ok`).
pub fn update_sync_run_accounts(
    conn: &Connection,
    run_id: i64,
    accounts: &[String],
) -> anyhow::Result<()> {
    let json = serde_json::to_string(accounts).context("serializing accounts_ok")?;
    conn.execute(
        "UPDATE sync_run SET accounts_ok = ?1 WHERE id = ?2",
        rusqlite::params![json, run_id],
    )
    .context("update_sync_run_accounts")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn open_temp() -> (NamedTempFile, Connection) {
        let f = NamedTempFile::new().unwrap();
        let conn = open(f.path()).unwrap();
        (f, conn)
    }

    #[test]
    fn migration_applies_m001_all_tables() {
        let (_f, conn) = open_temp();

        // Check user_version is 1 (migration ran)
        let ver: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 2, "user_version should be 2 after M001+M002");

        // Check all 7 tables exist
        let expected = [
            "node",
            "node_seen",
            "sync_run",
            "enrollment",
            "probe_run",
            "probe_hop",
            "cf_zone",
        ];
        for table in &expected {
            let count: i32 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "table {table} should exist");
        }
    }

    #[test]
    fn foreign_keys_are_on() {
        let (_f, conn) = open_temp();
        let fk: i32 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1, "foreign_keys should be ON");
    }

    #[test]
    fn cascade_delete_node_removes_dependents() {
        let (_f, conn) = open_temp();

        let now = "2026-01-01T00:00:00Z";
        // Insert a node
        conn.execute(
            "INSERT INTO node (fleet_id, hostname, last_seen, first_seen, updated_at)
             VALUES ('test-01', 'test-host', ?1, ?1, ?1)",
            [now],
        )
        .unwrap();

        // Insert a node_seen referencing it
        conn.execute(
            "INSERT INTO node_seen (account, device_id, node_id, last_seen, last_confirmed_run)
             VALUES ('acct1', 'dev1', 'test-01', ?1, 0)",
            [now],
        )
        .unwrap();

        // Insert an enrollment referencing it
        conn.execute(
            "INSERT INTO enrollment (fleet_id, system, remote_id, last_enrolled)
             VALUES ('test-01', 'sys1', 'r1', ?1)",
            [now],
        )
        .unwrap();

        // Now delete the node
        conn.execute("DELETE FROM node WHERE fleet_id='test-01'", [])
            .unwrap();

        // node_seen should be gone
        let ns_count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM node_seen WHERE node_id='test-01'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ns_count, 0, "node_seen should cascade delete");

        // enrollment should be gone
        let enr_count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM enrollment WHERE fleet_id='test-01'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(enr_count, 0, "enrollment should cascade delete");
    }

    #[test]
    fn sync_run_insert_and_accounts() {
        let (_f, conn) = open_temp();
        let run_id = insert_sync_run(&conn).unwrap();
        assert!(run_id >= 1);
        update_sync_run_accounts(
            &conn,
            run_id,
            &["personal".to_owned(), "client-acme".to_owned()],
        )
        .unwrap();
        let json: String = conn
            .query_row(
                "SELECT accounts_ok FROM sync_run WHERE id=?1",
                [run_id],
                |r| r.get(0),
            )
            .unwrap();
        let accounts: Vec<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(accounts, vec!["personal", "client-acme"]);
    }
}

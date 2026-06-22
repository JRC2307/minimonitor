//! `fleet serve` — read-only HTTP server exposing `/api/*` JSON endpoints.
//!
//! ## Design
//! - Opens SQLite **read-only** per request (`SQLITE_OPEN_READ_ONLY` + `PRAGMA query_only=ON`).
//! - Uses the `export::build_*` builders (spec §3.8) as the single source of truth for
//!   JSON shapes — same code path as `fleet list --json`.
//! - Never binds a real socket in tests; callers use `tower::ServiceExt::oneshot`.

use std::path::{Path, PathBuf};

use anyhow::Context;
use axum::{Router, routing::get};
use rusqlite::{Connection, OpenFlags};

use crate::config::ServeConfig;

pub mod routes;

/// Open a SQLite connection in strict read-only mode.
///
/// WAL reader semantics: a concurrent RW writer can commit without blocking reads.
pub fn open_readonly(db_path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open_readonly: {}", db_path.display()))?;

    conn.execute_batch("PRAGMA query_only=ON;")
        .context("open_readonly: PRAGMA query_only")?;

    Ok(conn)
}

/// Build the Axum router with `db_path` embedded in shared state.
///
/// Kept separate from `run` so tests can call `oneshot` without binding a port.
pub fn build_router(db_path: PathBuf) -> Router {
    let state = routes::AppState { db_path };

    Router::new()
        .route("/api/fleet", get(routes::get_fleet))
        .route("/api/node/{id}", get(routes::get_node))
        .route("/api/path-health", get(routes::get_path_health))
        .route("/api/cf", get(routes::get_cf))
        .with_state(state)
}

/// Bind and serve on `cfg.bind` (tailnet IP resolved externally; tests never
/// call this).
pub async fn run(cfg: &ServeConfig, db_path: &Path) -> anyhow::Result<()> {
    let addr: std::net::SocketAddr = cfg
        .bind
        .parse()
        .with_context(|| format!("fleet serve: invalid bind address {:?}", cfg.bind))?;

    let router = build_router(db_path.to_path_buf());

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("fleet serve: bind {addr}"))?;

    eprintln!("fleet serve: listening on {addr}");

    axum::serve(listener, router)
        .await
        .context("fleet serve: axum::serve error")?;

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::db::nodes::upsert_node;
    use crate::export::{FleetExport, NodeExport};
    use crate::model::{DedupeKind, Node, Tags, TailnetRef, Tier};
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use chrono::Utc;
    use tempfile::NamedTempFile;
    use tower::ServiceExt; // oneshot

    fn make_node(fleet_id: &str, hostname: &str) -> Node {
        let now = Utc::now();
        Node {
            fleet_id: fleet_id.to_owned(),
            hostname: hostname.to_owned(),
            fqdn: format!("{hostname}.ts.net"),
            seen_in: vec![TailnetRef {
                account: "personal".to_owned(),
                device_id: "1".to_owned(),
            }],
            addresses: vec!["100.64.0.1".to_owned()],
            os: "linux".to_owned(),
            online: true,
            last_seen: now,
            tags: Tags {
                role: Some("worker".to_owned()),
                owner: Some("self".to_owned()),
                site: None,
                gpu: None,
                raw: vec!["tag:worker".to_owned()],
            },
            tier: Tier::Agent,
            dedupe_key_kind: DedupeKind::Machinekey,
            notes: None,
            first_seen: now,
            updated_at: now,
            fuzzy_hint: None,
        }
    }

    fn seed_db(nodes: &[Node]) -> NamedTempFile {
        let f = NamedTempFile::new().unwrap();
        let conn = db::open(f.path()).unwrap();
        for n in nodes {
            upsert_node(&conn, n).unwrap();
        }
        f
    }

    async fn oneshot_get(router: Router, uri: &str) -> (StatusCode, Vec<u8>) {
        let req = Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, body.to_vec())
    }

    // ── api_fleet_returns_seeded_nodes ─────────────────────────────────────────

    #[tokio::test]
    async fn api_fleet_returns_seeded_nodes() {
        let n1 = make_node("fleet-01", "alpha");
        let n2 = make_node("fleet-02", "beta");
        let f = seed_db(&[n1.clone(), n2.clone()]);

        let router = build_router(f.path().to_path_buf());
        let (status, body) = oneshot_get(router, "/api/fleet").await;

        assert_eq!(
            status,
            StatusCode::OK,
            "body: {}",
            String::from_utf8_lossy(&body)
        );

        let export: FleetExport = serde_json::from_slice(&body).expect("valid FleetExport JSON");

        assert_eq!(export.nodes.len(), 2, "should have 2 nodes");

        // Both hostnames must appear
        let hostnames: Vec<&str> = export.nodes.iter().map(|n| n.hostname.as_str()).collect();
        assert!(hostnames.contains(&"alpha"), "alpha missing: {hostnames:?}");
        assert!(hostnames.contains(&"beta"), "beta missing: {hostnames:?}");

        // online is derived from DB (both inserted with online=true → 1)
        for node_export in &export.nodes {
            assert_eq!(
                node_export.online, 1,
                "{} should be online",
                node_export.hostname
            );
        }
    }

    // ── api_node_detail ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn api_node_detail_found() {
        let n = make_node("fleet-01", "alpha");
        let f = seed_db(std::slice::from_ref(&n));

        let router = build_router(f.path().to_path_buf());
        let (status, body) = oneshot_get(router, "/api/node/fleet-01").await;

        assert_eq!(
            status,
            StatusCode::OK,
            "body: {}",
            String::from_utf8_lossy(&body)
        );

        let export: NodeExport = serde_json::from_slice(&body).expect("valid NodeExport JSON");
        assert_eq!(export.id, "fleet-01");
        assert_eq!(export.hostname, "alpha");
    }

    #[tokio::test]
    async fn api_node_detail_not_found() {
        let f = seed_db(&[]);
        let router = build_router(f.path().to_path_buf());
        let (status, _body) = oneshot_get(router, "/api/node/no-such-node").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // ── api_path_health ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn api_path_health_empty_db() {
        let f = seed_db(&[]);
        let router = build_router(f.path().to_path_buf());
        let (status, body) = oneshot_get(router, "/api/path-health").await;

        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["hops"].is_array(), "hops should be an array");
        assert_eq!(
            v["hops"].as_array().unwrap().len(),
            0,
            "hops should be empty"
        );
    }

    // ── api_cf ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn api_cf_empty_db() {
        let f = seed_db(&[]);
        let router = build_router(f.path().to_path_buf());
        let (status, body) = oneshot_get(router, "/api/cf").await;

        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["zones"].is_array(), "zones should be an array");
        assert_eq!(
            v["zones"].as_array().unwrap().len(),
            0,
            "zones should be empty"
        );
    }

    // ── read-only: write through serve handle errors ───────────────────────────

    #[test]
    fn readonly_connection_rejects_writes() {
        let f = NamedTempFile::new().unwrap();
        // First open RW to run migrations
        let _rw = db::open(f.path()).unwrap();

        // Open the RO handle
        let ro = open_readonly(f.path()).unwrap();

        // Attempt an INSERT — must fail
        let result = ro.execute(
            "INSERT INTO node (fleet_id, hostname, last_seen, first_seen, updated_at)
             VALUES ('x', 'x', 'now', 'now', 'now')",
            [],
        );
        assert!(result.is_err(), "write through RO handle should fail");
    }

    // ── WAL concurrency: RW writer doesn't block RO reader ───────────────────

    #[test]
    fn wal_concurrent_rw_does_not_block_ro_read() {
        let f = NamedTempFile::new().unwrap();
        // Set up schema + seed one node via RW conn
        let rw = db::open(f.path()).unwrap();
        let node = make_node("fleet-wal", "wal-host");
        upsert_node(&rw, &node).unwrap();

        // Open a second RW connection and begin (but don't commit) a write txn
        let rw2 = Connection::open(f.path()).unwrap();
        rw2.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        rw2.execute_batch("BEGIN DEFERRED").unwrap();
        rw2.execute(
            "UPDATE node SET hostname='changing' WHERE fleet_id='fleet-wal'",
            [],
        )
        .unwrap();

        // RO read must succeed even with the uncommitted RW txn
        let ro = open_readonly(f.path()).unwrap();
        let nodes = crate::db::nodes::list(&ro).unwrap();
        assert_eq!(nodes.len(), 1, "RO read should see committed row");
        assert_eq!(nodes[0].hostname, "wal-host", "RO sees pre-commit snapshot");

        // Rollback the uncommitted write
        rw2.execute_batch("ROLLBACK").unwrap();
    }

    // ── schema-lock: /api/fleet body key-path golden contract ────────────────

    #[tokio::test]
    async fn schema_lock_api_fleet_key_paths() {
        let n = make_node("fleet-schema", "schema-host");
        let f = seed_db(&[n]);

        let router = build_router(f.path().to_path_buf());
        let (status, body) = oneshot_get(router, "/api/fleet").await;
        assert_eq!(status, StatusCode::OK);

        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // Top-level keys
        assert!(v.get("generated_at").is_some(), "missing key: generated_at");
        assert!(v.get("nodes").is_some(), "missing key: nodes");

        // Node-level keys
        let node = &v["nodes"][0];
        for key in &["id", "hostname", "tier", "online", "last_seen"] {
            assert!(
                node.get(*key).is_some(),
                "missing node key: {key} — field rename breaks the golden contract"
            );
        }

        // Check value types
        assert!(v["nodes"].is_array());
        assert!(node["online"].is_number(), "online must be numeric");
        assert!(node["tier"].is_string(), "tier must be a string");
    }
}

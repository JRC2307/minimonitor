//! `fleet serve` — read-only HTTP server exposing `/api/*` JSON endpoints.
//!
//! ## Design
//! - Opens SQLite **read-only** per request (`SQLITE_OPEN_READ_ONLY` + `PRAGMA query_only=ON`).
//! - Uses the `export::build_*` builders (spec §3.8) as the single source of truth for
//!   JSON shapes — same code path as `fleet list --json`.
//! - Never binds a real socket in tests; callers use `tower::ServiceExt::oneshot`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use axum::{Router, routing::get};
use rusqlite::{Connection, OpenFlags};
use tower_http::services::ServeDir;

use crate::config::ServeConfig;

pub mod routes;
pub mod templates;

/// Default freshness window for the derived `online` field (spec §3.3), used by
/// the simpler [`build_router`] entrypoint (e.g. Task-16 tests).
const DEFAULT_ONLINE_THRESHOLD: Duration = Duration::from_secs(900);

/// Default snapshot staleness threshold (3 h), used by the simpler [`build_router`]
/// entrypoint and the `full_router` test helper (spec §6.5).
const DEFAULT_SNAPSHOT_STALE_THRESHOLD: Duration = Duration::from_secs(10_800);

/// Filesystem dir holding the vendored static assets (htmx + css), served at
/// `/static/` via `tower_http::services::ServeDir`. Checked into the repo —
/// no CDN, no npm (spec §3.8).
fn assets_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets")
}

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

/// Build the Axum router with `db_path` embedded in shared state, using default
/// UI links and the default online threshold. Convenience entrypoint (Task-16
/// `/api/*` tests call this).
pub fn build_router(db_path: PathBuf) -> Router {
    build_router_with(routes::AppState {
        db_path,
        online_threshold: DEFAULT_ONLINE_THRESHOLD,
        snapshot_stale_threshold: DEFAULT_SNAPSHOT_STALE_THRESHOLD,
        beszel_ui_url: String::new(),
        kuma_ui_url: String::new(),
        labels: std::sync::Arc::new(crate::service_label::Labels::empty()),
        store: std::sync::Arc::new(crate::store::Catalog::builtin()),
    })
}

/// `GET /manifest.webmanifest` — PWA manifest, embedded at compile time so the
/// route works regardless of the on-disk assets dir. Must be same-origin with
/// scope `/` for Android "Add to Home Screen".
async fn get_manifest() -> impl axum::response::IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/manifest+json",
        )],
        include_str!("../../assets/manifest.webmanifest"),
    )
}

/// `GET /sw.js` — service worker. MUST be served from the origin root: a SW's
/// maximum scope is its script's directory, and the launcher scope is `/`.
async fn get_sw() -> impl axum::response::IntoResponse {
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/javascript"),
            // Let the browser recheck the SW promptly on deploys.
            (axum::http::header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../../assets/sw.js"),
    )
}

/// Build the full Axum router from an explicit [`routes::AppState`]: the four
/// HTML pages (spec §3.8), the four `/api/*` JSON endpoints (Task 16), and the
/// vendored static assets at `/static/` (`tower_http::services::ServeDir`).
pub fn build_router_with(state: routes::AppState) -> Router {
    Router::new()
        // caguastore launcher — the landing page; the monitor lives one level down
        .route("/", get(routes::get_store))
        .route("/store", get(routes::get_store))
        .route("/manifest.webmanifest", get(get_manifest))
        .route("/sw.js", get(get_sw))
        // HTML views (askama, server-rendered)
        .route("/inventory", get(routes::get_index))
        .route("/node/{id}", get(routes::get_node_html))
        .route("/paths", get(routes::get_paths_html))
        .route("/ports", get(routes::get_ports_html))
        .route("/workloads", get(routes::get_workloads_html))
        .route("/observability", get(routes::get_observability_html))
        // JSON API (Task 16)
        .route("/api/fleet", get(routes::get_fleet))
        .route("/api/node/{id}", get(routes::get_node))
        .route("/api/path-health", get(routes::get_path_health))
        .route("/api/cf", get(routes::get_cf))
        // JSON API (C9 — host snapshots)
        .route("/api/ports", get(routes::get_api_ports))
        .route("/api/workloads", get(routes::get_api_workloads))
        // Vendored CSS + HTMX (no CDN)
        .nest_service("/static", ServeDir::new(assets_dir()))
        .with_state(state)
}

/// Bind and serve on `cfg.bind` (tailnet IP resolved externally; tests never
/// call this). Uses the default online and snapshot-stale thresholds.
pub async fn run(cfg: &ServeConfig, db_path: &Path) -> anyhow::Result<()> {
    run_with(
        cfg,
        db_path,
        DEFAULT_ONLINE_THRESHOLD,
        DEFAULT_SNAPSHOT_STALE_THRESHOLD,
        crate::service_label::Labels::empty(),
        crate::store::Catalog::builtin(),
    )
    .await
}

/// Bind and serve with caller-supplied thresholds (wired from
/// `Config::online_threshold_secs` and `Config::snapshot_stale_secs`).
pub async fn run_with(
    cfg: &ServeConfig,
    db_path: &Path,
    online_threshold: Duration,
    snapshot_stale_threshold: Duration,
    labels: crate::service_label::Labels,
    store: crate::store::Catalog,
) -> anyhow::Result<()> {
    let addr: std::net::SocketAddr = cfg
        .bind
        .parse()
        .with_context(|| format!("fleet serve: invalid bind address {:?}", cfg.bind))?;

    let router = build_router_with(routes::AppState {
        db_path: db_path.to_path_buf(),
        online_threshold,
        snapshot_stale_threshold,
        beszel_ui_url: cfg.beszel_ui_url.clone(),
        kuma_ui_url: cfg.kuma_ui_url.clone(),
        labels: std::sync::Arc::new(labels),
        store: std::sync::Arc::new(store),
    });

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

    #[tokio::test]
    async fn api_cf_seeded() {
        use crate::cloudflare::CfZone;
        use crate::db::cf::upsert_cf_zone;

        let f = seed_db(&[]);
        {
            let conn = db::open(f.path()).unwrap();
            upsert_cf_zone(
                &conn,
                &CfZone {
                    id: "z-seed-1".to_owned(),
                    name: "seeded-zone.io".to_owned(),
                    status: "active".to_owned(),
                    paused: false,
                    healthy: true,
                    min_cert_expiry: None,
                },
            )
            .unwrap();
        }

        let router = build_router(f.path().to_path_buf());
        let (status, body) = oneshot_get(router, "/api/cf").await;

        assert_eq!(
            status,
            StatusCode::OK,
            "body: {}",
            String::from_utf8_lossy(&body)
        );
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let zones = v["zones"].as_array().expect("zones is an array");
        assert_eq!(zones.len(), 1, "expected one seeded zone, got {zones:?}");
        assert_eq!(
            zones[0]["name"].as_str(),
            Some("seeded-zone.io"),
            "zone name mismatch"
        );
        assert_eq!(
            zones[0]["id"].as_str(),
            Some("z-seed-1"),
            "zone id mismatch"
        );
        assert_eq!(
            zones[0]["status"].as_str(),
            Some("active"),
            "zone status mismatch"
        );
    }

    #[tokio::test]
    async fn api_path_health_seeded() {
        use crate::db::probe::{RunMeta, insert_run};
        use crate::probe::{HopStat, PathType, Severity};

        let f = seed_db(&[]);
        {
            let mut conn = db::open(f.path()).unwrap();
            let hops = vec![HopStat {
                ttl: 1,
                host: Some("10.0.0.1".to_owned()),
                sent: 5,
                recv: 5,
                loss_pct: 0.0,
                last_ms: 3.0,
                avg_ms: 3.0,
                best_ms: 2.5,
                worst_ms: 3.5,
                stddev_ms: 0.2,
                severity: Severity::Ok,
            }];
            insert_run(
                &mut conn,
                &RunMeta {
                    target_name: "test-target",
                    target_addr: "10.0.0.1",
                    path_type: PathType::Underlay,
                    cycles: 5,
                    breached: false,
                    ts: Utc::now(),
                },
                &hops,
            )
            .unwrap();
        }

        let router = build_router(f.path().to_path_buf());
        let (status, body) = oneshot_get(router, "/api/path-health").await;

        assert_eq!(
            status,
            StatusCode::OK,
            "body: {}",
            String::from_utf8_lossy(&body)
        );
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let hops = v["hops"].as_array().expect("hops is an array");
        assert_eq!(hops.len(), 1, "expected one seeded path, got {hops:?}");
        assert_eq!(
            hops[0]["target_name"].as_str(),
            Some("test-target"),
            "target_name mismatch"
        );
        assert_eq!(
            hops[0]["target_addr"].as_str(),
            Some("10.0.0.1"),
            "target_addr mismatch"
        );
        assert_eq!(
            hops[0]["dest_severity"].as_str(),
            Some("ok"),
            "dest_severity mismatch"
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

    // ── schema-lock: /api/ports ───────────────────────────────────────────────

    #[tokio::test]
    async fn schema_lock_api_ports_key_paths() {
        let node = make_node("fleet-ports-schema", "ports-schema-host");
        let f = seed_db(&[node]);
        let fresh_ts = (Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        {
            let conn = db::open(f.path()).unwrap();
            conn.execute(
                "INSERT INTO host_snapshot
                    (node_id, collected_at, hostname, total_cpu_percent,
                     used_memory_bytes, total_memory_bytes, workload_count, port_count, snapshot_json)
                 VALUES ('fleet-ports-schema', ?1, 'ports-schema-host', 0.0, 0, 0, 0, 1, '{}')",
                rusqlite::params![fresh_ts],
            ).unwrap();
            let sid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO host_port (snapshot_id, node_id, port, proto, process, pid, bind)
                 VALUES (?1, 'fleet-ports-schema', 9090, 'TCP', 'testd', 100, '127.0.0.1')",
                rusqlite::params![sid],
            )
            .unwrap();
        }

        let router = build_router(f.path().to_path_buf());
        let (status, body) = oneshot_get(router, "/api/ports").await;
        assert_eq!(
            status,
            StatusCode::OK,
            "body: {}",
            String::from_utf8_lossy(&body)
        );

        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.get("generated_at").is_some(), "missing key: generated_at");
        assert!(v.get("rows").is_some(), "missing key: rows");

        let row = &v["rows"][0];
        for key in &[
            "hostname",
            "fleet_id",
            "port",
            "proto",
            "process",
            "pid",
            "bind",
            "collected_at",
            "stale",
        ] {
            assert!(
                row.get(*key).is_some(),
                "missing port row key: {key} — field rename breaks golden contract"
            );
        }
        assert!(
            row["port"].is_number(),
            "port must be a JSON number, got: {}",
            row["port"]
        );
        assert!(
            row["stale"].is_boolean(),
            "stale must be a JSON boolean, got: {}",
            row["stale"]
        );
        assert_eq!(row["port"].as_u64(), Some(9090), "port value must be 9090");
        assert_eq!(
            row["stale"].as_bool(),
            Some(false),
            "fresh port must not be stale"
        );
    }

    // ── schema-lock: /api/workloads ───────────────────────────────────────────

    #[tokio::test]
    async fn schema_lock_api_workloads_key_paths() {
        let node = make_node("fleet-wl-schema", "wl-schema-host");
        let f = seed_db(&[node]);
        let fresh_ts = (Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        {
            let conn = db::open(f.path()).unwrap();
            conn.execute(
                "INSERT INTO host_snapshot
                    (node_id, collected_at, hostname, total_cpu_percent,
                     used_memory_bytes, total_memory_bytes, workload_count, port_count, snapshot_json)
                 VALUES ('fleet-wl-schema', ?1, 'wl-schema-host', 0.0, 0, 0, 1, 0, '{}')",
                rusqlite::params![fresh_ts],
            ).unwrap();
            let sid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO host_workload
                    (snapshot_id, node_id, label, category, process_count,
                     total_cpu_percent, total_memory_bytes, example_command)
                 VALUES (?1, 'fleet-wl-schema', 'ollama', 'llm', 1, 5.0, 1000000000, '/usr/bin/ollama serve')",
                rusqlite::params![sid],
            ).unwrap();
        }

        let router = build_router(f.path().to_path_buf());
        let (status, body) = oneshot_get(router, "/api/workloads").await;
        assert_eq!(
            status,
            StatusCode::OK,
            "body: {}",
            String::from_utf8_lossy(&body)
        );

        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.get("generated_at").is_some(), "missing key: generated_at");
        assert!(v.get("rows").is_some(), "missing key: rows");

        let row = &v["rows"][0];
        for key in &[
            "hostname",
            "fleet_id",
            "label",
            "category",
            "process_count",
            "total_cpu_percent",
            "total_memory_bytes",
            "example_command",
            "collected_at",
            "stale",
        ] {
            assert!(
                row.get(*key).is_some(),
                "missing workload row key: {key} — field rename breaks golden contract"
            );
        }
        assert!(
            row["stale"].is_boolean(),
            "stale must be a JSON boolean, got: {}",
            row["stale"]
        );
        assert_eq!(
            row["stale"].as_bool(),
            Some(false),
            "fresh workload must not be stale"
        );
    }

    // ── /api/ports empty DB returns empty rows ────────────────────────────────

    #[tokio::test]
    async fn api_ports_empty_db_returns_empty_rows() {
        let f = seed_db(&[]);
        let router = build_router(f.path().to_path_buf());
        let (status, body) = oneshot_get(router, "/api/ports").await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["rows"].is_array());
        assert_eq!(v["rows"].as_array().unwrap().len(), 0);
    }

    // ── /api/workloads empty DB returns empty rows ────────────────────────────

    #[tokio::test]
    async fn api_workloads_empty_db_returns_empty_rows() {
        let f = seed_db(&[]);
        let router = build_router(f.path().to_path_buf());
        let (status, body) = oneshot_get(router, "/api/workloads").await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["rows"].is_array());
        assert_eq!(v["rows"].as_array().unwrap().len(), 0);
    }

    // ════════════════════════════════════════════════════════════════════════
    //  HTML views (askama) + vendored assets — Task 17 (spec §3.8)
    // ════════════════════════════════════════════════════════════════════════

    use crate::db::nodes::upsert_node_seen;
    use crate::model::DedupeKind as Dk;

    /// Build the full router (HTML + assets) against a seeded DB path, with
    /// concrete Beszel/Kuma UI links and a generous online threshold so
    /// just-seeded nodes read as online.
    fn full_router(db_path: PathBuf) -> Router {
        build_router_with(routes::AppState {
            db_path,
            online_threshold: Duration::from_secs(900),
            snapshot_stale_threshold: DEFAULT_SNAPSHOT_STALE_THRESHOLD,
            beszel_ui_url: "http://intel-mini:8090".to_owned(),
            kuma_ui_url: "http://intel-mini:3001".to_owned(),
            labels: std::sync::Arc::new(crate::service_label::Labels::empty()),
            store: std::sync::Arc::new(crate::store::Catalog::builtin()),
        })
    }

    async fn html_get(router: Router, uri: &str) -> (StatusCode, String) {
        let (status, body) = oneshot_get(router, uri).await;
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    // ── index_renders_inventory ──────────────────────────────────────────────

    #[tokio::test]
    async fn index_renders_inventory() {
        // alpha: machinekey (no ~), online. zeta: fuzzy (~), offline (stale).
        let mut alpha = make_node("fleet-01", "alpha");
        alpha.last_seen = Utc::now();
        let mut zeta = make_node("fleet-02", "zeta");
        zeta.dedupe_key_kind = Dk::Fuzzy;
        zeta.last_seen = Utc::now() - chrono::Duration::hours(2); // offline by threshold

        let f = seed_db(&[alpha, zeta]);
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/inventory").await;

        assert_eq!(status, StatusCode::OK, "body: {html}");
        // Both hostnames present.
        assert!(html.contains("alpha"), "alpha missing:\n{html}");
        assert!(html.contains("zeta"), "zeta missing:\n{html}");
        // Online ● and offline ○ glyphs from DERIVED online.
        assert!(html.contains('\u{25cf}'), "online glyph ● missing");
        assert!(html.contains('\u{25cb}'), "offline glyph ○ missing");
        // The fuzzy row carries a ~ marker.
        assert!(html.contains('~'), "fuzzy ~ marker missing:\n{html}");
    }

    #[tokio::test]
    async fn index_partial_returns_table_only() {
        let f = seed_db(&[make_node("fleet-01", "alpha")]);
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/inventory?partial=1").await;
        assert_eq!(status, StatusCode::OK);
        // Fragment: a <table>, but NOT the full document shell.
        assert!(html.contains("<table"), "partial should contain the table");
        assert!(
            !html.contains("<html"),
            "partial must be a fragment, not the full page:\n{html}"
        );
    }

    // ── caguastore launcher (`/`) ────────────────────────────────────────────

    #[tokio::test]
    async fn store_is_the_landing_page() {
        let f = seed_db(&[]);
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/").await;
        assert_eq!(status, StatusCode::OK, "body: {html}");
        assert!(html.contains("caguastore"), "brand missing:\n{html}");
        // Built-in catalog tiles render.
        assert!(html.contains("cuentas"), "cuentas tile missing:\n{html}");
        assert!(html.contains("poker"), "poker tile missing:\n{html}");
        // The system dock links back into the monitor views.
        assert!(html.contains("/inventory"), "dock inventory link missing");
        assert!(html.contains("/observability"), "dock obs link missing");
    }

    #[tokio::test]
    async fn store_alias_route_serves_launcher() {
        let f = seed_db(&[]);
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/store").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("caguastore"));
    }

    #[tokio::test]
    async fn store_led_up_when_port_fresh() {
        // Seed a fresh snapshot exposing cuentas' port (8789) → its tile is "up".
        // NB: seed_host_snapshot writes node_id into the snapshot hostname column,
        // and the builtin catalog matches host "caguaserver" — so the id must match.
        let node = make_node("caguaserver", "caguaserver");
        let f = seed_db(&[node]);
        seed_host_snapshot(
            f.path(),
            "caguaserver",
            &Utc::now().to_rfc3339(),
            0,
            vec![(8789, "TCP", "python3", 42, "0.0.0.0")],
            vec![],
        );
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/").await;
        assert_eq!(status, StatusCode::OK, "body: {html}");
        assert!(
            html.contains(r#"data-slug="cuentas""#),
            "cuentas tile missing:\n{html}"
        );
        // The cuentas anchor carries the `up` class; apps with no fresh port are `down`.
        let cuentas_tile = html
            .split("<a class=\"")
            .find(|chunk| chunk.contains(r#"data-slug="cuentas""#))
            .expect("cuentas tile chunk");
        let class_end = cuentas_tile.find('"').unwrap();
        assert!(
            cuentas_tile[..class_end].contains("up"),
            "cuentas should be up, classes: {}",
            &cuentas_tile[..class_end]
        );
        assert!(html.contains("1/"), "up-count rollup missing:\n{html}");
    }

    #[tokio::test]
    async fn store_port_on_wrong_host_reads_down() {
        // Port 8789 fresh on a NON-caguaserver node must not light cuentas
        // (builtin catalog pins host = "caguaserver").
        let node = make_node("fleet-mac", "js-mac-mini");
        let f = seed_db(&[node]);
        seed_host_snapshot(
            f.path(),
            "fleet-mac",
            &Utc::now().to_rfc3339(),
            0,
            vec![(8789, "TCP", "python3", 42, "0.0.0.0")],
            vec![],
        );
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/").await;
        assert_eq!(status, StatusCode::OK);
        let cuentas_tile = html
            .split("<a class=\"")
            .find(|chunk| chunk.contains(r#"data-slug="cuentas""#))
            .expect("cuentas tile chunk");
        let class_end = cuentas_tile.find('"').unwrap();
        assert!(
            cuentas_tile[..class_end].contains("down"),
            "port on another host must not light the tile, classes: {}",
            &cuentas_tile[..class_end]
        );
    }

    #[tokio::test]
    async fn store_stale_port_reads_down() {
        let node = make_node("fleet-store2", "caguaserver");
        let f = seed_db(&[node]);
        let old_ts = (Utc::now() - chrono::Duration::hours(4)).to_rfc3339();
        seed_host_snapshot(
            f.path(),
            "fleet-store2",
            &old_ts,
            0,
            vec![(8789, "TCP", "python3", 42, "0.0.0.0")],
            vec![],
        );
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/").await;
        assert_eq!(status, StatusCode::OK);
        let cuentas_tile = html
            .split("<a class=\"")
            .find(|chunk| chunk.contains(r#"data-slug="cuentas""#))
            .expect("cuentas tile chunk");
        let class_end = cuentas_tile.find('"').unwrap();
        assert!(
            cuentas_tile[..class_end].contains("down"),
            "stale port must read down, classes: {}",
            &cuentas_tile[..class_end]
        );
    }

    // ── PWA plumbing ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn manifest_served_with_correct_type() {
        let f = seed_db(&[]);
        let router = full_router(f.path().to_path_buf());
        let req = Request::builder()
            .uri("/manifest.webmanifest")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers()[axum::http::header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(ct.contains("manifest"), "wrong content-type: {ct}");
        let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).expect("manifest is valid JSON");
        assert_eq!(v["scope"], "/", "scope must be / for the launcher PWA");
        assert_eq!(v["display"], "standalone");
    }

    #[tokio::test]
    async fn service_worker_served_from_root() {
        let f = seed_db(&[]);
        let router = full_router(f.path().to_path_buf());
        let (status, body) = oneshot_get(router, "/sw.js").await;
        assert_eq!(status, StatusCode::OK);
        let txt = String::from_utf8_lossy(&body);
        assert!(txt.contains("caches"), "sw.js should use CacheStorage");
    }

    #[tokio::test]
    async fn store_css_asset_served() {
        let f = seed_db(&[]);
        let router = full_router(f.path().to_path_buf());
        let (status, body) = oneshot_get(router, "/static/store.css").await;
        assert_eq!(status, StatusCode::OK, "store.css should be served");
        assert!(!body.is_empty());
    }

    // ── node_page_renders_detail ─────────────────────────────────────────────

    #[tokio::test]
    async fn node_page_renders_detail() {
        let n = make_node("fleet-01", "alpha");
        let f = seed_db(std::slice::from_ref(&n));
        // Persist two seen_in provenance pairs (get() does not populate seen_in).
        {
            let conn = db::open(f.path()).unwrap();
            upsert_node_seen(
                &conn, "personal", "dev-aaa", "fleet-01", "mk:1", None, "t", 1,
            )
            .unwrap();
            upsert_node_seen(
                &conn,
                "client-acme",
                "dev-bbb",
                "fleet-01",
                "mk:1",
                None,
                "t",
                1,
            )
            .unwrap();
        }

        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/node/fleet-01").await;

        assert_eq!(status, StatusCode::OK, "body: {html}");
        // Every seen_in pair appears.
        assert!(html.contains("personal"), "account personal missing");
        assert!(html.contains("dev-aaa"), "device dev-aaa missing");
        assert!(html.contains("client-acme"), "account client-acme missing");
        assert!(html.contains("dev-bbb"), "device dev-bbb missing");
        // dedupe_key_kind is surfaced.
        assert!(
            html.contains("machinekey"),
            "dedupe_key_kind missing:\n{html}"
        );
    }

    #[tokio::test]
    async fn node_page_404_for_missing() {
        let f = seed_db(&[]);
        let router = full_router(f.path().to_path_buf());
        let (status, _html) = html_get(router, "/node/no-such").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // ── paths_page ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn paths_page_renders_dest_hop_severities() {
        use crate::db::probe::{RunMeta, insert_run};
        use crate::probe::{HopStat, PathType, Severity};

        let f = seed_db(&[]);
        {
            let mut conn = db::open(f.path()).unwrap();
            let hops = vec![
                HopStat {
                    ttl: 1,
                    host: Some("192.168.1.1".to_owned()),
                    sent: 10,
                    recv: 10,
                    loss_pct: 0.0,
                    last_ms: 2.0,
                    avg_ms: 2.0,
                    best_ms: 1.0,
                    worst_ms: 3.0,
                    stddev_ms: 0.5,
                    severity: Severity::Ok,
                },
                // destination hop: breach severity, 30% loss.
                HopStat {
                    ttl: 2,
                    host: Some("1.1.1.1".to_owned()),
                    sent: 10,
                    recv: 7,
                    loss_pct: 30.0,
                    last_ms: 410.0,
                    avg_ms: 410.0,
                    best_ms: 400.0,
                    worst_ms: 420.0,
                    stddev_ms: 5.0,
                    severity: Severity::Breach,
                },
            ];
            insert_run(
                &mut conn,
                &RunMeta {
                    target_name: "cloudflare-dns",
                    target_addr: "1.1.1.1",
                    path_type: PathType::Underlay,
                    cycles: 10,
                    breached: true,
                    ts: Utc::now(),
                },
                &hops,
            )
            .unwrap();
        }

        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/paths").await;

        assert_eq!(status, StatusCode::OK, "body: {html}");
        assert!(html.contains("cloudflare-dns"), "target name missing");
        // Destination hop severity surfaced.
        assert!(html.contains("breach"), "dest severity missing:\n{html}");
        // Destination hop address surfaced (the dest, not the intermediate).
        assert!(html.contains("1.1.1.1"), "dest hop addr missing");
    }

    // ── observability_page ───────────────────────────────────────────────────

    #[tokio::test]
    async fn observability_page_renders_zones_links_and_rollup() {
        use crate::cloudflare::CfZone;
        use crate::db::cf::upsert_cf_zone;
        use chrono::{TimeZone, Utc as ChronoUtc};

        // 2 nodes: one online, one offline → rollup 1/1.
        let mut online = make_node("fleet-01", "alpha");
        online.last_seen = Utc::now();
        let mut offline = make_node("fleet-02", "beta");
        offline.last_seen = Utc::now() - chrono::Duration::hours(3);
        let f = seed_db(&[online, offline]);
        {
            let conn = db::open(f.path()).unwrap();
            upsert_cf_zone(
                &conn,
                &CfZone {
                    id: "z1".to_owned(),
                    name: "example.com".to_owned(),
                    status: "active".to_owned(),
                    paused: false,
                    healthy: true,
                    min_cert_expiry: Some(
                        ChronoUtc.with_ymd_and_hms(2026, 9, 20, 0, 0, 0).unwrap(),
                    ),
                },
            )
            .unwrap();
        }

        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/observability").await;

        assert_eq!(status, StatusCode::OK, "body: {html}");
        // CF zone rendered.
        assert!(html.contains("example.com"), "cf zone missing:\n{html}");
        // Links OUT to Beszel :8090 and Kuma :3001 (R-10: links only).
        assert!(
            html.contains("http://intel-mini:8090"),
            "Beszel deep-link missing"
        );
        assert!(
            html.contains("http://intel-mini:3001"),
            "Kuma deep-link missing"
        );
        // Registry-derived online rollup (1 online of 2) — use exact template wording.
        assert!(
            html.contains("1 online"),
            "rollup online count missing (expected '1 online'):\n{html}"
        );
        assert!(
            html.contains("1 offline"),
            "rollup offline count missing (expected '1 offline'):\n{html}"
        );
        assert!(
            html.contains("of 2 nodes"),
            "rollup total missing (expected 'of 2 nodes'):\n{html}"
        );
    }

    /// R-10: `/observability` must link out only — NEVER embed a Kuma
    /// socket.io call. Assert no `socket.io`/`io(`/`emit(` client glue is present.
    #[tokio::test]
    async fn observability_page_does_not_embed_kuma_socketio() {
        let f = seed_db(&[]);
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/observability").await;
        assert_eq!(status, StatusCode::OK);

        let lower = html.to_lowercase();
        assert!(
            !lower.contains("socket.io"),
            "R-10 violated: socket.io reference embedded:\n{html}"
        );
        assert!(
            !lower.contains("monitorlist"),
            "R-10 violated: Kuma monitorList referenced:\n{html}"
        );
        // No socket.io client bootstrap in any quoting style, engine.io, or monitorList.
        assert!(
            !lower.contains("io(\""),
            "R-10 violated: socket.io client constructed (double-quote):\n{html}"
        );
        assert!(
            !lower.contains("io('"),
            "R-10 violated: socket.io client constructed (single-quote):\n{html}"
        );
        assert!(
            !lower.contains("engine.io"),
            "R-10 violated: engine.io reference embedded:\n{html}"
        );
    }

    // ── vendored assets (tower-http ServeDir, no CDN) ────────────────────────

    #[tokio::test]
    async fn vendored_htmx_asset_served() {
        let f = seed_db(&[]);
        let router = full_router(f.path().to_path_buf());
        let (status, body) = oneshot_get(router, "/static/htmx.min.js").await;
        assert_eq!(status, StatusCode::OK, "htmx.min.js should be served");
        assert!(!body.is_empty(), "htmx.min.js should be non-empty");
        // Sanity: it really is the htmx library, not an error page.
        let txt = String::from_utf8_lossy(&body);
        assert!(txt.contains("htmx"), "served file is not htmx");
        // Byte-floor: the real htmx.min.js is ~48 KB; anything smaller is truncated/wrong.
        assert!(
            body.len() > 40_000,
            "htmx.min.js too small ({} bytes) — may be wrong file",
            body.len()
        );
    }

    #[tokio::test]
    async fn vendored_css_asset_served() {
        let f = seed_db(&[]);
        let router = full_router(f.path().to_path_buf());
        let (status, body) = oneshot_get(router, "/static/app.css").await;
        assert_eq!(status, StatusCode::OK, "app.css should be served");
        assert!(!body.is_empty(), "app.css should be non-empty");
    }

    // ════════════════════════════════════════════════════════════════════════
    //  Host snapshot pages — Task C8 (spec §6)
    // ════════════════════════════════════════════════════════════════════════

    /// Seed a host snapshot for tests. `collected_at` is an RFC3339 string.
    /// ports: Vec<(port, proto, process, pid, bind)>
    /// workloads: Vec<(label, category, process_count, cpu_pct, mem_bytes, example_cmd)>
    fn seed_host_snapshot(
        db_path: &std::path::Path,
        node_id: &str,
        collected_at: &str,
        workload_count: i64,
        ports: Vec<(u16, &str, &str, i64, &str)>,
        workloads: Vec<(&str, &str, i64, f64, i64, &str)>,
    ) {
        let conn = db::open(db_path).unwrap();
        conn.execute(
            "INSERT INTO host_snapshot
                (node_id, collected_at, hostname, total_cpu_percent, used_memory_bytes,
                 total_memory_bytes, workload_count, port_count, snapshot_json)
             VALUES (?1, ?2, ?3, 55.5, 2147483648, 8589934592, ?4, ?5, '{}')",
            rusqlite::params![
                node_id,
                collected_at,
                node_id,
                workload_count,
                ports.len() as i64
            ],
        )
        .unwrap();
        let sid = conn.last_insert_rowid();
        for (port, proto, process, pid, bind) in ports {
            conn.execute(
                "INSERT INTO host_port (snapshot_id, node_id, port, proto, process, pid, bind)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![sid, node_id, port as i64, proto, process, pid, bind],
            )
            .unwrap();
        }
        for (label, category, process_count, cpu_pct, mem_bytes, example_cmd) in workloads {
            conn.execute(
                "INSERT INTO host_workload
                    (snapshot_id, node_id, label, category, process_count, total_cpu_percent,
                     total_memory_bytes, example_command)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    sid,
                    node_id,
                    label,
                    category,
                    process_count,
                    cpu_pct,
                    mem_bytes,
                    example_cmd
                ],
            )
            .unwrap();
        }
    }

    // ── /ports tests ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn ports_empty_db_returns_200_with_empty_state() {
        let f = seed_db(&[]);
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/ports").await;
        assert_eq!(status, StatusCode::OK, "body: {html}");
        assert!(
            html.contains("class=\"empty\""),
            "empty-state class missing:\n{html}"
        );
    }

    #[tokio::test]
    async fn ports_seeded_returns_200_with_rows() {
        let node = make_node("fleet-p1", "port-host");
        let f = seed_db(&[node]);
        seed_host_snapshot(
            f.path(),
            "fleet-p1",
            &Utc::now().to_rfc3339(),
            0,
            vec![(8080, "TCP", "nginx", 1234, "0.0.0.0")],
            vec![],
        );
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/ports").await;
        assert_eq!(status, StatusCode::OK, "body: {html}");
        assert!(html.contains("8080"), "port 8080 missing:\n{html}");
        assert!(html.contains("TCP"), "proto TCP missing:\n{html}");
    }

    #[tokio::test]
    async fn ports_stale_class_present() {
        let node = make_node("fleet-p2", "stale-host");
        let f = seed_db(&[node]);
        let old_ts = (Utc::now() - chrono::Duration::hours(4)).to_rfc3339();
        seed_host_snapshot(
            f.path(),
            "fleet-p2",
            &old_ts,
            0,
            vec![(9090, "TCP", "myapp", 5678, "127.0.0.1")],
            vec![],
        );
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/ports").await;
        assert_eq!(status, StatusCode::OK, "body: {html}");
        assert!(
            html.contains("stale"),
            "stale class missing in port row:\n{html}"
        );
    }

    #[tokio::test]
    async fn ports_fresh_no_stale_class() {
        let node = make_node("fleet-p3", "fresh-host");
        let f = seed_db(&[node]);
        let fresh_ts = (Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        seed_host_snapshot(
            f.path(),
            "fleet-p3",
            &fresh_ts,
            0,
            vec![(3000, "TCP", "app", 999, "0.0.0.0")],
            vec![],
        );
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/ports").await;
        assert_eq!(status, StatusCode::OK, "body: {html}");
        // The tbody row should NOT have class="stale"; the word "stale" might appear
        // in the nav/CSS but the row should not.
        assert!(
            !html.contains("class=\"stale\""),
            "fresh row should not have stale class:\n{html}"
        );
    }

    #[tokio::test]
    async fn ports_shows_resolved_service_name() {
        let node = make_node("fleet-svc", "svc-host");
        let f = seed_db(&[node]);
        let fresh_ts = (Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        {
            let conn = db::open(f.path()).unwrap();
            let blob = r#"{"processes":[{"pid":4242,"command":"/Users/x/Desktop/1/projects/experiments/cuentas/.venv/bin/python app"}]}"#;
            conn.execute(
                "INSERT INTO host_snapshot
                    (node_id, collected_at, hostname, total_cpu_percent, used_memory_bytes,
                     total_memory_bytes, workload_count, port_count, snapshot_json)
                 VALUES ('fleet-svc', ?1, 'svc-host', 0.0, 0, 0, 0, 1, ?2)",
                rusqlite::params![fresh_ts, blob],
            )
            .unwrap();
            let sid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO host_port (snapshot_id, node_id, port, proto, process, pid, bind)
                 VALUES (?1, 'fleet-svc', 8789, 'TCP', 'python3.1', 4242, '0.0.0.0')",
                rusqlite::params![sid],
            )
            .unwrap();
        }
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/ports").await;
        assert_eq!(status, StatusCode::OK, "body: {html}");
        assert!(
            html.contains("cuentas"),
            "resolved service name missing:\n{html}"
        );
        // Raw process is still shown as ground truth.
        assert!(
            html.contains("python3.1"),
            "raw process should still render:\n{html}"
        );
    }

    // ── /workloads tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn workloads_empty_db_returns_200_with_empty_state() {
        let f = seed_db(&[]);
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/workloads").await;
        assert_eq!(status, StatusCode::OK, "body: {html}");
        assert!(
            html.contains("class=\"empty\""),
            "empty-state class missing:\n{html}"
        );
    }

    #[tokio::test]
    async fn workloads_seeded_returns_200_with_rows() {
        let node = make_node("fleet-w1", "wl-host");
        let f = seed_db(&[node]);
        seed_host_snapshot(
            f.path(),
            "fleet-w1",
            &Utc::now().to_rfc3339(),
            1,
            vec![],
            vec![(
                "llama.cpp",
                "inference",
                2,
                42.5,
                4_000_000_000,
                "llama-run model.gguf",
            )],
        );
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/workloads").await;
        assert_eq!(status, StatusCode::OK, "body: {html}");
        assert!(
            html.contains("llama.cpp"),
            "workload label missing:\n{html}"
        );
    }

    #[tokio::test]
    async fn workloads_top6_note_when_workload_count_exceeds_rows() {
        let node = make_node("fleet-w2", "top6-host");
        let f = seed_db(&[node]);
        // workload_count=10 but only 1 workload row inserted → "showing top 1 of 10"
        seed_host_snapshot(
            f.path(),
            "fleet-w2",
            &Utc::now().to_rfc3339(),
            10,
            vec![],
            vec![(
                "stable-diffusion",
                "image-gen",
                3,
                80.0,
                8_000_000_000,
                "sd_xl_turbo",
            )],
        );
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/workloads").await;
        assert_eq!(status, StatusCode::OK, "body: {html}");
        assert!(
            html.contains("showing top") || html.contains("of 10"),
            "top-N note missing when workload_count exceeds rendered rows:\n{html}"
        );
    }

    #[tokio::test]
    async fn workloads_stale_class_present() {
        let node = make_node("fleet-w3", "stale-wl-host");
        let f = seed_db(&[node]);
        let old_ts = (Utc::now() - chrono::Duration::hours(4)).to_rfc3339();
        seed_host_snapshot(
            f.path(),
            "fleet-w3",
            &old_ts,
            1,
            vec![],
            vec![("ollama", "llm", 1, 10.0, 2_000_000_000, "ollama run llama3")],
        );
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/workloads").await;
        assert_eq!(status, StatusCode::OK, "body: {html}");
        assert!(
            html.contains("stale"),
            "stale class missing in workload row:\n{html}"
        );
    }

    // ── /node/{id} host snapshot tests ───────────────────────────────────────

    #[tokio::test]
    async fn node_with_snapshot_shows_host_section() {
        let node = make_node("fleet-n1", "snap-host");
        let f = seed_db(&[node]);
        seed_host_snapshot(
            f.path(),
            "fleet-n1",
            &Utc::now().to_rfc3339(),
            1,
            vec![(443, "TCP", "nginx", 100, "0.0.0.0")],
            vec![(
                "vllm",
                "inference",
                1,
                75.0,
                6_000_000_000,
                "vllm serve llama3",
            )],
        );
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/node/fleet-n1").await;
        assert_eq!(status, StatusCode::OK, "body: {html}");
        // cpu and mem should be formatted and present
        assert!(
            html.contains("55.5%"),
            "cpu percent missing in host section:\n{html}"
        );
        // Memory: 2147483648 bytes = 2.0 GB
        assert!(
            html.contains("2.0 GB"),
            "used memory missing in host section:\n{html}"
        );
    }

    #[tokio::test]
    async fn node_without_snapshot_shows_200_not_500() {
        let node = make_node("fleet-n2", "no-snap-host");
        let f = seed_db(&[node]);
        // No snapshot seeded
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/node/fleet-n2").await;
        assert_eq!(status, StatusCode::OK, "body: {html}");
        assert!(
            html.contains("No host snapshot"),
            "missing 'No host snapshot' message:\n{html}"
        );
    }

    #[tokio::test]
    async fn node_host_ports_show_resolved_service() {
        let node = make_node("fleet-ns", "ns-host");
        let f = seed_db(&[node]);
        {
            let conn = db::open(f.path()).unwrap();
            // Command derives "cuentas" via tier-2 (projects/<type>/<name>); the raw
            // process is "python3.1". "cuentas" can ONLY come from the resolver — so
            // asserting it proves resolution happened, not a fallback to raw process.
            let blob = r#"{"processes":[{"pid":7777,"command":"/Users/x/Desktop/1/projects/experiments/cuentas/.venv/bin/python app"}]}"#;
            conn.execute(
                "INSERT INTO host_snapshot
                    (node_id, collected_at, hostname, total_cpu_percent, used_memory_bytes,
                     total_memory_bytes, workload_count, port_count, snapshot_json)
                 VALUES ('fleet-ns', ?1, 'ns-host', 0.0, 0, 0, 0, 1, ?2)",
                rusqlite::params![Utc::now().to_rfc3339(), blob],
            )
            .unwrap();
            let sid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO host_port (snapshot_id, node_id, port, proto, process, pid, bind)
                 VALUES (?1, 'fleet-ns', 8789, 'TCP', 'python3.1', 7777, '0.0.0.0')",
                rusqlite::params![sid],
            )
            .unwrap();
        }
        let router = full_router(f.path().to_path_buf());
        let (status, html) = html_get(router, "/node/fleet-ns").await;
        assert_eq!(status, StatusCode::OK, "body: {html}");
        // Resolved name from the resolver (not derivable any other way).
        assert!(
            html.contains("cuentas"),
            "resolved service name missing:\n{html}"
        );
        // Raw process still shown as ground truth.
        assert!(
            html.contains("python3.1"),
            "raw process should still render:\n{html}"
        );
    }
}

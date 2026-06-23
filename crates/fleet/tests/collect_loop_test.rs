// C6 TDD — fleet collect pull loop integration test
//
// Two tests:
//
// 1. collect_loop_resilient (good path + child-row assertions)
//    - good_server: serves a valid snapshot at GET /snapshot (200)
//    - node-good: 127.0.0.1 → good_server → host_snapshot + child rows written
//    - node-bad: 127.0.0.2 → connection refused → failure row written (no snapshot)
//    - node-agentless: skipped entirely
//    - Asserts: run() Ok(()), child row counts match fixture, agentless untouched
//
// 2. collect_loop_http500_failure (Fix 1 — exercises the HTTP-500 path)
//    - bad_server: returns 500 for every GET /snapshot
//    - node-500: 127.0.0.1 → bad_server (agent_port = bad_server_port) → 500
//    - Asserts: run() Ok(()), failure row has last_error set + last_success_at NULL,
//      no host_snapshot row written

use fleet::{commands::collect, config::Config, db};
use tempfile::NamedTempFile;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

// Load the existing fixture used by other tests.
// Fixture stats (for assertion constants below):
//   ports array length:        37
//   ai_snapshot.workload_count: 3  (== top_workloads length)
const SNAPSHOT_FIXTURE: &str = include_str!("fixtures/snapshot.json");
const FIXTURE_PORT_COUNT: i64 = 37;
const FIXTURE_WORKLOAD_COUNT: i64 = 3;

// ─── Helper: seed a node directly via SQL ────────────────────────────────────

fn seed_node(conn: &rusqlite::Connection, fleet_id: &str, addresses_json: &str, tier: &str) {
    let now = "2026-01-01T00:00:00+00:00";
    conn.execute(
        "INSERT INTO node (fleet_id, hostname, fqdn, os, addresses, online, last_seen,
                           tier, raw_tags, dedupe_key_kind, first_seen, updated_at)
         VALUES (?1, ?1, '', '', ?2, 1, ?3, ?4, '[]', 'fuzzy', ?3, ?3)",
        rusqlite::params![fleet_id, addresses_json, now, tier],
    )
    .unwrap();
}

// ─── Helper: build a minimal Config pointing at a temp DB ────────────────────

fn make_config(agent_port: u16) -> Config {
    // Parse a full default config using a minimal TOML string.
    use figment::{
        Figment,
        providers::{Format, Toml},
    };

    let toml_str = format!(
        r#"
db_path = "/tmp/fleet-test-unused.db"
export_yaml_path = "/tmp/fleet-test.yaml"

[collect]
agent_port = {agent_port}
concurrency = 4
per_host_timeout_ms = 5000
retention_days = 14
"#
    );
    Figment::new()
        .merge(Toml::string(&toml_str))
        .extract()
        .expect("config parse")
}

// ─── Helper: parse the port from a wiremock URI "http://127.0.0.1:PORT" ──────

fn port_from_uri(uri: &str) -> u16 {
    uri.trim_start_matches("http://")
        .split(':')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

// ─── Test 1: good path + connection-refused failure + child-row assertions ───

#[tokio::test]
async fn collect_loop_resilient() {
    // 1. Stand up the good server.
    let good_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/snapshot"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(SNAPSHOT_FIXTURE)
                .append_header("content-type", "application/json"),
        )
        .mount(&good_server)
        .await;

    let good_port = port_from_uri(&good_server.uri());

    // 2. Create temp DB and seed nodes.
    let tmp = NamedTempFile::new().unwrap();
    let db_path = tmp.path().to_str().unwrap().to_owned();

    {
        let conn = db::open(tmp.path()).unwrap();
        // node-good: 127.0.0.1 on good_port → good_server → 200
        // node-bad:  127.0.0.2 on good_port → nothing listening → connection refused
        // node-agentless: skipped (tier != agent)
        seed_node(&conn, "node-good", r#"["127.0.0.1"]"#, "agent");
        seed_node(&conn, "node-bad", r#"["127.0.0.2"]"#, "agent");
        seed_node(&conn, "node-agentless", r#"["127.0.0.3"]"#, "agentless");
    }

    // 3. Build config with agent_port = good_port.
    let cfg = make_config(good_port);

    // 4. Run collect — must return Ok(()) despite node-bad being unreachable.
    let result = collect::run(&cfg, &db_path).await;
    assert!(result.is_ok(), "collect::run must return Ok: {result:?}");

    // 5. Verify results.
    let conn = db::open(tmp.path()).unwrap();

    // node-good: must have exactly 1 host_snapshot row.
    let snap_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM host_snapshot WHERE node_id='node-good'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(snap_count, 1, "node-good should have 1 host_snapshot row");

    let snap_id: i64 = conn
        .query_row(
            "SELECT id FROM host_snapshot WHERE node_id='node-good'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // Fix 2: assert child rows match fixture constants (37 ports, 3 workloads).
    let port_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM host_port WHERE snapshot_id=?1",
            [snap_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        port_count, FIXTURE_PORT_COUNT,
        "node-good: host_port rows must equal fixture ports length ({FIXTURE_PORT_COUNT})"
    );

    let workload_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM host_workload WHERE snapshot_id=?1",
            [snap_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        workload_count, FIXTURE_WORKLOAD_COUNT,
        "node-good: host_workload rows must equal fixture top_workloads count ({FIXTURE_WORKLOAD_COUNT})"
    );

    // node-good: must have last_success_at set.
    let last_success: Option<String> = conn
        .query_row(
            "SELECT last_success_at FROM host_collect_status WHERE node_id='node-good'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        last_success.is_some(),
        "node-good: last_success_at should be set"
    );
    assert!(
        !last_success.as_deref().unwrap_or("").is_empty(),
        "node-good: last_success_at should be non-empty"
    );

    // node-bad: must NOT have a host_snapshot row.
    let bad_snap_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM host_snapshot WHERE node_id='node-bad'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        bad_snap_count, 0,
        "node-bad should have NO host_snapshot row"
    );

    // node-bad: must have a host_collect_status row with last_error set AND last_success_at NULL.
    let (last_error, last_success_bad): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT last_error, last_success_at FROM host_collect_status WHERE node_id='node-bad'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(
        last_error.is_some(),
        "node-bad: last_error should be set after failure"
    );
    assert!(
        last_success_bad.is_none(),
        "node-bad: last_success_at should be NULL (first-ever failure)"
    );

    // node-agentless: must have NO host_collect_status row (never contacted).
    let agentless_status_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM host_collect_status WHERE node_id='node-agentless'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        agentless_status_count, 0,
        "node-agentless: must not have any host_collect_status row"
    );

    // 6. Confirm collected_at format: should use +00:00 not Z.
    let collected_at: String = conn
        .query_row(
            "SELECT collected_at FROM host_snapshot WHERE node_id='node-good'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        collected_at.ends_with("+00:00"),
        "collected_at must use +00:00 format (not Z): got {collected_at}"
    );
    assert!(
        !collected_at.ends_with('Z'),
        "collected_at must NOT end with Z: got {collected_at}"
    );

    drop(good_server);
}

// ─── Test 2 (Fix 1): HTTP-500 server → record_collect_failure path ────────────
//
// Uses bad_server's actual port as agent_port so node-500 (127.0.0.1) is routed
// to bad_server and receives an HTTP 500.  Verifies the failure row is written with
// last_error set and last_success_at NULL, and no host_snapshot row is created.

#[tokio::test]
async fn collect_loop_http500_failure() {
    // Stand up the bad server (500 for everything).
    let bad_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/snapshot"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&bad_server)
        .await;

    // Use bad_server's actual port as agent_port so 127.0.0.1:{bad_port} → bad_server.
    let bad_port = port_from_uri(&bad_server.uri());

    let tmp = NamedTempFile::new().unwrap();
    let db_path = tmp.path().to_str().unwrap().to_owned();

    {
        let conn = db::open(tmp.path()).unwrap();
        seed_node(&conn, "node-500", r#"["127.0.0.1"]"#, "agent");
    }

    let cfg = make_config(bad_port);

    // run() must return Ok(()) even though the only node returns 500.
    let result = collect::run(&cfg, &db_path).await;
    assert!(
        result.is_ok(),
        "collect::run must return Ok even on HTTP 500: {result:?}"
    );

    let conn = db::open(tmp.path()).unwrap();

    // No host_snapshot row (HTTP 500 means no valid snapshot).
    let snap_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM host_snapshot WHERE node_id='node-500'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(snap_count, 0, "node-500: no host_snapshot row on HTTP 500");

    // Failure row must exist with last_error set and last_success_at NULL.
    let (last_error, last_success_at): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT last_error, last_success_at FROM host_collect_status WHERE node_id='node-500'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(
        last_error.is_some(),
        "node-500: last_error must be set after HTTP 500"
    );
    assert!(
        last_error.as_deref().unwrap_or("").contains("500"),
        "node-500: last_error should mention HTTP 500, got: {:?}",
        last_error
    );
    assert!(
        last_success_at.is_none(),
        "node-500: last_success_at must be NULL (first-ever failure)"
    );

    drop(bad_server);
}

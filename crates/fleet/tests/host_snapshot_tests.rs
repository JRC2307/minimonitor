// C3 TDD — host-snapshot storage tests
// All tests use a temp-file SQLite opened via db::open (foreign_keys=ON via PRAGMA).
// Fixtures from tests/fixtures/snapshot.json (already in the tree from C1).

use fleet::db;
use fleet::db::host as dbhost;
use minimonitor_core::{
    ai::{AiSnapshot, AiWorkload},
    net::{NetIdentity, PortRow},
    snapshot::{MonitorSnapshot, ProcessRow, SortMode},
};
use tempfile::NamedTempFile;

// ─── helpers ─────────────────────────────────────────────────────────────────

fn open_temp() -> (NamedTempFile, rusqlite::Connection) {
    let f = NamedTempFile::new().unwrap();
    let conn = db::open(f.path()).unwrap();
    (f, conn)
}

fn insert_test_node(conn: &rusqlite::Connection, fleet_id: &str) {
    let now = "2026-01-01T00:00:00+00:00";
    conn.execute(
        "INSERT INTO node (fleet_id, hostname, last_seen, first_seen, updated_at)
         VALUES (?1, ?1, ?2, ?2, ?2)",
        rusqlite::params![fleet_id, now],
    )
    .unwrap();
}

fn make_minimal_snapshot(cpu: f32, gpu: Option<f32>) -> MonitorSnapshot {
    MonitorSnapshot {
        total_memory_bytes: 16_000_000_000,
        used_memory_bytes: 8_000_000_000,
        total_swap_bytes: 0,
        used_swap_bytes: 0,
        total_cpu_percent: cpu,
        cores: vec![],
        load_average: (0.1, 0.2, 0.3),
        gpu_percent: gpu,
        net_rx_bps: 0,
        net_tx_bps: 0,
        disk_read_bps: 0,
        disk_write_bps: 0,
        ports: vec![
            PortRow {
                port: 8080,
                proto: "TCP".into(),
                process: "myapp".into(),
                pid: 1234,
                bind: "127.0.0.1".into(),
            },
            PortRow {
                port: 443,
                proto: "TCP".into(),
                process: "nginx".into(),
                pid: 5678,
                bind: "*".into(),
            },
        ],
        connections: vec![],
        identity: NetIdentity {
            hostname: "test-host".into(),
            lan_ip: None,
            tailnet_ip: None,
        },
        ai_snapshot: AiSnapshot {
            workload_count: 2,
            total_cpu_percent: 50.0,
            total_memory_bytes: 1_000_000_000,
            top_workloads: vec![
                AiWorkload {
                    label: "Ollama".into(),
                    category: "model-runtime".into(),
                    process_count: 1,
                    total_cpu_percent: 30.0,
                    total_memory_bytes: 600_000_000,
                    example_command: "/usr/bin/ollama serve --model llama3".into(),
                },
                AiWorkload {
                    label: "Claude".into(),
                    category: "agent-tool".into(),
                    process_count: 1,
                    total_cpu_percent: 20.0,
                    total_memory_bytes: 400_000_000,
                    example_command: "/usr/local/bin/claude serve".into(),
                },
            ],
        },
        processes: vec![ProcessRow {
            pid: 1234,
            name: "myapp".into(),
            cpu_percent: cpu,
            memory_bytes: 1_000_000,
            current_user: false,
            user_name: "root".into(),
            localhost: false,
            command: "/usr/local/bin/myapp --port 8080".into(),
            ai_label: None,
            ai_category: None,
            sustained_cpu: cpu,
        }],
        sort_mode: SortMode::Cpu,
        captured_at: "epoch 1".into(),
        disks: vec![],
        uptime_secs: 3600,
        boot_epoch: 1700000000,
    }
}

fn now_str() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ─── Test 1: migration_m003_tables ──────────────────────────────────────────

#[test]
fn migration_m003_tables() {
    let (_f, conn) = open_temp();

    let ver: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ver, 3, "user_version should be 3 after M001+M002+M003");

    let new_tables = [
        "host_snapshot",
        "host_port",
        "host_workload",
        "host_collect_status",
    ];
    for table in &new_tables {
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "table {table} should exist after M003");
    }
}

// ─── Test 2: insert_snapshot_rollups ────────────────────────────────────────

#[test]
fn insert_snapshot_rollups() {
    let (_f, mut conn) = open_temp();
    insert_test_node(&conn, "node-a");

    let snap = make_minimal_snapshot(42.5, Some(10.0));
    let now = now_str();
    let raw = serde_json::to_vec(&snap).unwrap();

    let snap_id = dbhost::insert_snapshot(&mut conn, "node-a", &raw, &snap, &now).unwrap();
    assert!(snap_id >= 1, "snapshot id should be positive");

    let (cpu, mem_used, mem_total, gpu, wc, pc): (f32, u64, u64, Option<f32>, i64, i64) = conn
        .query_row(
            "SELECT total_cpu_percent, used_memory_bytes, total_memory_bytes,
                    gpu_percent, workload_count, port_count
             FROM host_snapshot WHERE id=?1",
            [snap_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .unwrap();

    assert!((cpu - 42.5).abs() < 0.01, "cpu_percent mismatch");
    assert_eq!(mem_used, 8_000_000_000u64, "used_memory_bytes mismatch");
    assert_eq!(mem_total, 16_000_000_000u64, "total_memory_bytes mismatch");
    assert!(gpu.is_some(), "gpu_percent should be Some");
    assert!((gpu.unwrap() - 10.0).abs() < 0.01, "gpu_percent mismatch");
    // workload_count = ai_snapshot.workload_count (un-truncated total = 2)
    assert_eq!(wc, 2, "workload_count should be 2");
    // port_count = ports.len() = 2
    assert_eq!(pc, 2, "port_count should be 2");
}

// ─── Test 3: insert_snapshot_ports ──────────────────────────────────────────

#[test]
fn insert_snapshot_ports() {
    let (_f, mut conn) = open_temp();
    insert_test_node(&conn, "node-b");

    let snap = make_minimal_snapshot(10.0, None);
    let raw = serde_json::to_vec(&snap).unwrap();
    let snap_id = dbhost::insert_snapshot(&mut conn, "node-b", &raw, &snap, &now_str()).unwrap();

    let port_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM host_port WHERE snapshot_id=?1",
            [snap_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(port_count, 2, "should have 2 host_port rows");

    // Verify one specific port
    let port: u16 = conn
        .query_row(
            "SELECT port FROM host_port WHERE snapshot_id=?1 AND port=8080",
            [snap_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(port, 8080);
}

// ─── Test 4: insert_snapshot_workloads ──────────────────────────────────────

#[test]
fn insert_snapshot_workloads() {
    let (_f, mut conn) = open_temp();
    insert_test_node(&conn, "node-c");

    let snap = make_minimal_snapshot(10.0, None);
    let raw = serde_json::to_vec(&snap).unwrap();
    let snap_id = dbhost::insert_snapshot(&mut conn, "node-c", &raw, &snap, &now_str()).unwrap();

    let wl_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM host_workload WHERE snapshot_id=?1",
            [snap_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        wl_count,
        snap.ai_snapshot.top_workloads.len() as i64,
        "workload rows should match top_workloads count"
    );
}

// ─── Test 5: scrub_at_rest ───────────────────────────────────────────────────

#[test]
fn scrub_at_rest() {
    let (_f, mut conn) = open_temp();
    insert_test_node(&conn, "node-scrub");

    // Plant a secret in a ProcessRow.command (intentional test data)
    let secret = "supersecret_token_abc123"; // # pragma: allowlist secret
    let mut snap = make_minimal_snapshot(5.0, None);
    // Put secret in process command
    snap.processes[0].command = format!("/usr/bin/myapp --token={secret}");
    // Also put secret in an ai workload example_command
    snap.ai_snapshot.top_workloads[0].example_command =
        format!("/usr/bin/ollama serve --token={secret}");

    let raw = serde_json::to_vec(&snap).unwrap();
    let snap_id =
        dbhost::insert_snapshot(&mut conn, "node-scrub", &raw, &snap, &now_str()).unwrap();

    // Check that snapshot_json does NOT contain the secret
    let snapshot_json: String = conn
        .query_row(
            "SELECT snapshot_json FROM host_snapshot WHERE id=?1",
            [snap_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !snapshot_json.contains(secret),
        "secret leaked into snapshot_json: found `{secret}` in stored blob"
    );

    // Check that host_workload.example_command does NOT contain the secret
    let mut stmt = conn
        .prepare("SELECT example_command FROM host_workload WHERE snapshot_id=?1")
        .unwrap();
    let commands: Vec<String> = stmt
        .query_map([snap_id], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    for cmd in &commands {
        assert!(
            !cmd.contains(secret),
            "secret leaked into host_workload.example_command: found `{secret}` in `{cmd}`"
        );
    }
}

// ─── Test 6: cascade_delete ──────────────────────────────────────────────────

#[test]
fn cascade_delete() {
    let (_f, mut conn) = open_temp();
    insert_test_node(&conn, "node-cascade");

    let snap = make_minimal_snapshot(5.0, None);
    let raw = serde_json::to_vec(&snap).unwrap();
    let snap_id =
        dbhost::insert_snapshot(&mut conn, "node-cascade", &raw, &snap, &now_str()).unwrap();

    // Confirm child rows exist
    let port_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM host_port WHERE snapshot_id=?1",
            [snap_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(port_count > 0, "should have host_port rows before delete");

    // Delete the parent
    conn.execute("DELETE FROM host_snapshot WHERE id=?1", [snap_id])
        .unwrap();

    // Child rows should be gone (cascade)
    let port_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM host_port WHERE snapshot_id=?1",
            [snap_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(port_after, 0, "host_port rows should cascade-delete");

    let wl_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM host_workload WHERE snapshot_id=?1",
            [snap_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(wl_after, 0, "host_workload rows should cascade-delete");
}

// ─── Test 7: retention_latest_guard ─────────────────────────────────────────

#[test]
fn retention_latest_guard() {
    let (_f, mut conn) = open_temp();
    insert_test_node(&conn, "node-ret-a");
    insert_test_node(&conn, "node-ret-b");

    let snap = make_minimal_snapshot(5.0, None);
    let raw = serde_json::to_vec(&snap).unwrap();

    // Node A: old snapshot + recent snapshot
    let old_ts_a = "2026-01-01T00:00:00+00:00";
    let new_ts_a = "2026-06-22T12:00:00+00:00";
    let id_a_old = dbhost::insert_snapshot(&mut conn, "node-ret-a", &raw, &snap, old_ts_a).unwrap();
    let id_a_new = dbhost::insert_snapshot(&mut conn, "node-ret-a", &raw, &snap, new_ts_a).unwrap();

    // Node B: only an old snapshot (latest-guard must keep it)
    let old_ts_b = "2026-01-01T00:00:00+00:00";
    let id_b_old = dbhost::insert_snapshot(&mut conn, "node-ret-b", &raw, &snap, old_ts_b).unwrap();

    // Cutoff: anything before 2026-06-01 is "old"
    let cutoff = "2026-06-01T00:00:00+00:00";
    let deleted = dbhost::retention_sweep(&mut conn, cutoff).unwrap();

    // Only node-ret-a's old snapshot should be deleted
    assert_eq!(deleted, 1, "exactly 1 row should be deleted");

    // id_a_old should be gone
    let count_a_old: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM host_snapshot WHERE id=?1",
            [id_a_old],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count_a_old, 0, "node-a old snapshot should be deleted");

    // id_a_new should survive
    let count_a_new: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM host_snapshot WHERE id=?1",
            [id_a_new],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count_a_new, 1, "node-a recent snapshot should be kept");

    // id_b_old should survive (latest-guard: only snapshot for node-b)
    let count_b_old: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM host_snapshot WHERE id=?1",
            [id_b_old],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count_b_old, 1,
        "node-b sole (old) snapshot should be kept by latest-guard"
    );
}

// ─── Test 8: record_collect_failure_preserves_success ───────────────────────

#[test]
fn record_collect_failure_preserves_success() {
    let (_f, mut conn) = open_temp();
    insert_test_node(&conn, "node-fail");

    // First: insert a successful snapshot (sets last_success_at)
    let snap = make_minimal_snapshot(5.0, None);
    let raw = serde_json::to_vec(&snap).unwrap();
    let success_ts = "2026-06-22T10:00:00+00:00";
    dbhost::insert_snapshot(&mut conn, "node-fail", &raw, &snap, success_ts).unwrap();

    // Verify last_success_at is set
    let last_success: Option<String> = conn
        .query_row(
            "SELECT last_success_at FROM host_collect_status WHERE node_id=?1",
            ["node-fail"],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        last_success.is_some(),
        "last_success_at should be set after insert_snapshot"
    );
    assert_eq!(
        last_success.as_deref(),
        Some(success_ts),
        "last_success_at should match the inserted ts"
    );

    // Now record a failure
    let fail_ts = "2026-06-22T11:00:00+00:00";
    dbhost::record_collect_failure(&conn, "node-fail", fail_ts, "connection refused").unwrap();

    // last_success_at must remain unchanged
    let (last_attempt, last_success_after, last_error): (String, Option<String>, Option<String>) =
        conn.query_row(
            "SELECT last_attempt_at, last_success_at, last_error
             FROM host_collect_status WHERE node_id=?1",
            ["node-fail"],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();

    assert_eq!(
        last_attempt, fail_ts,
        "last_attempt_at should be updated to fail_ts"
    );
    assert_eq!(
        last_success_after.as_deref(),
        Some(success_ts),
        "last_success_at must NOT change on failure"
    );
    assert!(
        last_error.is_some(),
        "last_error should be set after record_collect_failure"
    );
    assert!(
        last_error.unwrap().contains("connection refused"),
        "last_error should contain the error message"
    );

    // Extra: first-ever failure for a new node leaves last_success_at NULL
    insert_test_node(&conn, "node-never-succeeded");
    let fail_ts2 = "2026-06-22T11:00:00+00:00";
    dbhost::record_collect_failure(&conn, "node-never-succeeded", fail_ts2, "timeout").unwrap();

    let first_failure_success: Option<String> = conn
        .query_row(
            "SELECT last_success_at FROM host_collect_status WHERE node_id=?1",
            ["node-never-succeeded"],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        first_failure_success.is_none(),
        "last_success_at should be NULL for first-ever failure"
    );
}

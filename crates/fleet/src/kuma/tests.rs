//! Unit tests for the pure Kuma reconcile + `MonitorSpec` serialization
//! contract + enrollment DB helpers. The wire protocol is tested separately in
//! `tests/kuma_transport.rs` (the non-ignored transport/replay test).

use super::*;
use crate::model::{MonitorSpec, MonitorType, RemoteMonitor};
use std::collections::BTreeMap;
use std::sync::Mutex;

// ─── MonitorSpec serialization contract (byte-match recorded 1.23.16) ────────
//
// A field rename / reorder fails HERE, not in production. The fixtures were
// LIVE-RECORDED against `louislam/uptime-kuma:1.23.16` (see scripts/kuma-record.js).

fn normalize(json: &str) -> serde_json::Value {
    serde_json::from_str(json).expect("fixture is valid JSON")
}

#[test]
fn monitor_spec_ping_matches_recorded_fixture() {
    let fixture = include_str!("../../tests/fixtures/kuma/monitor_spec_ping.json");
    let spec = MonitorSpec::ping("nas-01", "100.64.0.1", 1);
    let got = serde_json::to_value(&spec).unwrap();
    assert_eq!(
        got,
        normalize(fixture),
        "ping MonitorSpec must match recorded 1.23.16 payload"
    );
}

#[test]
fn monitor_spec_http_matches_recorded_fixture() {
    let fixture = include_str!("../../tests/fixtures/kuma/monitor_spec_http.json");
    let spec = MonitorSpec::http("site-01", "https://example.com/health", 1);
    let got = serde_json::to_value(&spec).unwrap();
    assert_eq!(
        got,
        normalize(fixture),
        "http MonitorSpec must match fixture"
    );
}

#[test]
fn monitor_spec_port_matches_recorded_fixture() {
    let fixture = include_str!("../../tests/fixtures/kuma/monitor_spec_port.json");
    let spec = MonitorSpec::port("db-01", "100.64.0.5", 5432, 1);
    let got = serde_json::to_value(&spec).unwrap();
    assert_eq!(
        got,
        normalize(fixture),
        "port MonitorSpec must match fixture"
    );
}

#[test]
fn monitor_spec_field_order_is_load_bearing() {
    // Byte-level (not just structural) check on the canonical ping shape: the
    // `type` key serializes first and the notification map last. Guards against
    // a reorder that serde_json::Value comparison would hide.
    let spec = MonitorSpec::ping("nas-01", "100.64.0.1", 1);
    let s = serde_json::to_string(&spec).unwrap();
    assert!(
        s.starts_with(r#"{"type":"ping","name":"nas-01","hostname":"100.64.0.1""#),
        "got: {s}"
    );
    assert!(
        s.ends_with(r#""notificationIDList":{"1":true}}"#),
        "got: {s}"
    );
}

#[test]
fn ntfy_id_zero_means_no_notification() {
    let spec = MonitorSpec::ping("x", "1.2.3.4", 0);
    assert!(spec.notification_id_list.is_empty());
    let s = serde_json::to_string(&spec).unwrap();
    assert!(s.contains(r#""notificationIDList":{}"#), "got: {s}");
}

// ─── monitorList broadcast parsing (the pushed object, keyed by id) ──────────

#[test]
fn parse_recorded_monitor_list_broadcast() {
    let raw = include_str!("../../tests/fixtures/kuma/monitor_list_broadcast.json");
    let mons = sio::parse_monitor_list(&serde_json::from_str(raw).unwrap()).unwrap();
    assert_eq!(mons.len(), 1, "one monitor in the broadcast");
    let m = &mons[0];
    assert_eq!(m.id, 2);
    assert_eq!(m.name, "nas-01");
    assert_eq!(m.monitor_type, MonitorType::Ping);
    assert_eq!(m.hostname.as_deref(), Some("100.64.0.1"));
    assert_eq!(m.interval, 60);
    assert_eq!(m.notification_id_list.get("2"), Some(&true));
}

#[test]
fn parse_empty_monitor_list_broadcast() {
    let raw = include_str!("../../tests/fixtures/kuma/monitor_list_empty.json");
    let mons = sio::parse_monitor_list(&serde_json::from_str(raw).unwrap()).unwrap();
    assert!(mons.is_empty());
}

// ─── ACK frame parsing (against the LIVE-RECORDED 1.23.16 ack frames) ────────
//
// The ack parsers take a rust_socketio `Payload`; a real ACK arrives as
// `Payload::Text(vec![<the ack object>])`. Wrapping each recorded fixture that
// way exercises the exact parse path used in production.

fn as_ack(json: &str) -> rust_socketio::Payload {
    rust_socketio::Payload::Text(vec![serde_json::from_str(json).unwrap()])
}

#[test]
fn ack_token_extracts_jwt_from_recorded_login_ack() {
    let raw = include_str!("../../tests/fixtures/kuma/login_ack.json");
    let token = sio::ack_token(&as_ack(raw)).unwrap();
    assert!(!token.is_empty(), "login ACK carried a token");
}

#[test]
fn ack_monitor_id_extracts_id_from_recorded_add_ack() {
    let raw = include_str!("../../tests/fixtures/kuma/add_ack.json");
    assert_eq!(sio::ack_monitor_id(&as_ack(raw)).unwrap(), 2);
}

#[test]
fn ack_ok_accepts_recorded_edit_and_delete_acks() {
    let edit = include_str!("../../tests/fixtures/kuma/edit_ack.json");
    let del = include_str!("../../tests/fixtures/kuma/delete_ack.json");
    sio::ack_ok(&as_ack(edit), "editMonitor").unwrap();
    sio::ack_ok(&as_ack(del), "deleteMonitor").unwrap();
}

#[test]
fn ack_token_rejects_failed_login() {
    let payload = as_ack(r#"{"ok":false,"msg":"Incorrect username or password."}"#);
    let err = sio::ack_token(&payload).unwrap_err();
    assert!(err.to_string().contains("login rejected"), "got: {err}");
}

// ─── Fake KumaClient for pure reconcile tests ────────────────────────────────

#[derive(Default)]
struct FakeKuma {
    monitors: Vec<RemoteMonitor>,
    calls: Mutex<Vec<String>>,
    next_id: Mutex<i64>,
}

impl FakeKuma {
    fn new(monitors: Vec<RemoteMonitor>) -> Self {
        let max = monitors.iter().map(|m| m.id).max().unwrap_or(0);
        Self {
            monitors,
            calls: Mutex::new(Vec::new()),
            next_id: Mutex::new(max + 1),
        }
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl KumaClient for FakeKuma {
    async fn list(&self) -> anyhow::Result<Vec<RemoteMonitor>> {
        self.calls.lock().unwrap().push("list".to_owned());
        Ok(self.monitors.clone())
    }
    async fn add(&self, m: &MonitorSpec) -> anyhow::Result<i64> {
        let mut id = self.next_id.lock().unwrap();
        let new = *id;
        *id += 1;
        self.calls.lock().unwrap().push(format!("add:{}", m.name));
        Ok(new)
    }
    async fn edit(&self, id: i64, m: &MonitorSpec) -> anyhow::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("edit:{}:{}", id, m.name));
        Ok(())
    }
    async fn delete(&self, id: i64) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!("delete:{id}"));
        Ok(())
    }
}

fn remote(id: i64, name: &str, host: &str, interval: u32) -> RemoteMonitor {
    let mut n = BTreeMap::new();
    n.insert("1".to_owned(), true);
    RemoteMonitor {
        id,
        name: name.to_owned(),
        monitor_type: MonitorType::Ping,
        url: None,
        hostname: Some(host.to_owned()),
        port: None,
        interval,
        maxretries: 1,
        notification_id_list: n,
    }
}

#[tokio::test]
async fn reconcile_adds_absent_monitor() {
    let fake = FakeKuma::new(vec![]);
    let want = vec![MonitorSpec::ping("nas-01", "100.64.0.1", 1)];
    reconcile(&fake, &want, DELETE_GUARD_PCT as u8)
        .await
        .unwrap();
    assert_eq!(fake.calls(), vec!["list", "add:nas-01"]);
}

#[tokio::test]
async fn reconcile_edits_drifted_monitor_with_full_object() {
    // Present but interval drifted (server=120, want=60) → edit with full object.
    let fake = FakeKuma::new(vec![remote(7, "nas-01", "100.64.0.1", 120)]);
    let want = vec![MonitorSpec::ping("nas-01", "100.64.0.1", 1)];
    reconcile(&fake, &want, DELETE_GUARD_PCT as u8)
        .await
        .unwrap();
    assert_eq!(fake.calls(), vec!["list", "edit:7:nas-01"]);
}

#[tokio::test]
async fn reconcile_in_sync_is_noop() {
    let fake = FakeKuma::new(vec![remote(7, "nas-01", "100.64.0.1", 60)]);
    let want = vec![MonitorSpec::ping("nas-01", "100.64.0.1", 1)];
    reconcile(&fake, &want, DELETE_GUARD_PCT as u8)
        .await
        .unwrap();
    assert_eq!(
        fake.calls(),
        vec!["list"],
        "no add/edit/delete when in sync"
    );
}

#[tokio::test]
async fn reconcile_deletes_undesired_monitor() {
    // 2 present, 1 desired → delete the other (1/2 = 50% > 40% would trip; use
    // 3 present so 1/3 = 33% is under guard).
    let fake = FakeKuma::new(vec![
        remote(1, "keep-a", "100.64.0.1", 60),
        remote(2, "keep-b", "100.64.0.2", 60),
        remote(3, "gone", "100.64.0.9", 60),
    ]);
    let want = vec![
        MonitorSpec::ping("keep-a", "100.64.0.1", 1),
        MonitorSpec::ping("keep-b", "100.64.0.2", 1),
    ];
    reconcile(&fake, &want, DELETE_GUARD_PCT as u8)
        .await
        .unwrap();
    assert_eq!(fake.calls(), vec!["list", "delete:3"]);
}

// ── Delete-guard boundaries: empty have, empty want, exactly 40, just over ───

#[tokio::test]
async fn guard_empty_have_no_op() {
    let fake = FakeKuma::new(vec![]);
    // empty want too → nothing happens, guard does not divide-by-zero.
    reconcile(&fake, &[], DELETE_GUARD_PCT as u8).await.unwrap();
    assert_eq!(fake.calls(), vec!["list"]);
}

#[tokio::test]
async fn guard_empty_want_against_nonempty_have_trips() {
    // want empty, have 2 → 100% deletion → guard trips, NOTHING mutated.
    let fake = FakeKuma::new(vec![
        remote(1, "a", "100.64.0.1", 60),
        remote(2, "b", "100.64.0.2", 60),
    ]);
    let err = reconcile(&fake, &[], DELETE_GUARD_PCT as u8)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("delete-guard"), "got: {err}");
    assert_eq!(fake.calls(), vec!["list"], "no delete after guard trips");
}

#[tokio::test]
async fn guard_exactly_at_threshold_does_not_trip() {
    // 5 have, 2 to delete = exactly 40% → does NOT trip (strictly greater).
    let mut have = Vec::new();
    let mut want = Vec::new();
    for i in 0..5 {
        have.push(remote(i, &format!("m{i}"), "1.2.3.4", 60));
    }
    // keep 3 (m0..m2), delete 2 (m3,m4) = 40%.
    for i in 0..3 {
        want.push(MonitorSpec::ping(&format!("m{i}"), "1.2.3.4", 1));
    }
    let fake = FakeKuma::new(have);
    reconcile(&fake, &want, DELETE_GUARD_PCT as u8)
        .await
        .unwrap();
    let calls = fake.calls();
    assert!(calls.contains(&"delete:3".to_owned()), "got: {calls:?}");
    assert!(calls.contains(&"delete:4".to_owned()), "got: {calls:?}");
}

#[tokio::test]
async fn guard_just_over_threshold_trips() {
    // 5 have, 3 to delete = 60% > 40% → trips, nothing mutated.
    let mut have = Vec::new();
    for i in 0..5 {
        have.push(remote(i, &format!("m{i}"), "1.2.3.4", 60));
    }
    let want = vec![
        MonitorSpec::ping("m0", "1.2.3.4", 1),
        MonitorSpec::ping("m1", "1.2.3.4", 1),
    ];
    let fake = FakeKuma::new(have);
    let err = reconcile(&fake, &want, DELETE_GUARD_PCT as u8)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("delete-guard"), "got: {err}");
    // Guard runs before add/edit too — only `list` happened.
    assert_eq!(fake.calls(), vec!["list"]);
}

// ─── enrollment DB round-trip ────────────────────────────────────────────────

#[test]
fn kuma_enrollment_db_round_trip() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let conn = crate::db::open(f.path()).unwrap();
    let now = chrono::Utc::now();
    let node = crate::model::Node {
        fleet_id: "n1".to_owned(),
        hostname: "h".to_owned(),
        fqdn: "h.local".to_owned(),
        seen_in: vec![],
        addresses: vec![],
        os: "linux".to_owned(),
        online: true,
        last_seen: now,
        tags: crate::model::Tags::default(),
        tier: crate::model::Tier::Agentless,
        dedupe_key_kind: crate::model::DedupeKind::Fuzzy,
        notes: None,
        first_seen: now,
        updated_at: now,
        fuzzy_hint: None,
    };
    crate::db::nodes::upsert_node(&conn, &node).unwrap();

    upsert_enrollment(&conn, "n1", "42").unwrap();
    let rows = list_enrollments(&conn).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].fleet_id, "n1");
    assert_eq!(rows[0].remote_id, "42");

    upsert_enrollment(&conn, "n1", "99").unwrap();
    let rows = list_enrollments(&conn).unwrap();
    assert_eq!(rows.len(), 1, "idempotent upsert");
    assert_eq!(rows[0].remote_id, "99");

    delete_enrollment(&conn, "n1").unwrap();
    assert!(list_enrollments(&conn).unwrap().is_empty());
}

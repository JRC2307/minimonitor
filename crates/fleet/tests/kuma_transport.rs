//! Non-ignored transport test for the Kuma socket.io wire protocol (spec §3.7).
//!
//! This is the **load-bearing** proof: it stands up a real socket.io v4 server
//! (`socketioxide`) that mimics Uptime-Kuma's push-based dance — it **pushes a
//! `monitorList` broadcast on connect**, ACKs `login` with a JWT, and ACKs `add`
//! with a new `monitorID` — then drives the production `rust_socketio`-backed
//! [`SioKumaClient::connect_and_login`] against it and asserts:
//!
//!   1. `list()` resolves from the **pushed** broadcast (the oneshot armed
//!      BEFORE connect), proving the out-of-band push is captured, and
//!   2. `add()` returns the new `monitorID` from its ACK.
//!
//! Both ends speak engine.io v4, so this exercises the exact transport the
//! pinned 1.23.16 container uses. The monitorList frame replayed here is the
//! one LIVE-RECORDED from `louislam/uptime-kuma:1.23.16` (see scripts/kuma-record.js;
//! fixture `tests/fixtures/kuma/monitor_list_broadcast.json`).

use std::time::Duration;

use axum::Router;
use fleet::kuma::KumaClient;
use fleet::kuma::sio::SioKumaClient;
use fleet::model::MonitorType;
use serde_json::Value;
use socketioxide::SocketIo;
use socketioxide::extract::{AckSender, Data, SocketRef};
use tokio::net::TcpListener;

/// The recorded 1.23.16 monitorList broadcast (object keyed by stringified id).
const MONITOR_LIST: &str = include_str!("fixtures/kuma/monitor_list_broadcast.json");

#[tokio::test]
async fn connect_login_resolves_pushed_list_and_add_returns_monitor_id() {
    // ── Stand up a Kuma-shaped socket.io server ──────────────────────────────
    let (layer, io) = SocketIo::new_layer();

    io.ns("/", |socket: SocketRef| async move {
        // PUSH the monitorList broadcast immediately on connect — exactly like
        // Kuma does right after auth. This is the out-of-band frame the client's
        // pre-armed oneshot must capture.
        let list: Value = serde_json::from_str(MONITOR_LIST).unwrap();
        socket.emit("monitorList", &list).ok();

        // login → ACK with a JWT (the shape `ack_token` parses).
        socket.on(
            "login",
            |ack: AckSender, Data::<Value>(_creds)| async move {
                ack.send(&serde_json::json!({
                    "ok": true,
                    "token": "eyJ-test-jwt"
                }))
                .ok();
            },
        );

        // add → ACK with a fresh monitorID (the shape `ack_monitor_id` parses).
        socket.on("add", |ack: AckSender, Data::<Value>(_spec)| async move {
            ack.send(&serde_json::json!({
                "ok": true,
                "msg": "Added Successfully.",
                "monitorID": 99
            }))
            .ok();
        });
    });

    let app = Router::new().layer(layer);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let url = format!("http://{addr}");

    // ── Drive the production client through the real transport ────────────────
    let client = tokio::time::timeout(
        Duration::from_secs(15),
        SioKumaClient::connect_and_login(&url, "admin", "pw"),
    )
    .await
    .expect("connect_and_login timed out")
    .expect("connect_and_login failed");

    // (1) list() resolved from the PUSHED broadcast (pre-armed oneshot).
    let monitors = client.list().await.unwrap();
    assert_eq!(monitors.len(), 1, "one monitor from the pushed broadcast");
    assert_eq!(monitors[0].id, 2);
    assert_eq!(monitors[0].name, "nas-01");
    assert_eq!(monitors[0].monitor_type, MonitorType::Ping);
    assert_eq!(monitors[0].hostname.as_deref(), Some("100.64.0.1"));

    // (2) add() returns the new monitorID from its ACK.
    let spec = fleet::model::MonitorSpec::ping("worker-01", "100.64.0.7", 0);
    let new_id = client.add(&spec).await.unwrap();
    assert_eq!(new_id, 99, "add returns the monitorID from the ACK");

    server.abort();
}

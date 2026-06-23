//! The ONE place the Uptime-Kuma socket.io wire protocol lives (spec §3.7).
//!
//! Kuma's management API is push-based, so [`KumaClient::list`] cannot be a plain
//! request/response: the server **pushes** a `monitorList` broadcast right after
//! authentication. [`SioKumaClient::connect_and_login`] therefore **arms a oneshot
//! BEFORE `connect()`**, captures the broadcast in the event handler, logs in via
//! `emit_with_ack("login")` (the ACK carries the JWT), and resolves `list()` from
//! the captured broadcast. `add`/`edit`/`delete` are `emit_with_ack` calls whose
//! ACKs are funneled back through a per-call oneshot (rust_socketio's ack callback
//! is fire-and-forget and returns `()`).
//!
//! This module is the unstable surface (version-coupled to Kuma 1.23.x). It is
//! kept thin and isolated behind the [`KumaClient`] trait so a protocol bump
//! touches one file. Pure parsing ([`parse_monitor_list`], [`ack_token`],
//! [`ack_monitor_id`]) is split out and unit-tested against recorded frames.

use super::KumaClient;
use crate::model::{MonitorSpec, RemoteMonitor};
use anyhow::{Context, anyhow, bail};
use futures_util::FutureExt;
use rust_socketio::Payload;
use rust_socketio::TransportType;
use rust_socketio::asynchronous::{Client, ClientBuilder};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, oneshot};

const ACK_TIMEOUT: Duration = Duration::from_secs(10);
const MONITOR_LIST_TIMEOUT: Duration = Duration::from_secs(10);

// ─── Pure frame parsing (unit-tested against recorded 1.23.16 frames) ────────

/// Parse the pushed `monitorList` broadcast — an **object keyed by stringified
/// monitor id** whose values are full Kuma monitor objects — into the slim
/// [`RemoteMonitor`]s reconcile needs. Unknown/extra fields are ignored.
pub fn parse_monitor_list(payload: &Value) -> anyhow::Result<Vec<RemoteMonitor>> {
    let obj = payload
        .as_object()
        .ok_or_else(|| anyhow!("monitorList payload is not a JSON object"))?;
    let mut out = Vec::with_capacity(obj.len());
    for (key, v) in obj {
        let m: RemoteMonitor = serde_json::from_value(v.clone())
            .with_context(|| format!("parsing monitor id={key} from monitorList"))?;
        out.push(m);
    }
    out.sort_by_key(|m| m.id);
    Ok(out)
}

/// Extract the first object argument from a socket.io ACK `Payload::Text`.
///
/// rust_socketio surfaces ACK args as `Payload::Text(Vec<Value>)`. Depending on
/// how the server encoded the single ACK argument, the first slot is either the
/// object itself or a one-element array wrapping it — descend through one such
/// array so both wire encodings yield the inner ACK object.
fn ack_first_object(payload: &Payload) -> anyhow::Result<Value> {
    match payload {
        Payload::Text(values) => {
            let first = values
                .first()
                .cloned()
                .ok_or_else(|| anyhow!("empty ACK payload"))?;
            match first {
                Value::Array(mut arr) if arr.len() == 1 && arr[0].is_object() => Ok(arr.remove(0)),
                other => Ok(other),
            }
        }
        // Kuma never replies binary for these events.
        other => bail!("unexpected non-text ACK payload: {other:?}"),
    }
}

/// Pull the JWT out of a `login` ACK (`{ ok: true, token: "..." }`).
pub fn ack_token(payload: &Payload) -> anyhow::Result<String> {
    let v = ack_first_object(payload)?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        let msg = v
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("login failed");
        bail!("kuma login rejected: {msg}");
    }
    v.get("token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("login ACK carried no token"))
}

/// Pull the new `monitorID` out of an `add` ACK (`{ ok: true, monitorID: N }`).
pub fn ack_monitor_id(payload: &Payload) -> anyhow::Result<i64> {
    let v = ack_first_object(payload)?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        let msg = v.get("msg").and_then(Value::as_str).unwrap_or("add failed");
        bail!("kuma add rejected: {msg}");
    }
    v.get("monitorID")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("add ACK carried no monitorID"))
}

/// Assert an `editMonitor`/`deleteMonitor` ACK is `{ ok: true }`.
pub fn ack_ok(payload: &Payload, what: &str) -> anyhow::Result<()> {
    let v = ack_first_object(payload)?;
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        let msg = v.get("msg").and_then(Value::as_str).unwrap_or("rejected");
        bail!("kuma {what} rejected: {msg}")
    }
}

// ─── Live socket.io client ───────────────────────────────────────────────────

/// A connected, authenticated Kuma socket.io session.
///
/// `monitors` is the snapshot captured from the `monitorList` broadcast pushed at
/// login time — [`KumaClient::list`] returns it without another round-trip.
pub struct SioKumaClient {
    client: Client,
    #[allow(dead_code)] // retained for future re-auth / debugging; not re-sent.
    token: String,
    monitors: Vec<RemoteMonitor>,
}

impl SioKumaClient {
    /// Connect to `url`, arm the `monitorList` oneshot BEFORE connecting, log in
    /// (capturing the JWT from the ACK), and resolve the pushed broadcast.
    pub async fn connect_and_login(url: &str, user: &str, password: &str) -> anyhow::Result<Self> {
        // Arm the monitorList oneshot BEFORE connect — the broadcast can arrive
        // the instant auth completes, possibly before login()'s ACK returns.
        let (mon_tx, mon_rx) = oneshot::channel::<anyhow::Result<Vec<RemoteMonitor>>>();
        let mon_tx = Arc::new(Mutex::new(Some(mon_tx)));

        let handler_tx = mon_tx.clone();
        let client = ClientBuilder::new(url.to_owned())
            .reconnect(false)
            // Pin engine.io long-polling (no websocket upgrade). The fleet↔Kuma
            // link is low-latency on the tailnet, polling is the universally
            // interoperable engine.io transport (Kuma + socketioxide both speak
            // it cleanly), and it sidesteps rust_engineio's flaky WS-upgrade
            // probe. This is also what keeps the contract/transport test green.
            .transport_type(TransportType::Polling)
            .on("monitorList", move |payload, _client| {
                let handler_tx = handler_tx.clone();
                async move {
                    // Fire the oneshot only on the FIRST monitorList push.
                    let parsed = match &payload {
                        Payload::Text(values) => values
                            .first()
                            .ok_or_else(|| anyhow!("empty monitorList push"))
                            .and_then(parse_monitor_list),
                        other => Err(anyhow!("unexpected monitorList payload: {other:?}")),
                    };
                    if let Some(tx) = handler_tx.lock().await.take() {
                        let _ = tx.send(parsed);
                    }
                }
                .boxed()
            })
            .connect()
            .await
            .context("connecting to kuma socket.io")?;

        // Login is an async ACK carrying the JWT. Funnel the ACK back via oneshot.
        let token = emit_with_ack(
            &client,
            "login",
            serde_json::json!({ "username": user, "password": password, "token": "" }),
        )
        .await
        .and_then(|p| ack_token(&p))
        .context("kuma login")?;

        let monitors = tokio::time::timeout(MONITOR_LIST_TIMEOUT, mon_rx)
            .await
            .context("timed out waiting for the pushed monitorList broadcast")?
            .context("monitorList oneshot dropped")??;

        Ok(Self {
            client,
            token,
            monitors,
        })
    }
}

/// `emit_with_ack`, made awaitable: rust_socketio's ack callback is fire-and-forget
/// and returns `()`, so we bridge its single ACK back through a oneshot.
async fn emit_with_ack(
    client: &Client,
    event: &'static str,
    data: Value,
) -> anyhow::Result<Payload> {
    let (tx, rx) = oneshot::channel::<Payload>();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let cb_tx = tx.clone();
    client
        .emit_with_ack(event, data, ACK_TIMEOUT, move |payload, _client| {
            let cb_tx = cb_tx.clone();
            async move {
                if let Some(tx) = cb_tx.lock().await.take() {
                    let _ = tx.send(payload);
                }
            }
            .boxed()
        })
        .await
        .with_context(|| format!("emit_with_ack({event}) send failed"))?;

    tokio::time::timeout(ACK_TIMEOUT + Duration::from_secs(1), rx)
        .await
        .with_context(|| format!("ACK timeout for {event}"))?
        .with_context(|| format!("ACK oneshot dropped for {event}"))
}

#[async_trait::async_trait]
impl KumaClient for SioKumaClient {
    async fn list(&self) -> anyhow::Result<Vec<RemoteMonitor>> {
        // Resolved from the pushed broadcast captured at login.
        Ok(self.monitors.clone())
    }

    async fn add(&self, m: &MonitorSpec) -> anyhow::Result<i64> {
        let payload = emit_with_ack(&self.client, "add", serde_json::to_value(m)?).await?;
        ack_monitor_id(&payload)
    }

    async fn edit(&self, id: i64, m: &MonitorSpec) -> anyhow::Result<()> {
        // editMonitor needs the FULL object plus the id.
        let mut body = serde_json::to_value(m)?;
        if let Value::Object(map) = &mut body {
            map.insert("id".to_owned(), Value::from(id));
        }
        let payload = emit_with_ack(&self.client, "editMonitor", body).await?;
        ack_ok(&payload, "editMonitor")
    }

    async fn delete(&self, id: i64) -> anyhow::Result<()> {
        let payload = emit_with_ack(&self.client, "deleteMonitor", Value::from(id)).await?;
        ack_ok(&payload, "deleteMonitor")
    }
}

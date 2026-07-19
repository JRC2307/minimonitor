//! `/hub/*` — server-side proxy to sibling loopback services.
//!
//! The caguastore page is served over Tailscale HTTPS, so the browser cannot
//! call the plain-HTTP loopback ports of the sibling services (Command Center
//! :8787, cuentas :8789, hermeshub :8796) directly. These routes forward the
//! request server-side — fleet-serve runs on the same host in prod.
//!
//! Policy:
//! - `/hub/cc/{*rest}`      → `{cc_url}/api/{rest}`      — GET, POST, DELETE
//! - `/hub/cuentas/{*rest}` → `{cuentas_url}/api/{rest}` — GET only
//! - `/hub/hermes/{*rest}`  → `{hermeshub_url}/api/{rest}` — GET only
//!
//! Query string, JSON body, and upstream status pass through. 4 s timeout;
//! upstream failure yields a graceful `502 {"error": ...}`.

use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::{Method, StatusCode, header},
    response::{IntoResponse, Response},
};

use super::routes::AppState;

/// Per-request upstream timeout.
const HUB_TIMEOUT: Duration = Duration::from_secs(4);

fn json_error(status: StatusCode, msg: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({ "error": msg }).to_string(),
    )
        .into_response()
}

/// Forward `method /api/{rest}?{query}` to `base`, enforcing `allowed` methods.
async fn proxy(
    state: &AppState,
    base: &str,
    allowed: &[Method],
    method: Method,
    rest: &str,
    query: Option<String>,
    body: Bytes,
) -> Response {
    if !allowed.contains(&method) {
        return json_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }

    let mut url = format!("{}/api/{}", base.trim_end_matches('/'), rest);
    if let Some(q) = query {
        url.push('?');
        url.push_str(&q);
    }

    let mut req = state.http.request(method, &url).timeout(HUB_TIMEOUT);
    if !body.is_empty() {
        req = req
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
    }

    match req.send().await {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let content_type = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_owned();
            match resp.bytes().await {
                Ok(bytes) => {
                    (status, [(header::CONTENT_TYPE, content_type)], bytes).into_response()
                }
                Err(e) => json_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("upstream body read failed: {e}"),
                ),
            }
        }
        Err(e) => json_error(StatusCode::BAD_GATEWAY, &format!("upstream unreachable: {e}")),
    }
}

/// `/hub/cc/{*rest}` — Command Center. GET + POST + DELETE (task CRUD).
pub async fn hub_cc(
    State(state): State<AppState>,
    method: Method,
    Path(rest): Path<String>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let base = state.cc_url.clone();
    proxy(
        &state,
        &base,
        &[Method::GET, Method::POST, Method::DELETE],
        method,
        &rest,
        query,
        body,
    )
    .await
}

/// `/hub/cuentas/{*rest}` — cuentas. Read-only, and PIN-gated: money numbers
/// must never reach an un-unlocked browser. Requires `X-Money-Pin` matching
/// `[serve] money_pin`; with no PIN configured the proxy is disabled entirely.
pub async fn hub_cuentas(
    State(state): State<AppState>,
    method: Method,
    Path(rest): Path<String>,
    RawQuery(query): RawQuery,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let Some(pin) = state.money_pin.clone() else {
        return json_error(StatusCode::NOT_FOUND, "money proxy disabled");
    };
    let presented = headers
        .get("x-money-pin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if presented != pin {
        return json_error(StatusCode::UNAUTHORIZED, "money pin required");
    }
    let base = state.cuentas_url.clone();
    proxy(&state, &base, &[Method::GET], method, &rest, query, body).await
}

/// `/hub/hermes/{*rest}` — hermeshub. Read-only.
pub async fn hub_hermes(
    State(state): State<AppState>,
    method: Method,
    Path(rest): Path<String>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let base = state.hermeshub_url.clone();
    proxy(&state, &base, &[Method::GET], method, &rest, query, body).await
}

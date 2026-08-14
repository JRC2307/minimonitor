//! `/hub/*` — server-side proxy to sibling loopback services.
//!
//! The caguastore page is served over Tailscale HTTPS, so the browser cannot
//! call the plain-HTTP loopback ports of the sibling services (Command Center
//! :8787, cuentas :8789, hermeshub :8796) directly. These routes forward the
//! request server-side — fleet-serve runs on the same host in prod.
//!
//! Policy:
//! - `/hub/cc/{*rest}`      → `{cc_url}/api/{rest}`      — GET, POST, DELETE
//! - `/hub/today/{*rest}`   → `{cc_url}/api/{rest}`      — *path*-whitelisted
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
/// `basic_auth` (`"user:pass"`) is presented upstream when the sibling service
/// has its own credential (cuentas fails closed since 2026-07-24);
/// `bearer_auth` likewise for token-authed siblings (portfolio).
async fn proxy(
    state: &AppState,
    base: &str,
    basic_auth: Option<&str>,
    bearer_auth: Option<&str>,
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
    if let Some(cred) = basic_auth {
        let (user, pass) = cred.split_once(':').unwrap_or((cred, ""));
        req = req.basic_auth(user, Some(pass));
    }
    if let Some(token) = bearer_auth {
        req = req.bearer_auth(token);
    }
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
        None,
        None,
        &[Method::GET, Method::POST, Method::DELETE],
        method,
        &rest,
        query,
        body,
    )
    .await
}

// ── /hub/today — the narrow Command Center surface ────────────────────────────
//
// `/hub/cc/*` is the *browser* board's proxy: whatever the kanban needs, which
// is effectively the whole task API. The native cagua app and its widgets need
// only a handful of calls, and they run on a phone that is off the tailnet half
// the time, so they get their own door with a **path** whitelist instead of only
// a method one. That is what lets the app talk HTTPS through fleet-serve and
// carry no ATS exception for the plain-HTTP Command Center on :8787.

/// The only task statuses the narrow proxy will forward. Anything else is a
/// vocabulary the app does not have, so it is somebody probing the door.
const TODAY_STATUSES: [&str; 3] = ["backlog", "in_progress", "done"];

/// `tasks/{id}` with a numeric id — the single patchable path.
fn is_task_id_path(rest: &str) -> bool {
    match rest.strip_prefix("tasks/") {
        Some(id) => !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// `tasks/reorder` — the bulk reorder write. It is a *literal* path that sits in
/// the same slot as `tasks/{id}`, so it must be matched **before** the numeric-id
/// matcher and never fall through to it: a POST here is a whole-project
/// reordering, not a status patch on a task whose id happens to read "reorder".
/// (`is_task_id_path` would reject it anyway on the all-digits test, but the two
/// are kept explicitly disjoint so neither can drift into the other's door.)
const TODAY_REORDER_PATH: &str = "tasks/reorder";

/// `tasks/recent-done` — the read that backs the phone's "últimas completadas"
/// undo list. Same literal-vs-numeric note as reorder above.
const TODAY_RECENT_DONE_PATH: &str = "tasks/recent-done";

/// `tasks/{id}/punt` with a numeric id — the one *reordering* write. It carries
/// no payload of its own (the server derives the new position), so unlike the
/// status patch there is nothing in the body to whitelist; there just must not
/// be anything in it.
fn is_task_punt_path(rest: &str) -> bool {
    match rest
        .strip_prefix("tasks/")
        .and_then(|s| s.strip_suffix("/punt"))
    {
        Some(id) => !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// A punt body must be absent or an empty JSON object. Anything with fields in
/// it is somebody trying to smuggle a patch through the reordering door.
fn is_empty_body(body: &[u8]) -> bool {
    if body.iter().all(u8::is_ascii_whitespace) {
        return true;
    }
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.as_object().map(|o| o.is_empty()))
        .unwrap_or(false)
}

/// The one body shape allowed through: `{"status": "<known status>"}` and
/// nothing else. Title edits, priority changes and deletes stay on `/hub/cc`,
/// behind the browser, where a human is looking at what they are doing.
fn is_status_patch(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let Some(obj) = value.as_object() else {
        return false;
    };
    obj.len() == 1
        && obj
            .get("status")
            .and_then(|s| s.as_str())
            .is_some_and(|s| TODAY_STATUSES.contains(&s))
}

/// The one body shape the reorder door accepts: exactly
/// `{"project_id": <int>, "ordered_ids": [<int>, ...]}` — both keys, no others,
/// no string ids, no nulls. `ordered_ids` must be **non-empty**: an empty list is
/// never a reordering the phone meant to send, and the Command Center itself
/// rejects it (`ordered_ids` has `min_length=1`), so refusing here just saves the
/// round trip and returns the clearer error.
fn is_reorder_body(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let Some(obj) = value.as_object() else {
        return false;
    };
    if obj.len() != 2 {
        return false;
    }
    let project_id_ok = obj.get("project_id").is_some_and(|v| {
        // serde_json treats 1.0 as f64, not u64 — a float is not an id.
        v.is_i64() || v.is_u64()
    });
    let ordered_ids_ok = obj
        .get("ordered_ids")
        .and_then(|v| v.as_array())
        .is_some_and(|ids| !ids.is_empty() && ids.iter().all(|id| id.is_i64() || id.is_u64()));
    project_id_ok && ordered_ids_ok
}

/// `/hub/today/{*rest}` — Command Center, path-whitelisted for the native app:
///
/// - `GET  tasks[?query]`             → the board (the `today` ranking runs client-side)
/// - `GET  fronts[?query]`            → the per-project front view, ranked server-side
/// - `GET  tasks/recent-done[?query]` → the "últimas completadas" undo list
/// - `POST tasks/reorder`             → `{"project_id": .., "ordered_ids": [..]}` only
/// - `POST tasks/{id}`                → `{"status": "..."}` only
/// - `POST tasks/{id}/punt`           → empty body; sends a task down its own queue
///
/// Anything else is a 404 (path not proxied) or a 405 (wrong method for a
/// proxied path); a POST whose body is not a bare status patch (or, for a punt,
/// not empty; for a reorder, not exactly the two-key shape) is a 400.
///
/// Note the match order: the two literal `tasks/<word>` paths are tested before
/// the numeric `tasks/{id}` matcher, so neither can be mistaken for a task id.
pub async fn hub_today(
    State(state): State<AppState>,
    method: Method,
    Path(rest): Path<String>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let rest = rest.trim_end_matches('/');

    if rest == "tasks" || rest == "fronts" || rest == TODAY_RECENT_DONE_PATH {
        if method != Method::GET {
            return json_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
    } else if rest == TODAY_REORDER_PATH {
        // Before the numeric-id matcher on purpose: "reorder" is a literal path,
        // never a task id, and must not be treated as a status patch.
        if method != Method::POST {
            return json_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        if !is_reorder_body(&body) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "reorder takes exactly {\"project_id\": <int>, \"ordered_ids\": [<int>, ...]} \
                 with a non-empty id list",
            );
        }
    } else if is_task_punt_path(rest) {
        if method != Method::POST {
            return json_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        if !is_empty_body(&body) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "punt takes no body — use POST tasks/{id} to change a task",
            );
        }
    } else if is_task_id_path(rest) {
        if method != Method::POST {
            return json_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        if !is_status_patch(&body) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "only a bare {\"status\": ...} patch is proxied here",
            );
        }
    } else {
        return json_error(StatusCode::NOT_FOUND, "path not proxied");
    }

    let base = state.cc_url.clone();
    proxy(
        &state,
        &base,
        None,
        None,
        &[Method::GET, Method::POST],
        method,
        rest,
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
    let auth = state.cuentas_basic_auth.clone();
    proxy(
        &state,
        &base,
        auth.as_deref(),
        None,
        &[Method::GET],
        method,
        &rest,
        query,
        body,
    )
    .await
}

/// POST-able hermeshub endpoints for the launcher's quick-prompt lane: send a
/// message, create the channel, set its model, mark it read. Everything else
/// stays read-only (no channel close/reopen, no relay control).
fn hermes_post_allowed(rest: &str) -> bool {
    rest == "send"
        || rest == "channels"
        || (rest.starts_with("channels/") && (rest.ends_with("/model") || rest.ends_with("/read")))
}

/// `/hub/portfolio/{*rest}` — portfolio (inversiones). Read-only and money
/// PIN-gated like cuentas; the service's own bearer token is injected
/// server-side so it never reaches the browser.
pub async fn hub_portfolio(
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
    let base = state.portfolio_url.clone();
    let token = state.portfolio_token.clone();
    proxy(
        &state,
        &base,
        None,
        token.as_deref(),
        &[Method::GET],
        method,
        &rest,
        query,
        body,
    )
    .await
}

/// `/hub/vitals/{*rest}` — vitals (WHOOP dashboard). Read-only, except the
/// journal endpoint: the launcher's "marcar" widget logs context marks
/// (eat/drink/stress/...) straight into the vitals journal.
pub async fn hub_vitals(
    State(state): State<AppState>,
    method: Method,
    Path(rest): Path<String>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let allowed: &[Method] = if rest == "journal" {
        &[Method::GET, Method::POST]
    } else {
        &[Method::GET]
    };
    let base = state.vitals_url.clone();
    proxy(&state, &base, None, None, allowed, method, &rest, query, body).await
}

/// `/hub/polybot/{*rest}` — polybot panel. Read-only; the launcher chip only
/// pulls `/widget` (account total + today), same tailnet exposure as the
/// panel itself on :3006.
pub async fn hub_polybot(
    State(state): State<AppState>,
    method: Method,
    Path(rest): Path<String>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let base = state.polybot_url.clone();
    proxy(
        &state,
        &base,
        None,
        None,
        &[Method::GET],
        method,
        &rest,
        query,
        body,
    )
    .await
}

/// `/hub/hermes/{*rest}` — hermeshub. GET everywhere; POST only on the
/// quick-prompt whitelist above.
pub async fn hub_hermes(
    State(state): State<AppState>,
    method: Method,
    Path(rest): Path<String>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let allowed: &[Method] = if hermes_post_allowed(&rest) {
        &[Method::GET, Method::POST]
    } else {
        &[Method::GET]
    };
    let base = state.hermeshub_url.clone();
    proxy(&state, &base, None, None, allowed, method, &rest, query, body).await
}

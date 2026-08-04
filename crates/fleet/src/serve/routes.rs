//! Axum route handlers for `fleet serve` (spec §3.8).
//!
//! Each handler opens a read-only SQLite connection, loads data, and
//! serializes via the `export::build_*` builders — same shapes as
//! `fleet list --json`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::model::{DedupeKind, Tier, is_online};
use crate::serve::templates;
use crate::{db, export};

/// Shared state for the handlers.
#[derive(Clone)]
pub struct AppState {
    /// Path to the SQLite registry file (opened read-only per request).
    pub db_path: PathBuf,
    /// Freshness window for the **derived** `online` field (spec §3.3). The HTML
    /// views recompute online at request time rather than trusting the stored flag.
    pub online_threshold: Duration,
    /// Age threshold after which a host snapshot is considered stale (spec §6.5).
    /// Derived from `Config::snapshot_stale_secs` in `run_with`; defaults to 3 h.
    pub snapshot_stale_threshold: Duration,
    /// Deep-drill-down link target for `/observability` (NOT polled — R-10).
    pub beszel_ui_url: String,
    /// Deep-drill-down link target for `/observability` (NOT polled — R-10).
    pub kuma_ui_url: String,
    /// Curated port→service-name overrides, loaded once at startup (spec: port
    /// service naming). Wrapped in `Arc` so `AppState` stays cheap to `Clone`.
    pub labels: std::sync::Arc<crate::service_label::Labels>,
    /// The caguastore app catalog (built-in default or `store.toml` override),
    /// loaded once at startup.
    pub store: std::sync::Arc<crate::store::Catalog>,
    /// Shared HTTP client for the `/hub/*` proxy (reqwest clients are cheap to
    /// clone — internal Arc).
    pub http: reqwest::Client,
    /// Command Center base URL (`/hub/cc/*` upstream).
    pub cc_url: String,
    /// cuentas base URL (`/hub/cuentas/*` upstream).
    pub cuentas_url: String,
    /// `user:pass` Basic credential presented to cuentas upstream.
    pub cuentas_basic_auth: Option<String>,
    /// hermeshub base URL (`/hub/hermes/*` upstream).
    pub hermeshub_url: String,
    /// vitals base URL (`/hub/vitals/*` upstream).
    pub vitals_url: String,
    /// polybot panel base URL (`/hub/polybot/*` upstream).
    pub polybot_url: String,
    /// portfolio base URL (`/hub/portfolio/*` upstream, money-PIN-gated).
    pub portfolio_url: String,
    /// Bearer token for the portfolio upstream.
    pub portfolio_token: Option<String>,
    /// PIN for the money proxy (`X-Money-Pin` header). None → proxy disabled.
    pub money_pin: Option<String>,
    /// Home-screen ticker watchlist (`SYMBOL` or `SYMBOL:label`), served
    /// ungated by `/api/tickers` — public prices carry no holding sizes.
    pub tickers: std::sync::Arc<Vec<String>>,
}

// ── format helpers ────────────────────────────────────────────────────────────

fn fmt_bytes(bytes: i64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let kb = bytes as f64 / 1024.0;
    if kb < 1024.0 {
        return format!("{kb:.1} KB");
    }
    let mb = kb / 1024.0;
    if mb < 1024.0 {
        return format!("{mb:.1} MB");
    }
    let gb = mb / 1024.0;
    format!("{gb:.1} GB")
}

fn fmt_pct(pct: f64) -> String {
    format!("{pct:.1}%")
}

fn truncate80(s: &str) -> String {
    if s.len() <= 80 {
        s.to_owned()
    } else {
        // Char-safe: slice on a UTF-8 boundary so a multi-byte char near byte 79
        // can't panic (a malicious/odd command line could carry non-ASCII).
        let end = s.char_indices().nth(79).map(|(i, _)| i).unwrap_or(s.len());
        format!("{}…", &s[..end])
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Open a read-only connection and return a 500 on failure.
fn ro_conn(state: &AppState) -> Result<rusqlite::Connection, (StatusCode, String)> {
    super::open_readonly(&state.db_path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("db open failed: {e:#}"),
        )
    })
}

// ── GET /api/fleet ────────────────────────────────────────────────────────────

pub async fn get_fleet(State(state): State<AppState>) -> impl IntoResponse {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    match db::nodes::list(&conn) {
        Ok(nodes) => Json(export::build_fleet_json(&nodes)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("db query failed: {e:#}"),
        )
            .into_response(),
    }
}

// ── GET /api/node/{id} ────────────────────────────────────────────────────────

pub async fn get_node(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    match db::nodes::get(&conn, &id) {
        Ok(Some(node)) => {
            // Reuse the per-node projection from build_fleet_json
            let fleet = export::build_fleet_json(&[node]);
            let node_export = fleet.nodes.into_iter().next().unwrap();
            Json(node_export).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("db query failed: {e:#}"),
        )
            .into_response(),
    }
}

// ── GET /api/path-health ──────────────────────────────────────────────────────

pub async fn get_path_health(State(state): State<AppState>) -> impl IntoResponse {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    match db::probe::latest_paths(&conn) {
        Ok(paths) => {
            let hops: Vec<serde_json::Value> = paths
                .into_iter()
                .map(|p| {
                    serde_json::json!({
                        "target_name": p.target_name,
                        "target_addr": p.target_addr,
                        "path_type": p.path_type,
                        "dest_host": p.dest_host,
                        "dest_loss_pct": p.dest_loss_pct,
                        "dest_avg_ms": p.dest_avg_ms,
                        "dest_severity": p.dest_severity,
                    })
                })
                .collect();
            Json(export::build_path_health_json(&hops)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("db query failed: {e:#}"),
        )
            .into_response(),
    }
}

// ── GET /api/cf ──────────────────────────────────────────────────────────────

pub async fn get_cf(State(state): State<AppState>) -> impl IntoResponse {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    match db::cf::list_cf_zones(&conn) {
        Ok(zones) => {
            let zone_values: Vec<serde_json::Value> = zones
                .into_iter()
                .map(|z| {
                    serde_json::json!({
                        "id": z.id,
                        "name": z.name,
                        "status": z.status,
                        "paused": z.paused,
                        "healthy": z.healthy,
                        "min_cert_expiry": z.min_cert_expiry
                            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
                    })
                })
                .collect();
            Json(export::build_cf_json(&zone_values)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("db query failed: {e:#}"),
        )
            .into_response(),
    }
}

// ════════════════════════════════════════════════════════════════════════════
//  HTML views (askama, server-rendered) — spec §3.8
// ════════════════════════════════════════════════════════════════════════════

/// 500 helper for HTML handlers (DB errors).
fn html_500(e: impl std::fmt::Display) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("error: {e:#}")).into_response()
}

fn tier_str(t: Tier) -> &'static str {
    match t {
        Tier::Agent => "agent",
        Tier::Agentless => "agentless",
    }
}

fn dedupe_str(k: DedupeKind) -> &'static str {
    match k {
        DedupeKind::Machinekey => "machinekey",
        DedupeKind::Alias => "alias",
        DedupeKind::Fuzzy => "fuzzy",
    }
}

/// Build the inventory rows from the DB, recomputing `online` from `last_seen`
/// freshness (never the stored flag) and flagging fuzzy-merged rows.
fn inventory_rows(
    state: &AppState,
    conn: &rusqlite::Connection,
) -> anyhow::Result<Vec<templates::InventoryRow>> {
    let nodes = db::nodes::list(conn)?;
    Ok(nodes
        .into_iter()
        .map(|n| templates::InventoryRow {
            online: is_online(n.last_seen, state.online_threshold),
            fuzzy: n.dedupe_key_kind == DedupeKind::Fuzzy,
            tier: tier_str(n.tier).to_owned(),
            site: n.tags.site.unwrap_or_default(),
            role: n.tags.role.unwrap_or_default(),
            owner: n.tags.owner.unwrap_or_default(),
            last_seen: n.last_seen.format("%Y-%m-%d %H:%M").to_string(),
            fleet_id: n.fleet_id,
            hostname: n.hostname,
        })
        .collect())
}

// ── GET / (caguastore launcher) ──────────────────────────────────────────────

/// Glyph keys present in the `store.html` sprite. A catalog entry with any
/// other `icon` value renders the generic `app` glyph instead of a broken ref.
const STORE_ICONS: &[&str] = &[
    "spade", "mountain", "hold", "cap", "kanban", "coin", "pulse", "gauge", "bell", "app", "term",
    "code", "mesh", "calendar", "plane", "chart", "bot", "sun", "hand", "door", "speech", "house",
    "camera", "map", "music",
];

/// The launcher home screen. Liveness LED per app: its catalog `port` appears
/// in a **non-stale** host_port row (any node — every catalog app lives on
/// caguaserver today; revisit if apps spread across hosts).
pub async fn get_store(State(state): State<AppState>) -> Response {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    // Fresh listening ports across the fleet, with the node they were seen on
    // (hostname lowercased for the catalog's substring match).
    let fresh_ports: Vec<(String, u16)> = db::host::all_ports(&conn)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| !crate::model::is_stale(&r.collected_at, state.snapshot_stale_threshold))
        .map(|r| (r.hostname.to_lowercase(), r.port))
        .collect();

    // Group tiles into launcher sections by catalog `category`, preserving
    // catalog order (a category's position = its first tile's position).
    let mut groups: Vec<templates::StoreGroup> = Vec::new();
    let mut led_count = 0;
    let mut up_count = 0;
    for (idx, a) in state.store.apps.iter().enumerate() {
        let icon = if STORE_ICONS.contains(&a.icon.as_str()) {
            a.icon.clone()
        } else {
            "app".to_owned()
        };
        let tile = templates::StoreTile {
            slug: a.slug.clone(),
            name: a.name.clone(),
            tagline: a.tagline.clone(),
            url: a.url.clone(),
            icon,
            hue: a.hue,
            has_led: a.port.is_some(),
            up: a.port.is_some_and(|p| {
                let host = a.host.as_deref().map(str::to_lowercase);
                fresh_ports.iter().any(|(h, port)| {
                    *port == p && host.as_deref().is_none_or(|needle| h.contains(needle))
                })
            }),
            private: a.private,
            idx,
        };
        led_count += usize::from(tile.has_led);
        up_count += usize::from(tile.up);
        match groups.iter_mut().find(|g| g.title == a.category) {
            Some(g) => g.tiles.push(tile),
            None => groups.push(templates::StoreGroup {
                title: a.category.clone(),
                tiles: vec![tile],
            }),
        }
    }

    templates::render(&templates::StorePage {
        groups,
        up_count,
        led_count,
    })
}

// ── GET /api/news (breaking headlines for the launcher's news widget) ────────

/// Minimal XML entity unescape for RSS titles.
fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

/// Pull `<tag>…</tag>` out of an XML fragment (no attribute handling — Google
/// News RSS item titles/dates are plain elements).
fn xml_tag<'a>(frag: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = frag.find(&open)? + open.len();
    let end = frag[start..].find(&close)? + start;
    Some(&frag[start..end])
}

/// Headlines, cached ~10 min so a fleet of phone opens doesn't hammer the feed.
static NEWS_CACHE: std::sync::OnceLock<std::sync::Mutex<Option<(std::time::Instant, String)>>> =
    std::sync::OnceLock::new();

/// `GET /api/news` — top headlines (Google News RSS es-MX), parsed server-side
/// because the feed has no CORS. `[{title, source, ts}]`, newest first.
pub async fn get_news(State(state): State<AppState>) -> Response {
    let cache = NEWS_CACHE.get_or_init(|| std::sync::Mutex::new(None));
    if let Some((at, body)) = cache.lock().unwrap().clone()
        && at.elapsed() < Duration::from_secs(600)
    {
        return ([(axum::http::header::CONTENT_TYPE, "application/json")], body).into_response();
    }

    let feed = "https://news.google.com/rss?hl=es-419&gl=MX&ceid=MX:es-419";
    let xml = match state
        .http
        .get(feed)
        .timeout(Duration::from_secs(6))
        .send()
        .await
    {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                format!("{{\"error\":\"news feed unreachable: {e}\"}}"),
            )
                .into_response();
        }
    };

    let mut items: Vec<serde_json::Value> = Vec::new();
    for frag in xml.split("<item>").skip(1).take(8) {
        let Some(raw_title) = xml_tag(frag, "title") else { continue };
        let title = xml_unescape(raw_title.trim());
        // Google News appends " - Source" to every headline.
        let (headline, source) = match title.rsplit_once(" - ") {
            Some((h, s)) => (h.to_owned(), s.to_owned()),
            None => (title, String::new()),
        };
        let ts = xml_tag(frag, "pubDate").map(str::trim).unwrap_or("");
        let link = xml_tag(frag, "link").map(str::trim).unwrap_or("");
        items.push(serde_json::json!({
            "title": headline,
            "source": source,
            "ts": ts,
            "link": link,
        }));
    }

    let body = serde_json::json!({ "items": items }).to_string();
    *cache.lock().unwrap() = Some((std::time::Instant::now(), body.clone()));
    ([(axum::http::header::CONTENT_TYPE, "application/json")], body).into_response()
}

// ── GET /api/quotes (live market quotes for the portfolio ticker board) ──────

/// Quotes cache: 5 min per normalized symbol set. Market data is public; the
/// PRIVATE part (which tickers the owner holds) only reaches this endpoint
/// after the client has unlocked the portfolio with the money PIN.
static QUOTES_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, String)>>,
> = std::sync::OnceLock::new();

/// Percent-encode the two non-alphanumerics Yahoo symbols carry that a URL path
/// segment can't take literally: `^` (indices, `^GSPC`) and `=` is path-safe but
/// encoded too so the whole segment is unambiguous.
fn encode_symbol(sym: &str) -> String {
    sym.chars()
        .map(|c| match c {
            '^' => "%5E".to_owned(),
            '=' => "%3D".to_owned(),
            _ => c.to_string(),
        })
        .collect()
}

/// Fetch one symbol's Yahoo v8 chart meta → quote JSON (None on any miss).
async fn fetch_quote(http: reqwest::Client, symbol: String) -> Option<serde_json::Value> {
    let path = encode_symbol(&symbol);
    let url =
        format!("https://query1.finance.yahoo.com/v8/finance/chart/{path}?interval=1d&range=2d");
    let resp = http
        .get(&url)
        .header(axum::http::header::USER_AGENT.as_str(), "Mozilla/5.0")
        .timeout(Duration::from_secs(6))
        .send()
        .await
        .ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    let meta = v.pointer("/chart/result/0/meta")?;
    let price = meta.get("regularMarketPrice")?.as_f64()?;
    let prev = meta.get("chartPreviousClose").and_then(|x| x.as_f64());
    let change_pct = prev
        .filter(|p| *p > 0.0)
        .map(|p| (price - p) / p * 100.0);
    let exchange = meta
        .get("exchangeName")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    // Trading now? Crypto (CCC) runs 24/7; otherwise inside the regular session.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let regular = meta.pointer("/currentTradingPeriod/regular");
    let in_session = regular
        .and_then(|r| {
            let start = r.get("start")?.as_i64()?;
            let end = r.get("end")?.as_i64()?;
            Some(now >= start && now < end)
        })
        .unwrap_or(false);
    let trading = exchange == "CCC" || in_session;
    Some(serde_json::json!({
        "symbol": meta.get("symbol").and_then(|x| x.as_str()).unwrap_or(&symbol),
        "price": price,
        "prevClose": prev,
        "changePct": change_pct,
        "trading": trading,
        "crypto": exchange == "CCC",
        "currency": meta.get("currency").and_then(|x| x.as_str()).unwrap_or(""),
    }))
}

/// Parse a comma-separated symbol list: uppercased, deduped, ≤20 entries.
/// `=` and `^` are allowed on purpose — without them FX pairs (`USDMXN=X`),
/// futures (`BZ=F`) and indices (`^GSPC`) are silently dropped, which is what
/// kept the peso off the board.
fn parse_symbols(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in raw.split(',') {
        let s = s.trim().to_uppercase();
        let ok = !s.is_empty()
            && s.len() <= 12
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '=' | '^'));
        if ok && !out.contains(&s) {
            out.push(s);
        }
        if out.len() == 20 {
            break;
        }
    }
    out
}

/// `GET /api/quotes?symbols=RTX,XLE,BTC-USD` — batched Yahoo quotes with day
/// change and an is-it-trading flag. Cached 5 min per symbol set, ≤20 symbols.
pub async fn get_quotes(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let raw = params.get("symbols").cloned().unwrap_or_default();
    let symbols = parse_symbols(&raw);
    if symbols.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            r#"{"error":"symbols required"}"#.to_owned(),
        )
            .into_response();
    }

    let key = symbols.join(",");
    let cache = QUOTES_CACHE.get_or_init(Default::default);
    if let Some((at, body)) = cache.lock().unwrap().get(&key).cloned()
        && at.elapsed() < Duration::from_secs(300)
    {
        return ([(axum::http::header::CONTENT_TYPE, "application/json")], body).into_response();
    }

    let tasks: Vec<_> = symbols
        .iter()
        .map(|sym| fetch_quote(state.http.clone(), sym.clone()))
        .collect();
    let results = futures_util::future::join_all(tasks).await;
    let quotes: Vec<serde_json::Value> = results.into_iter().flatten().collect();

    let body = serde_json::json!({ "quotes": quotes }).to_string();
    cache
        .lock()
        .unwrap()
        .insert(key, (std::time::Instant::now(), body.clone()));
    ([(axum::http::header::CONTENT_TYPE, "application/json")], body).into_response()
}

// ── GET /api/tickers (home-screen watchlist — public data, no PIN) ──────────

/// `GET /api/tickers` — the configured home watchlist (`[serve] tickers`),
/// quoted and labelled. Deliberately **ungated**: it carries prices and day
/// moves only, never a position size, so nothing here is worth a PIN. The
/// PIN-gated `mercado` screen remains the place holdings values show up.
pub async fn get_tickers(State(state): State<AppState>) -> Response {
    // Entries are `SYMBOL` or `SYMBOL:label`; the label is display-only.
    let mut labels: Vec<(String, String)> = Vec::new();
    for entry in state.tickers.iter() {
        let (sym, label) = match entry.split_once(':') {
            Some((s, l)) => (s.trim(), l.trim()),
            None => (entry.trim(), ""),
        };
        let parsed = parse_symbols(sym);
        if let Some(s) = parsed.into_iter().next() {
            let label = if label.is_empty() {
                s.clone()
            } else {
                label.to_owned()
            };
            labels.push((s, label));
        }
    }
    if labels.is_empty() {
        return (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            r#"{"tickers":[]}"#.to_owned(),
        )
            .into_response();
    }

    let key = format!("tickers:{}", labels.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>().join(","));
    let cache = QUOTES_CACHE.get_or_init(Default::default);
    if let Some((at, body)) = cache.lock().unwrap().get(&key).cloned()
        && at.elapsed() < Duration::from_secs(300)
    {
        return ([(axum::http::header::CONTENT_TYPE, "application/json")], body).into_response();
    }

    let tasks: Vec<_> = labels
        .iter()
        .map(|(sym, _)| fetch_quote(state.http.clone(), sym.clone()))
        .collect();
    let results = futures_util::future::join_all(tasks).await;

    // Keep catalog order and drop misses, so the widget never renders a hole.
    let tickers: Vec<serde_json::Value> = labels
        .iter()
        .zip(results)
        .filter_map(|((_, label), quote)| {
            let mut q = quote?;
            if let Some(obj) = q.as_object_mut() {
                obj.insert("label".to_owned(), serde_json::Value::String(label.clone()));
            }
            Some(q)
        })
        .collect();

    let body = serde_json::json!({ "tickers": tickers }).to_string();
    cache
        .lock()
        .unwrap()
        .insert(key, (std::time::Instant::now(), body.clone()));
    ([(axum::http::header::CONTENT_TYPE, "application/json")], body).into_response()
}

// ── GET /api/rss (generalized RSS/Atom proxy for launcher widgets) ───────────

/// Per-feed cache: 10 min per URL, same pattern as `QUOTES_CACHE`.
static RSS_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, String)>>,
> = std::sync::OnceLock::new();

/// Validate a caller-supplied feed URL: `https://` only, no userinfo, sane
/// length, and never a loopback/private/tailnet host — this proxies the public
/// internet, not the tailnet.
fn rss_url_ok(url: &str) -> bool {
    if !url.starts_with("https://") || url.len() > 300 || url.contains('@') {
        return false;
    }
    let host = url["https://".len()..]
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    let internal = host.starts_with("localhost")
        || host.starts_with("127.")
        || host.starts_with("10.")
        || host.starts_with("192.168.")
        || host.starts_with("100.")
        || host.ends_with(".ts.net");
    !internal
}

/// Pull the first `href="…"` attribute value out of a fragment (Atom `<link>`
/// elements are self-closing, so `xml_tag` can't see them).
fn atom_href(frag: &str) -> Option<&str> {
    let start = frag.find("href=\"")? + "href=\"".len();
    let end = frag[start..].find('"')? + start;
    Some(&frag[start..end])
}

/// `GET /api/rss?url=<feed>` — server-side fetch + parse of an arbitrary public
/// RSS/Atom feed (feeds have no CORS). `{feed, items: [{title, link, ts}]}`,
/// max 12 items, cached 10 min per URL.
pub async fn get_rss(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let url = params.get("url").map(|s| s.trim().to_owned()).unwrap_or_default();
    if !rss_url_ok(&url) {
        return (
            StatusCode::BAD_REQUEST,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            r#"{"error":"url must be a public https:// feed (max 300 chars, no userinfo, no local/tailnet hosts)"}"#
                .to_owned(),
        )
            .into_response();
    }

    let cache = RSS_CACHE.get_or_init(Default::default);
    if let Some((at, body)) = cache.lock().unwrap().get(&url).cloned()
        && at.elapsed() < Duration::from_secs(600)
    {
        return ([(axum::http::header::CONTENT_TYPE, "application/json")], body).into_response();
    }

    let raw = match state
        .http
        .get(&url)
        .header(axum::http::header::USER_AGENT.as_str(), "Mozilla/5.0")
        .timeout(Duration::from_secs(6))
        .send()
        .await
    {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                format!("{{\"error\":\"feed unreachable: {e}\"}}"),
            )
                .into_response();
        }
    };
    // Cap the body at 512 KB (char-boundary-safe) before parsing.
    let mut cap = 512 * 1024;
    let xml = if raw.len() > cap {
        while !raw.is_char_boundary(cap) {
            cap -= 1;
        }
        &raw[..cap]
    } else {
        raw.as_str()
    };

    // RSS `<item>` fragments first; fall back to Atom `<entry>`.
    let mut frags: Vec<&str> = xml.split("<item>").skip(1).collect();
    if frags.is_empty() {
        frags = xml.split("<entry>").skip(1).collect();
    }

    let mut items: Vec<serde_json::Value> = Vec::new();
    for frag in frags.into_iter().take(12) {
        let title = xml_tag(frag, "title")
            .map(|t| {
                let t = t.trim();
                let t = t
                    .strip_prefix("<![CDATA[")
                    .and_then(|x| x.strip_suffix("]]>"))
                    .unwrap_or(t);
                xml_unescape(t.trim())
            })
            .unwrap_or_default();
        // RSS: plain `<link>text</link>`. Atom: self-closing `<link href="…"/>`.
        let link = match xml_tag(frag, "link").map(str::trim).filter(|l| !l.is_empty()) {
            Some(l) => l.to_owned(),
            None => atom_href(frag).unwrap_or_default().to_owned(),
        };
        let ts = xml_tag(frag, "pubDate")
            .or_else(|| xml_tag(frag, "updated"))
            .or_else(|| xml_tag(frag, "published"))
            .map(str::trim)
            .unwrap_or("");
        items.push(serde_json::json!({
            "title": title,
            "link": link,
            "ts": ts,
        }));
    }

    let body = serde_json::json!({ "feed": url, "items": items }).to_string();
    cache
        .lock()
        .unwrap()
        .insert(url, (std::time::Instant::now(), body.clone()));
    ([(axum::http::header::CONTENT_TYPE, "application/json")], body).into_response()
}

// ── GET /board (kanban over the Command Center) ──────────────────────────────

/// The task board — a static shell; all data flows through `/hub/cc/*` from
/// the browser (the Command Center stays the single source of truth).
pub async fn get_board() -> Response {
    templates::render(&templates::BoardPage {})
}

// ── GET /inventory (mirrors `fleet list`) ────────────────────────────────────

/// `?partial=1` returns just the `<table>` fragment for HTMX `hx-get` refresh.
pub async fn get_index(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let rows = match inventory_rows(&state, &conn) {
        Ok(r) => r,
        Err(e) => return html_500(e),
    };

    if params.get("partial").is_some_and(|v| v == "1") {
        templates::render(&templates::InventoryTable { rows })
    } else {
        templates::render(&templates::InventoryPage { rows })
    }
}

// ── GET /node/{id} (detail, mirrors `fleet show`) ────────────────────────────

pub async fn get_node_html(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    let node = match db::nodes::get(&conn, &id) {
        Ok(Some(n)) => n,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return html_500(e),
    };

    // `get` does not populate seen_in — load it from node_seen.
    let seen_in = match db::nodes::load_seen_in(&conn, &node.fleet_id) {
        Ok(s) => s,
        Err(e) => return html_500(e),
    };

    let host_snapshot = match db::host::latest_for_node(&conn, &node.fleet_id) {
        Ok(Some(hs)) => {
            let node_cmds =
                db::host::commands_by_pid_for_node(&conn, &node.fleet_id).unwrap_or_default();
            let ports = db::host::ports_for_node(&conn, &node.fleet_id)
                .unwrap_or_default()
                .into_iter()
                .map(|p| templates::HostPortRow {
                    service: crate::service_label::resolve_service(
                        p.port,
                        node_cmds.get(&p.pid).map(String::as_str),
                        &p.process,
                        &state.labels,
                    ),
                    port: p.port,
                    proto: p.proto,
                    process: p.process,
                    pid: p.pid,
                    bind: p.bind,
                })
                .collect();
            let workloads_db =
                db::host::workloads_for_node(&conn, &node.fleet_id).unwrap_or_default();
            let rendered_count = workloads_db.len() as i64;
            let workloads = workloads_db
                .into_iter()
                .map(|w| templates::HostWorkloadRow {
                    label: w.label,
                    category: w.category,
                    process_count: w.process_count,
                    cpu_pct: fmt_pct(w.total_cpu_percent),
                    mem_human: fmt_bytes(w.total_memory_bytes),
                    example_command: truncate80(&w.example_command),
                })
                .collect();
            let showing_top_n_note = if hs.workload_count > rendered_count {
                Some(format!(
                    "showing top {} of {}",
                    rendered_count, hs.workload_count
                ))
            } else {
                None
            };
            Some(templates::HostSnapshotView {
                collected_at: hs.collected_at.clone(),
                stale: crate::model::is_stale(&hs.collected_at, state.snapshot_stale_threshold),
                cpu_pct: fmt_pct(hs.total_cpu_percent),
                mem_used: fmt_bytes(hs.used_memory_bytes),
                mem_total: fmt_bytes(hs.total_memory_bytes),
                gpu_pct: hs.gpu_percent.map(fmt_pct),
                ports,
                workloads,
                workload_count: hs.workload_count,
                showing_top_n_note,
            })
        }
        Ok(None) => None,
        Err(e) => return html_500(e),
    };

    let page = templates::NodePage {
        online: is_online(node.last_seen, state.online_threshold),
        tier: tier_str(node.tier).to_owned(),
        dedupe_key_kind: dedupe_str(node.dedupe_key_kind).to_owned(),
        role: node.tags.role.clone().unwrap_or_default(),
        owner: node.tags.owner.clone().unwrap_or_default(),
        site: node.tags.site.clone().unwrap_or_default(),
        gpu: node.tags.gpu.clone().unwrap_or_default(),
        last_seen: node.last_seen.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        addresses: node.addresses.clone(),
        seen_in: seen_in
            .into_iter()
            .map(|s| templates::SeenInRow {
                account: s.account,
                device_id: s.device_id,
            })
            .collect(),
        raw_tags: node.tags.raw.clone(),
        notes: node.notes.clone(),
        fleet_id: node.fleet_id,
        hostname: node.hostname,
        fqdn: node.fqdn,
        os: node.os,
        host_snapshot,
    };
    templates::render(&page)
}

// ── GET /paths (MTR path health) ─────────────────────────────────────────────

pub async fn get_paths_html(State(state): State<AppState>) -> Response {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    let paths = match db::probe::latest_paths(&conn) {
        Ok(p) => p,
        Err(e) => return html_500(e),
    };

    let page = templates::PathsPage {
        paths: paths
            .into_iter()
            .map(|p| templates::PathRow {
                target_name: p.target_name,
                target_addr: p.target_addr,
                path_type: p.path_type,
                dest_host: p.dest_host,
                dest_loss_pct: p.dest_loss_pct,
                dest_avg_ms: p.dest_avg_ms,
                dest_severity: p.dest_severity,
            })
            .collect(),
    };
    templates::render(&page)
}

// ── GET /ports (fleet-wide listening ports) ──────────────────────────────────

pub async fn get_ports_html(State(state): State<AppState>) -> Response {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    let rows = match db::host::all_ports(&conn) {
        Ok(r) => r,
        Err(e) => return html_500(e),
    };
    let cmds = db::host::commands_by_pid_all(&conn).unwrap_or_default();

    let page = templates::PortsPage {
        rows: rows
            .into_iter()
            .map(|r| {
                let command = cmds
                    .get(&r.node_id)
                    .and_then(|m| m.get(&r.pid))
                    .map(String::as_str);
                templates::FleetPortViewRow {
                    service: crate::service_label::resolve_service(
                        r.port,
                        command,
                        &r.process,
                        &state.labels,
                    ),
                    fleet_id: r.node_id,
                    hostname: r.hostname,
                    port: r.port,
                    proto: r.proto,
                    process: r.process,
                    pid: r.pid,
                    bind: r.bind,
                    collected_at: r.collected_at.clone(),
                    stale: crate::model::is_stale(&r.collected_at, state.snapshot_stale_threshold),
                }
            })
            .collect(),
    };
    templates::render(&page)
}

// ── GET /workloads (fleet-wide AI workloads) ──────────────────────────────────

pub async fn get_workloads_html(State(state): State<AppState>) -> Response {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    let rows = match db::host::all_workloads(&conn) {
        Ok(r) => r,
        Err(e) => return html_500(e),
    };

    // For each node, we need to know how many workload rows were rendered
    // to decide if "showing top N of M" note applies.
    // Group by node_id to count rendered rows per node.
    let mut node_rendered_counts: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    for r in &rows {
        *node_rendered_counts.entry(r.node_id.clone()).or_insert(0) += 1;
    }

    let page = templates::WorkloadsPage {
        rows: rows
            .into_iter()
            .map(|r| {
                let rendered_for_node = node_rendered_counts.get(&r.node_id).copied().unwrap_or(0);
                let showing_top_n_note = if r.workload_count > rendered_for_node {
                    Some(format!(
                        "showing top {} of {}",
                        rendered_for_node, r.workload_count
                    ))
                } else {
                    None
                };
                templates::FleetWorkloadViewRow {
                    fleet_id: r.node_id,
                    hostname: r.hostname,
                    label: r.label,
                    category: r.category,
                    process_count: r.process_count,
                    cpu_pct: fmt_pct(r.total_cpu_percent),
                    mem_human: fmt_bytes(r.total_memory_bytes),
                    example_command: truncate80(&r.example_command),
                    collected_at: r.collected_at.clone(),
                    stale: crate::model::is_stale(&r.collected_at, state.snapshot_stale_threshold),
                    showing_top_n_note,
                }
            })
            .collect(),
    };
    templates::render(&page)
}

// ── GET /api/ports ────────────────────────────────────────────────────────────

pub async fn get_api_ports(State(state): State<AppState>) -> impl IntoResponse {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    match db::host::all_ports(&conn) {
        Ok(rows) => Json(export::build_ports_json(
            &rows,
            state.snapshot_stale_threshold,
        ))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("db query failed: {e:#}"),
        )
            .into_response(),
    }
}

// ── GET /api/workloads ────────────────────────────────────────────────────────

pub async fn get_api_workloads(State(state): State<AppState>) -> impl IntoResponse {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    match db::host::all_workloads(&conn) {
        Ok(rows) => Json(export::build_workloads_json(
            &rows,
            state.snapshot_stale_threshold,
        ))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("db query failed: {e:#}"),
        )
            .into_response(),
    }
}

// ── GET /observability (CF zones + links-out + online rollup) ────────────────

pub async fn get_observability_html(State(state): State<AppState>) -> Response {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    // Registry-derived online rollup (R-10: NEVER from Kuma socket.io).
    let nodes = match db::nodes::list(&conn) {
        Ok(n) => n,
        Err(e) => return html_500(e),
    };
    let total_count = nodes.len();
    let online_count = nodes
        .iter()
        .filter(|n| is_online(n.last_seen, state.online_threshold))
        .count();
    let offline_count = total_count - online_count;

    let zones = match db::cf::list_cf_zones(&conn) {
        Ok(z) => z,
        Err(e) => return html_500(e),
    };
    let zone_rows = zones
        .into_iter()
        .map(|z| templates::ZoneRow {
            cert_expiry: z
                .min_cert_expiry
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "—".to_owned()),
            name: z.name,
            status: z.status,
            healthy: z.healthy,
        })
        .collect();

    let page = templates::ObservabilityPage {
        online_count,
        offline_count,
        total_count,
        beszel_ui_url: state.beszel_ui_url.clone(),
        kuma_ui_url: state.kuma_ui_url.clone(),
        zones: zone_rows,
    };
    templates::render(&page)
}

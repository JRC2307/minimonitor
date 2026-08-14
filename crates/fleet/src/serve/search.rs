//! `GET /api/search` — one fleet-wide search across the four things worth
//! finding from the launcher: **apps**, **tasks**, **hermes messages**, and the
//! **vault**.
//!
//! Everything is resolved server-side. fleet-serve runs on caguaserver, so the
//! Command Center (:8787), hermeshub (:8796) and the Syncthing'd Obsidian vault
//! are all loopback/local from here — the browser never has to reach them (and,
//! served over Tailscale HTTPS, could not).
//!
//! Shape (groups always present, always in this order):
//!
//! ```json
//! {"query":"…","groups":[
//!   {"kind":"apps",   "hits":[{"title","subtitle","url","slug","icon","private"}]},
//!   {"kind":"tasks",  "hits":[{"title","subtitle","url","id","project","status"}]},
//!   {"kind":"hermes", "hits":[{"title","subtitle","url","channel","ts"}]},
//!   {"kind":"vault",  "hits":[{"title","subtitle","path"}]}
//! ]}
//! ```
//!
//! A group that failed carries an extra `"error"` string and an empty `hits`.
//!
//! Policy:
//! - GET only, no PIN — same exposure as `/` and `/api/store`.
//! - **Never** touches the money proxies (cuentas, portfolio): those are
//!   PIN-gated for a reason and an ungated search endpoint is exactly the hole
//!   that would undo it. The apps group can surface a money *tile* (so can
//!   `/api/store`), flagged `private`, but never a number from inside one.
//! - Async fan-out with a per-source timeout, so one dead sibling degrades its
//!   own group instead of blanking the response.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};

use super::routes::{AppState, STORE_ICONS};

/// Per-source budget. One dead source must never cost the whole response.
const SOURCE_TIMEOUT: Duration = Duration::from_millis(2000);
/// HTTP timeouts inside a source, kept under `SOURCE_TIMEOUT`.
const HTTP_TIMEOUT: Duration = Duration::from_millis(1500);
const HERMES_CHANNELS_TIMEOUT: Duration = Duration::from_millis(700);
const HERMES_MESSAGES_TIMEOUT: Duration = Duration::from_millis(1100);

/// Bounds. Search is a convenience, not an index — every scan is capped so a
/// pathological query can't turn into a fleet-wide grep.
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 50;
/// Channels scanned per query (hermeshub returns them newest-active first).
const HERMES_MAX_CHANNELS: usize = 12;
/// Messages pulled per channel (hermeshub caps `limit` at 500 itself).
const HERMES_MESSAGES_PER_CHANNEL: usize = 80;
/// Vault files whose *content* we are willing to read in one query.
const VAULT_MAX_FILES: usize = 4000;
/// Skip anything bigger — a 1 MB markdown file is an attachment dump, not a note.
const VAULT_MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Snippet length for message/line subtitles.
const SNIPPET_CHARS: usize = 140;

// ── matching ─────────────────────────────────────────────────────────────────

/// Lowercase + strip Latin diacritics, so `café` / `CAFE` / `Café` all fold to
/// `cafe` and `niño` is findable as `nino`. Spanish content is the norm here,
/// and the owner types without accents — an accent-sensitive search would just
/// look broken. Hand-rolled rather than pulling a unicode-normalization crate:
/// the fleet's corpus is Latin-1-shaped.
pub(crate) fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        match c {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'ā' => out.push('a'),
            'é' | 'è' | 'ê' | 'ë' | 'ē' => out.push('e'),
            'í' | 'ì' | 'î' | 'ï' | 'ī' => out.push('i'),
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ō' => out.push('o'),
            'ú' | 'ù' | 'û' | 'ü' | 'ū' => out.push('u'),
            'ñ' => out.push('n'),
            'ç' => out.push('c'),
            'ý' | 'ÿ' => out.push('y'),
            _ => out.push(c),
        }
    }
    out
}

/// A folded, whitespace-split query. All terms must be present (AND) — typing
/// more words narrows, which is the behaviour a search box trains you to expect.
#[derive(Debug, Clone)]
pub(crate) struct Matcher {
    terms: Vec<String>,
}

impl Matcher {
    pub(crate) fn new(query: &str) -> Matcher {
        Matcher {
            terms: fold(query).split_whitespace().map(str::to_owned).collect(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Does an already-folded haystack contain every term?
    pub(crate) fn matches_folded(&self, folded: &str) -> bool {
        !self.terms.is_empty() && self.terms.iter().all(|t| folded.contains(t.as_str()))
    }

    /// Fold `hay` and match. Prefer [`Matcher::matches_folded`] in loops where
    /// the same haystack is tested repeatedly.
    pub(crate) fn matches(&self, hay: &str) -> bool {
        self.matches_folded(&fold(hay))
    }

    /// Rank: 0 = title starts with the first term, 1 = title contains it,
    /// 2 = matched only on secondary fields. Stable-sorted, so within a rank
    /// the source's own order (catalog order, board order) survives.
    fn rank(&self, title: &str) -> u8 {
        let Some(first) = self.terms.first() else {
            return 2;
        };
        let t = fold(title);
        if t.starts_with(first.as_str()) {
            0
        } else if t.contains(first.as_str()) {
            1
        } else {
            2
        }
    }
}

/// Char-safe truncation with an ellipsis, for message/line snippets.
fn snippet(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= SNIPPET_CHARS {
        return s.to_owned();
    }
    let end = s
        .char_indices()
        .nth(SNIPPET_CHARS)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    format!("{}…", &s[..end].trim_end())
}

/// Percent-encode a query-string value (channel names are slugs today, but the
/// API takes arbitrary names and a stray `&` would silently truncate the query).
fn encode_query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ── group envelope ───────────────────────────────────────────────────────────

/// One source's result. `error` is present only when the source failed, so a
/// client can tell "nothing matched" from "hermeshub is down".
struct Group {
    kind: &'static str,
    hits: Vec<serde_json::Value>,
    error: Option<String>,
}

impl Group {
    fn ok(kind: &'static str, hits: Vec<serde_json::Value>) -> Group {
        Group {
            kind,
            hits,
            error: None,
        }
    }

    fn failed(kind: &'static str, msg: impl std::fmt::Display) -> Group {
        Group {
            kind,
            hits: Vec::new(),
            error: Some(msg.to_string()),
        }
    }

    fn into_json(self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert("kind".to_owned(), serde_json::Value::from(self.kind));
        obj.insert("hits".to_owned(), serde_json::Value::Array(self.hits));
        if let Some(e) = self.error {
            obj.insert("error".to_owned(), serde_json::Value::String(e));
        }
        serde_json::Value::Object(obj)
    }
}

/// Run one source under the per-source budget, turning both a timeout and an
/// error into a `Group` carrying `error` rather than poisoning the response.
async fn bounded<F>(kind: &'static str, fut: F) -> Group
where
    F: std::future::Future<Output = Result<Vec<serde_json::Value>, String>>,
{
    match tokio::time::timeout(SOURCE_TIMEOUT, fut).await {
        Ok(Ok(hits)) => Group::ok(kind, hits),
        Ok(Err(e)) => Group::failed(kind, e),
        Err(_) => Group::failed(kind, "timed out"),
    }
}

// ── source: apps (in-memory catalog) ─────────────────────────────────────────

/// Match the caguastore catalog on name, slug, tagline and category. Purely
/// in-memory, so this group is the one that always answers.
pub(crate) fn search_apps(
    catalog: &crate::store::Catalog,
    m: &Matcher,
    limit: usize,
) -> Vec<serde_json::Value> {
    let mut scored: Vec<(u8, serde_json::Value)> = Vec::new();
    for a in catalog.apps.iter() {
        let hay = fold(&format!(
            "{} {} {} {}",
            a.name, a.slug, a.tagline, a.category
        ));
        if !m.matches_folded(&hay) {
            continue;
        }
        let icon = if STORE_ICONS.contains(&a.icon.as_str()) {
            a.icon.as_str()
        } else {
            "app"
        };
        scored.push((
            m.rank(&a.name),
            serde_json::json!({
                "title": a.name,
                "subtitle": a.tagline,
                "url": a.url,
                "slug": a.slug,
                "icon": icon,
                // Money tiles stay visible but flagged, exactly as `/api/store`
                // reports them — the PIN gates the data, not the tile's name.
                "private": a.private,
            }),
        ));
    }
    scored.sort_by_key(|(r, _)| *r);
    scored.into_iter().take(limit).map(|(_, v)| v).collect()
}

// ── source: tasks (Command Center, loopback) ─────────────────────────────────

/// `done` tasks are noise unless the query is *about* doneness — a search for
/// "vitals" wants the open work, a search for "done vitals" wants the archive.
fn query_wants_done(m: &Matcher) -> bool {
    m.terms
        .iter()
        .any(|t| t.len() >= 3 && "done".contains(t.as_str()))
}

/// Filter a Command Center `TaskOut[]` payload. Split out from the fetch so the
/// matching rules are testable without a live Command Center.
pub(crate) fn filter_tasks(
    tasks: &[serde_json::Value],
    m: &Matcher,
    limit: usize,
) -> Vec<serde_json::Value> {
    let wants_done = query_wants_done(m);
    let mut scored: Vec<(u8, serde_json::Value)> = Vec::new();
    for t in tasks {
        let title = t.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let status = t.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let project = t.get("project_name").and_then(|v| v.as_str()).unwrap_or("");
        if status == "done" && !wants_done {
            continue;
        }
        if !m.matches_folded(&fold(&format!("{title} {project} {status}"))) {
            continue;
        }
        scored.push((
            m.rank(title),
            serde_json::json!({
                "title": title,
                "subtitle": format!("{project} · {status}"),
                // The board is fleet-serve's own page and same-origin, so this
                // link works from the launcher over Tailscale HTTPS. The
                // Command Center's own UI has no per-task deep link to point at.
                "url": "/board",
                "id": t.get("id").cloned().unwrap_or(serde_json::Value::Null),
                "project": project,
                "status": status,
            }),
        ));
    }
    scored.sort_by_key(|(r, _)| *r);
    scored.into_iter().take(limit).map(|(_, v)| v).collect()
}

async fn search_tasks(
    http: reqwest::Client,
    cc_url: String,
    m: Matcher,
    limit: usize,
) -> Result<Vec<serde_json::Value>, String> {
    let url = format!("{}/api/tasks?project_id=all", cc_url.trim_end_matches('/'));
    let resp = http
        .get(&url)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("command center unreachable: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("command center returned {}", resp.status()));
    }
    let tasks: Vec<serde_json::Value> = resp
        .json()
        .await
        .map_err(|e| format!("command center payload unreadable: {e}"))?;
    Ok(filter_tasks(&tasks, &m, limit))
}

// ── source: hermes (hermeshub, loopback) ─────────────────────────────────────

/// hermeshub has no search endpoint — it has `/api/channels` and
/// `/api/messages?channel=…&limit=…`. So: list channels, pull a bounded recent
/// window from each in parallel, and match client-side. Bounded by
/// [`HERMES_MAX_CHANNELS`] × [`HERMES_MESSAGES_PER_CHANNEL`]; this is a
/// recent-history search, deliberately not a full-archive one.
async fn search_hermes(
    http: reqwest::Client,
    hermeshub_url: String,
    deep_link_base: String,
    m: Matcher,
    limit: usize,
) -> Result<Vec<serde_json::Value>, String> {
    let base = hermeshub_url.trim_end_matches('/').to_owned();
    let resp = http
        .get(format!("{base}/api/channels"))
        .timeout(HERMES_CHANNELS_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("hermeshub unreachable: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("hermeshub returned {}", resp.status()));
    }
    let channels: Vec<serde_json::Value> = resp
        .json()
        .await
        .map_err(|e| format!("hermeshub payload unreadable: {e}"))?;

    let names: Vec<String> = channels
        .iter()
        .filter_map(|c| c.get("name").and_then(|v| v.as_str()).map(str::to_owned))
        .take(HERMES_MAX_CHANNELS)
        .collect();

    let fetches = names.iter().map(|name| {
        let http = http.clone();
        let url = format!(
            "{base}/api/messages?channel={}&limit={HERMES_MESSAGES_PER_CHANNEL}",
            encode_query_value(name)
        );
        async move {
            let resp = http
                .get(&url)
                .timeout(HERMES_MESSAGES_TIMEOUT)
                .send()
                .await
                .ok()?;
            resp.json::<Vec<serde_json::Value>>().await.ok()
        }
    });
    let per_channel = futures_util::future::join_all(fetches).await;

    let mut hits: Vec<serde_json::Value> = Vec::new();
    for (name, msgs) in names.iter().zip(per_channel) {
        let Some(msgs) = msgs else { continue };
        // hermeshub returns oldest→newest; walk back so the freshest message in
        // a channel is the one that surfaces.
        for msg in msgs.iter().rev() {
            if hits.len() >= limit {
                break;
            }
            let text = msg.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if text.is_empty() || !m.matches(text) {
                continue;
            }
            hits.push(serde_json::json!({
                "title": name,
                "subtitle": snippet(text),
                "url": format!("{}/#c/{}", deep_link_base.trim_end_matches('/'), name),
                "channel": name,
                "ts": msg.get("ts").and_then(|v| v.as_str()).unwrap_or(""),
            }));
        }
        if hits.len() >= limit {
            break;
        }
    }
    Ok(hits)
}

/// Deep links must be the *user-facing* hermeshub URL (Tailscale HTTPS), not
/// the loopback address the proxy dials. The catalog already knows it — read it
/// from there rather than hardcoding a hostname that renaming the tailnet would
/// break (it has, once).
fn hermes_deep_link_base(state: &AppState) -> String {
    state
        .store
        .apps
        .iter()
        .find(|a| a.slug == "hermeshub")
        .map(|a| a.url.clone())
        .unwrap_or_else(|| state.hermeshub_url.clone())
}

// ── source: vault (local filesystem) ─────────────────────────────────────────

/// Filename + content search over the Obsidian vault's `*.md` notes.
///
/// Bounded on every axis: dot-directories (`.obsidian`, `.git`, `.trash`) are
/// never descended into, non-markdown (i.e. attachments) is never opened, files
/// over [`VAULT_MAX_FILE_BYTES`] are skipped, at most [`VAULT_MAX_FILES`] files
/// are read, and the walk stops as soon as `limit` hits are in hand.
///
/// Synchronous by nature — callers run it on a blocking thread.
pub(crate) fn search_vault(root: &Path, m: &Matcher, limit: usize) -> Vec<serde_json::Value> {
    let mut hits: Vec<serde_json::Value> = Vec::new();
    if m.is_empty() || !root.is_dir() {
        return hits;
    }

    let mut queue: std::collections::VecDeque<PathBuf> = std::collections::VecDeque::new();
    queue.push_back(root.to_path_buf());
    let mut files_read = 0usize;

    while let Some(dir) = queue.pop_front() {
        if hits.len() >= limit || files_read >= VAULT_MAX_FILES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if hits.len() >= limit || files_read >= VAULT_MAX_FILES {
                break;
            }
            let path = entry.path();
            let raw_name = entry.file_name();
            let name = raw_name.to_string_lossy();
            // `.obsidian`, `.git`, `.trash`, `.stfolder` — config and machinery,
            // never notes.
            if name.starts_with('.') {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                queue.push_back(path);
                continue;
            }
            if !ft.is_file() || !name.ends_with(".md") {
                continue;
            }
            if entry.metadata().map(|md| md.len()).unwrap_or(u64::MAX) > VAULT_MAX_FILE_BYTES {
                continue;
            }

            let stem = name.trim_end_matches(".md").to_owned();
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();

            // Filename match wins and costs no read.
            if m.matches(&stem) {
                hits.push(serde_json::json!({
                    "title": stem,
                    "subtitle": rel,
                    "path": rel,
                }));
                continue;
            }

            files_read += 1;
            let Ok(body) = std::fs::read_to_string(&path) else {
                continue;
            };
            if !m.matches_folded(&fold(&body)) {
                continue;
            }
            // Show the first line carrying the query; a whole-file match may be
            // spread across lines, so fall back to the first non-empty line
            // rather than dropping an otherwise valid hit.
            let line = body
                .lines()
                .find(|l| m.matches(l))
                .or_else(|| body.lines().find(|l| !l.trim().is_empty()))
                .unwrap_or("");
            hits.push(serde_json::json!({
                "title": stem,
                "subtitle": snippet(line),
                "path": rel,
            }));
        }
    }
    hits
}

// ── handler ──────────────────────────────────────────────────────────────────

fn json_response(status: StatusCode, body: String) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

/// `GET /api/search?q=<query>&limit=<n>` — grouped results across apps, tasks,
/// hermes and the vault. `limit` (default 20, max 50) is **per group**.
pub async fn get_search(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let query = params.get("q").map(|s| s.trim()).unwrap_or("").to_owned();
    let matcher = Matcher::new(&query);
    if matcher.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            r#"{"error":"q required"}"#.to_owned(),
        );
    }
    let limit = params
        .get("limit")
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);

    // apps is in-memory: no timeout needed, and it means the response is never
    // completely empty just because the network side of the box is unhappy.
    let apps = Group::ok("apps", search_apps(&state.store, &matcher, limit));

    let tasks = bounded(
        "tasks",
        search_tasks(
            state.http.clone(),
            state.cc_url.clone(),
            matcher.clone(),
            limit,
        ),
    );
    let hermes = bounded(
        "hermes",
        search_hermes(
            state.http.clone(),
            state.hermeshub_url.clone(),
            hermes_deep_link_base(&state),
            matcher.clone(),
            limit,
        ),
    );
    let vault_root = state.vault_path.clone();
    let vault_matcher = matcher.clone();
    let vault = bounded("vault", async move {
        tokio::task::spawn_blocking(move || search_vault(&vault_root, &vault_matcher, limit))
            .await
            .map_err(|e| format!("vault scan failed: {e}"))
    });

    let (tasks, hermes, vault) = tokio::join!(tasks, hermes, vault);

    let body = serde_json::json!({
        "query": query,
        "groups": [
            apps.into_json(),
            tasks.into_json(),
            hermes.into_json(),
            vault.into_json(),
        ],
    })
    .to_string();
    json_response(StatusCode::OK, body)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt; // oneshot

    // ── matcher ──────────────────────────────────────────────────────────────

    #[test]
    fn fold_strips_case_and_diacritics() {
        assert_eq!(fold("Café"), "cafe");
        assert_eq!(fold("MAÑANA"), "manana");
        assert_eq!(fold("Ñoño"), "nono");
        assert_eq!(fold("Über"), "uber");
        assert_eq!(fold("agenda self-hosted"), "agenda self-hosted");
    }

    #[test]
    fn matcher_is_case_and_diacritic_insensitive() {
        // Typed without accents, content has them (the common direction).
        assert!(Matcher::new("cafe").matches("Café de olla"));
        assert!(Matcher::new("manana").matches("MAÑANA temprano"));
        // And the reverse: typed with accents, content without.
        assert!(Matcher::new("Añadir").matches("anadir factura"));
        assert!(Matcher::new("VUELOS").matches("flight tracker vuelos"));
        assert!(!Matcher::new("zzz").matches("nothing here"));
    }

    #[test]
    fn matcher_requires_every_term() {
        let m = Matcher::new("flight tracker");
        assert!(m.matches("vuelos — flight tracker"));
        assert!(!m.matches("flight only"));
    }

    #[test]
    fn empty_query_matches_nothing() {
        let m = Matcher::new("   ");
        assert!(m.is_empty());
        assert!(!m.matches("anything at all"));
    }

    #[test]
    fn rank_prefers_title_prefix() {
        let m = Matcher::new("vue");
        assert_eq!(m.rank("vuelos"), 0);
        assert_eq!(m.rank("mis vuelos"), 1);
        assert_eq!(m.rank("unrelated"), 2);
    }

    #[test]
    fn snippet_is_char_safe() {
        let long = "á".repeat(500);
        let s = snippet(&long);
        assert!(s.ends_with('…'));
        assert!(s.chars().count() <= SNIPPET_CHARS + 1);
    }

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(encode_query_value("hermes"), "hermes");
        assert_eq!(encode_query_value("a b&c"), "a%20b%26c");
    }

    // ── apps ─────────────────────────────────────────────────────────────────

    #[test]
    fn apps_match_name_slug_and_tagline() {
        let cat = crate::store::Catalog::builtin();
        let hits = search_apps(&cat, &Matcher::new("vuelos"), 20);
        assert!(hits.iter().any(|h| h["slug"] == "vuelos"));

        // tagline-only match ("flight tracker" is the vuelos tagline)
        let hits = search_apps(&cat, &Matcher::new("flight"), 20);
        assert!(hits.iter().any(|h| h["slug"] == "vuelos"));

        // every hit carries the documented shape
        let h = &hits[0];
        for k in ["title", "subtitle", "url", "slug", "icon"] {
            assert!(h.get(k).is_some(), "app hit missing {k}");
        }
    }

    #[test]
    fn apps_normalize_unknown_icons_and_respect_limit() {
        let cat = crate::store::Catalog {
            apps: vec![crate::store::StoreApp {
                slug: "x".to_owned(),
                name: "xylo".to_owned(),
                tagline: String::new(),
                url: "http://x".to_owned(),
                port: None,
                host: None,
                icon: "not-a-real-glyph".to_owned(),
                hue: 1,
                category: "apps".to_owned(),
                private: false,
            }],
        };
        let hits = search_apps(&cat, &Matcher::new("xylo"), 20);
        assert_eq!(hits[0]["icon"], "app");

        let hits = search_apps(&crate::store::Catalog::builtin(), &Matcher::new("a"), 3);
        assert_eq!(hits.len(), 3, "limit is per group");
    }

    #[test]
    fn apps_flag_money_tiles_private() {
        let hits = search_apps(
            &crate::store::Catalog::builtin(),
            &Matcher::new("cuentas"),
            20,
        );
        let h = hits.iter().find(|h| h["slug"] == "cuentas").unwrap();
        assert_eq!(h["private"], true, "money tiles must stay flagged");
    }

    // ── tasks ────────────────────────────────────────────────────────────────

    fn task(id: i64, title: &str, status: &str, project: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "title": title, "status": status, "project_name": project,
            "priority": "high",
        })
    }

    #[test]
    fn tasks_match_title_and_project_and_shape() {
        let tasks = vec![
            task(1, "Añadir el tile de búsqueda", "backlog", "minimonitor"),
            task(2, "unrelated", "backlog", "dinara"),
        ];
        let hits = filter_tasks(&tasks, &Matcher::new("anadir"), 20);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["id"], 1);
        assert_eq!(hits[0]["subtitle"], "minimonitor · backlog");
        assert_eq!(hits[0]["url"], "/board");
        assert_eq!(hits[0]["project"], "minimonitor");
        assert_eq!(hits[0]["status"], "backlog");

        // project-name match alone is enough
        let hits = filter_tasks(&tasks, &Matcher::new("dinara"), 20);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["id"], 2);
    }

    #[test]
    fn tasks_skip_done_unless_query_says_done() {
        let tasks = vec![
            task(1, "ship search", "done", "minimonitor"),
            task(2, "ship search v2", "backlog", "minimonitor"),
        ];
        let hits = filter_tasks(&tasks, &Matcher::new("ship"), 20);
        assert_eq!(hits.len(), 1, "done is noise by default");
        assert_eq!(hits[0]["id"], 2);

        let hits = filter_tasks(&tasks, &Matcher::new("ship done"), 20);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["id"], 1, "asking for done returns done");
    }

    // ── vault ────────────────────────────────────────────────────────────────

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn vault_matches_filename_and_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "wiki/ios-sideload.md",
            "# sideload\nuse pymobiledevice3\n",
        );
        write(
            root,
            "notes/2026-08-13.md",
            "hoy revisé la máquina de café\n",
        );
        write(root, "notes/otro.md", "nada que ver\n");

        // filename match
        let hits = search_vault(root, &Matcher::new("sideload"), 20);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["title"], "ios-sideload");
        assert_eq!(hits[0]["path"], "wiki/ios-sideload.md");
        assert!(hits[0].get("url").is_none(), "vault hits carry no url");

        // content match, diacritic-insensitive
        let hits = search_vault(root, &Matcher::new("cafe"), 20);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["title"], "2026-08-13");
        assert_eq!(hits[0]["subtitle"], "hoy revisé la máquina de café");

        // nothing matches
        assert!(search_vault(root, &Matcher::new("zzzz"), 20).is_empty());
    }

    #[test]
    fn vault_skips_dotdirs_attachments_and_big_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, ".obsidian/plugins/needle.md", "needle in config\n");
        write(root, ".trash/needle-deleted.md", "needle in trash\n");
        write(root, "attachments/needle.png", "needle but binary-ish\n");
        write(
            root,
            "big.md",
            &format!("needle\n{}", "x".repeat(2 * 1024 * 1024)),
        );
        write(root, "good.md", "needle here\n");

        let hits = search_vault(root, &Matcher::new("needle"), 20);
        assert_eq!(hits.len(), 1, "only good.md should match: {hits:?}");
        assert_eq!(hits[0]["path"], "good.md");
    }

    #[test]
    fn vault_respects_limit_and_missing_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..10 {
            write(root, &format!("n{i}.md"), "needle\n");
        }
        assert_eq!(search_vault(root, &Matcher::new("needle"), 4).len(), 4);
        // A vault path that isn't there yet is empty, not an error.
        assert!(search_vault(Path::new("/nonexistent/vault"), &Matcher::new("x"), 5).is_empty());
    }

    // ── handler ──────────────────────────────────────────────────────────────

    /// A router whose remote siblings all point at a closed port, so tasks and
    /// hermes are guaranteed to fail — which is the point of the shape tests.
    fn search_router(vault_path: PathBuf) -> axum::Router {
        super::super::build_router_with(AppState {
            db_path: PathBuf::from("/nonexistent/fleet.db"),
            online_threshold: Duration::from_secs(900),
            snapshot_stale_threshold: Duration::from_secs(10_800),
            beszel_ui_url: String::new(),
            kuma_ui_url: String::new(),
            labels: std::sync::Arc::new(crate::service_label::Labels::empty()),
            store: std::sync::Arc::new(crate::store::Catalog::builtin()),
            http: reqwest::Client::new(),
            cc_url: "http://127.0.0.1:1".to_owned(),
            cuentas_url: "http://127.0.0.1:1".to_owned(),
            cuentas_basic_auth: None,
            hermeshub_url: "http://127.0.0.1:1".to_owned(),
            vitals_url: "http://127.0.0.1:1".to_owned(),
            polybot_url: "http://127.0.0.1:1".to_owned(),
            portfolio_url: "http://127.0.0.1:1".to_owned(),
            portfolio_token: None,
            money_pin: None,
            tickers: std::sync::Arc::new(crate::config::default_tickers()),
            vault_path,
        })
    }

    async fn get_json(router: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let resp = router
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn search_requires_a_query() {
        let dir = tempfile::tempdir().unwrap();
        let (status, body) = get_json(search_router(dir.path().to_path_buf()), "/api/search").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].is_string());

        let (status, _) = get_json(
            search_router(dir.path().to_path_buf()),
            "/api/search?q=%20%20",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "whitespace is not a query");
    }

    #[tokio::test]
    async fn search_returns_the_four_groups_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let (status, body) = get_json(
            search_router(dir.path().to_path_buf()),
            "/api/search?q=vuelos",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["query"], "vuelos");
        let groups = body["groups"].as_array().unwrap();
        let kinds: Vec<&str> = groups.iter().map(|g| g["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["apps", "tasks", "hermes", "vault"]);
        for g in groups {
            assert!(g["hits"].is_array(), "every group carries a hits array");
        }
    }

    #[tokio::test]
    async fn dead_sources_degrade_their_own_group_only() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "vuelos-notes.md", "sobre vuelos\n");
        let (status, body) = get_json(
            search_router(dir.path().to_path_buf()),
            "/api/search?q=vuelos",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let groups = body["groups"].as_array().unwrap();
        let by_kind = |k: &str| groups.iter().find(|g| g["kind"] == k).unwrap().clone();

        // Local sources still answer…
        let apps = by_kind("apps");
        assert!(apps["error"].is_null(), "apps never fails");
        assert!(!apps["hits"].as_array().unwrap().is_empty());
        let vault = by_kind("vault");
        assert!(vault["error"].is_null(), "vault is local, must not fail");
        assert_eq!(vault["hits"][0]["path"], "vuelos-notes.md");

        // …while the unreachable ones report an error and an empty hit list.
        for k in ["tasks", "hermes"] {
            let g = by_kind(k);
            assert!(g["error"].is_string(), "{k} should report its failure");
            assert!(g["hits"].as_array().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn limit_is_clamped_and_applies_per_group() {
        let dir = tempfile::tempdir().unwrap();
        let (_, body) = get_json(
            search_router(dir.path().to_path_buf()),
            "/api/search?q=a&limit=2",
        )
        .await;
        assert!(body["groups"][0]["hits"].as_array().unwrap().len() <= 2);

        // Absurd limits clamp rather than 400.
        let (status, body) = get_json(
            search_router(dir.path().to_path_buf()),
            "/api/search?q=a&limit=99999",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["groups"][0]["hits"].as_array().unwrap().len() <= MAX_LIMIT);
    }

    #[test]
    fn hermes_deep_link_uses_the_public_catalog_url() {
        let base = crate::store::Catalog::builtin()
            .apps
            .iter()
            .find(|a| a.slug == "hermeshub")
            .unwrap()
            .url
            .clone();
        assert!(
            base.starts_with("https://"),
            "deep links must use the public HTTPS URL, not loopback"
        );
    }
}

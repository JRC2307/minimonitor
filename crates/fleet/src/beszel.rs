//! PocketBase REST client for Beszel (agent-tier `fleet enroll`).
//!
//! ## Design constraints (spec §3.7 + task-11-brief.md)
//!
//! 1. **Auth against `users` collection** (NOT `_superusers`).
//!    The Beszel `/api/beszel/*` routes reject superuser tokens; only regular
//!    `users` tokens are accepted.
//! 2. **Raw `Authorization` header** — no `Bearer ` prefix.
//!    PocketBase auth endpoints for regular users return a token that is sent
//!    raw, not as `Bearer <token>`.
//! 3. **Parameterized filters only** — never string-interpolated.
//!    A `filter=host={:h}` with a bound params object prevents injection from
//!    attacker-controlled hostnames (R-2).
//! 4. **Never blind-create** — match-on-self-reported-identity only.
//!    Agents self-register under the universal-token bootstrap; enroll only
//!    PATCHes drift, never POSTs a create-by-fleet_id.
//! 5. **On-demand universal-token** — enabled only when there is at least one
//!    desired agent node with no matching system; NOT re-enabled on every run.
//! 6. **40% delete-guard** — if >40% of existing systems would be deleted,
//!    abort loudly and delete nothing.
//! 7. **Idempotent** — a second run with no drift executes no writes.

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

/// Constant percent threshold for the decommission delete-guard (R-12).
/// Hardcoded — not a config knob (spec §3.7 note).
pub const DELETE_GUARD_PCT: usize = 40;

// ─── Wire types ──────────────────────────────────────────────────────────────

/// Response from `POST /api/collections/users/auth-with-password`.
#[derive(Debug, Deserialize)]
pub struct AuthResponse {
    pub token: String,
    // `record` field exists on the wire but is not needed here.
}

/// A Beszel `systems` record as returned by the PocketBase list/filter API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BeszelSystem {
    /// PocketBase record id (opaque string).
    pub id: String,
    /// The agent's self-reported name.
    pub name: String,
    /// The agent's self-reported host (typically the tailnet IP `100.x.y.z`).
    pub host: String,
    /// Agent port (typically 45876).
    #[serde(default)]
    pub port: u16,
    /// Users linked to this system.
    #[serde(default)]
    pub users: Vec<String>,
    /// System status from Beszel.
    #[serde(default)]
    pub status: String,
}

/// Request body for `PATCH /api/collections/systems/records/{id}`.
#[derive(Debug, Serialize)]
pub struct PatchSystemRequest {
    /// Friendly name to set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Users to link.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub users: Option<Vec<String>>,
}

/// PocketBase paginated list envelope.
#[derive(Debug, Deserialize)]
pub struct PbList<T> {
    pub page: u32,
    #[serde(rename = "perPage")]
    pub per_page: u32,
    #[serde(rename = "totalItems")]
    pub total_items: u32,
    pub items: Vec<T>,
}

// ─── Client ──────────────────────────────────────────────────────────────────

/// Thin PocketBase REST client for the Beszel `systems` collection.
///
/// `base_url` is injectable so tests can point it at a wiremock server.
#[derive(Clone)]
pub struct BeszelClient {
    base_url: String,
    http: reqwest::Client,
}

impl BeszelClient {
    /// Construct a client against `base_url` (no trailing slash required).
    /// In production this is `http://<host>:8090`; in tests it is the wiremock
    /// server URI.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            http: reqwest::Client::new(),
        }
    }

    /// Authenticate against the **`users`** collection (NOT `_superusers`).
    ///
    /// Returns the raw token string. The caller must pass this token to every
    /// subsequent request via `Authorization: <token>` (no `Bearer` prefix).
    pub async fn auth_with_password(
        &self,
        identity: &str,
        password: &str,
    ) -> anyhow::Result<String> {
        // IMPORTANT: uses `users`, not `_superusers`.
        let url = format!("{}/api/collections/users/auth-with-password", self.base_url);
        let body = serde_json::json!({ "identity": identity, "password": password });
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("beszel auth request failed")?;

        let status = resp.status();
        if !status.is_success() {
            bail!("beszel auth returned HTTP {status}");
        }
        let auth: AuthResponse = resp.json().await.context("decoding beszel auth response")?;
        Ok(auth.token)
    }

    /// List Beszel systems, optionally filtered by `host` (parameterized — no
    /// string interpolation of the host value into the filter string).
    ///
    /// `host_filter` is `Some(ip)` to match a specific agent host; `None` to
    /// list all systems (used at reconcile start to get the full inventory).
    pub async fn list_systems(
        &self,
        token: &str,
        host_filter: Option<&str>,
    ) -> anyhow::Result<Vec<BeszelSystem>> {
        // All pages — fetch until totalItems is satisfied.
        let mut all: Vec<BeszelSystem> = Vec::new();
        let mut page = 1u32;

        loop {
            let mut req = self
                .http
                .get(format!("{}/api/collections/systems/records", self.base_url));
            let per_page = "100".to_owned();
            let page_str = page.to_string();
            req = req
                // Raw token — no Bearer prefix.
                .header("Authorization", token)
                .query(&[("page", &page_str), ("perPage", &per_page)]);

            if let Some(host) = host_filter {
                // Parameterized filter: the host value is passed as a separate
                // query parameter, never interpolated into the filter string.
                req = req.query(&[("filter", "host={:h}"), ("h", host)]);
            }

            let resp = req
                .send()
                .await
                .context("beszel list_systems request failed")?;
            let status = resp.status();
            if !status.is_success() {
                bail!("beszel list_systems returned HTTP {status}");
            }
            let list: PbList<BeszelSystem> = resp
                .json()
                .await
                .context("decoding beszel list_systems response")?;

            let fetched = list.items.len() as u32;
            all.extend(list.items);
            // Stop when we've fetched all items or got an empty page.
            if fetched == 0 || all.len() as u32 >= list.total_items {
                break;
            }
            page += 1;
        }

        Ok(all)
    }

    /// PATCH a Beszel system record to update `name` and/or `users`.
    pub async fn patch_system(
        &self,
        token: &str,
        record_id: &str,
        req: &PatchSystemRequest,
    ) -> anyhow::Result<()> {
        let url = format!(
            "{}/api/collections/systems/records/{}",
            self.base_url, record_id
        );
        let resp = self
            .http
            .patch(&url)
            .header("Authorization", token)
            .json(req)
            .send()
            .await
            .context("beszel patch_system request failed")?;

        let status = resp.status();
        if !status.is_success() {
            bail!("beszel patch_system returned HTTP {status}");
        }
        Ok(())
    }

    /// DELETE a Beszel system record (decommission).
    pub async fn delete_system(&self, token: &str, record_id: &str) -> anyhow::Result<()> {
        let url = format!(
            "{}/api/collections/systems/records/{}",
            self.base_url, record_id
        );
        let resp = self
            .http
            .delete(&url)
            .header("Authorization", token)
            .send()
            .await
            .context("beszel delete_system request failed")?;

        let status = resp.status();
        if !status.is_success() {
            bail!("beszel delete_system returned HTTP {status}");
        }
        Ok(())
    }

    /// Enable the universal bootstrap token (on-demand, only when agents are
    /// unregistered). Calls the Beszel settings endpoint to enable it.
    pub async fn enable_bootstrap_token(&self, token: &str) -> anyhow::Result<()> {
        let url = format!("{}/api/beszel/set-user-token", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", token)
            .json(&serde_json::json!({ "enabled": true }))
            .send()
            .await
            .context("beszel enable_bootstrap_token request failed")?;

        let status = resp.status();
        if !status.is_success() {
            bail!("beszel enable_bootstrap_token returned HTTP {status}");
        }
        Ok(())
    }
}

// ─── Enrollment row ───────────────────────────────────────────────────────────

/// An enrollment row from the local `enrollment` table for `system='beszel'`.
#[derive(Debug, Clone)]
pub struct EnrollmentRow {
    pub fleet_id: String,
    pub remote_id: String,
}

// ─── Pure reconcile logic ─────────────────────────────────────────────────────

/// An agent node the enroll command wants enrolled in Beszel.
#[derive(Debug, Clone)]
pub struct DesiredAgent {
    /// Stable local fleet id.
    pub fleet_id: String,
    /// Friendly hostname (used to PATCH `name` on the Beszel record).
    pub friendly_name: String,
    /// The agent's tailnet IP (`100.x.y.z`) — the match key against
    /// `BeszelSystem.host`.
    pub tailnet_ip: String,
}

/// The plan produced by [`plan_reconcile`].
#[derive(Debug)]
pub struct ReconcilePlan {
    /// Systems to PATCH (drift detected).
    pub to_patch: Vec<(DesiredAgent, BeszelSystem)>,
    /// Fleet ids whose desired agents have no matching Beszel system.
    /// When non-empty, the universal-token enable call is needed.
    pub missing_fleet_ids: Vec<String>,
    /// Enrollment rows to decommission (fleet_id no longer desired).
    pub to_delete: Vec<EnrollmentRow>,
    /// Whether the delete-guard was tripped.
    pub guard_tripped: bool,
}

/// Pure reconcile planner — no I/O.
///
/// Produces a [`ReconcilePlan`] that the caller executes against the live API.
///
/// # Arguments
/// - `desired` — agent-tier nodes that *should* exist in Beszel.
/// - `existing_systems` — all systems currently in Beszel.
/// - `enrolled` — enrollment rows from the local DB (`system='beszel'`).
pub fn plan_reconcile(
    desired: &[DesiredAgent],
    existing_systems: &[BeszelSystem],
    enrolled: &[EnrollmentRow],
) -> ReconcilePlan {
    // Build a lookup: tailnet_ip → BeszelSystem.
    let by_host: std::collections::HashMap<&str, &BeszelSystem> = existing_systems
        .iter()
        .map(|s| (s.host.as_str(), s))
        .collect();

    // Note: enrolled_map is kept for future extension (e.g., checking remote_id matches).
    let _enrolled_map: std::collections::HashMap<&str, &EnrollmentRow> =
        enrolled.iter().map(|e| (e.fleet_id.as_str(), e)).collect();

    // Build the set of desired fleet_ids.
    let desired_ids: std::collections::HashSet<&str> =
        desired.iter().map(|d| d.fleet_id.as_str()).collect();

    let mut to_patch: Vec<(DesiredAgent, BeszelSystem)> = Vec::new();
    let mut missing_fleet_ids: Vec<String> = Vec::new();

    for agent in desired {
        match by_host.get(agent.tailnet_ip.as_str()) {
            Some(&sys) => {
                // System exists — check for drift.
                if drifted(sys, agent) {
                    to_patch.push((agent.clone(), sys.clone()));
                }
                // (If not drifted, no-op — idempotent.)
            }
            None => {
                // No system found for this agent's host — it hasn't registered
                // yet. We do NOT create-by-fleet_id; instead we signal that the
                // bootstrap token needs to be enabled on-demand.
                missing_fleet_ids.push(agent.fleet_id.clone());
            }
        }
    }

    // Decommission: enrollment rows whose fleet_id is no longer desired.
    let to_delete: Vec<EnrollmentRow> = enrolled
        .iter()
        .filter(|e| !desired_ids.contains(e.fleet_id.as_str()))
        // Only delete rows that actually have a remote_id we can delete.
        .filter(|e| !e.remote_id.is_empty())
        .cloned()
        .collect();

    // Delete guard: if >40% of existing systems would be deleted, abort.
    let guard_tripped = !existing_systems.is_empty()
        && to_delete.len() * 100 / existing_systems.len() > DELETE_GUARD_PCT;

    ReconcilePlan {
        to_patch,
        missing_fleet_ids,
        to_delete: if guard_tripped { vec![] } else { to_delete },
        guard_tripped,
    }
}

/// Returns true if the Beszel system record has drifted from the desired agent
/// state (friendly name differs).
fn drifted(sys: &BeszelSystem, agent: &DesiredAgent) -> bool {
    sys.name != agent.friendly_name
}

// ─── DB helpers for enrollment table ─────────────────────────────────────────

/// Upsert an enrollment row in the local DB (`system='beszel'`).
pub fn upsert_enrollment(
    conn: &rusqlite::Connection,
    fleet_id: &str,
    remote_id: &str,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO enrollment (fleet_id, system, remote_id, last_enrolled)
         VALUES (?1, 'beszel', ?2, ?3)
         ON CONFLICT(fleet_id, system) DO UPDATE SET
             remote_id     = excluded.remote_id,
             last_enrolled = excluded.last_enrolled",
        rusqlite::params![fleet_id, remote_id, now],
    )
    .context("upsert enrollment (beszel)")?;
    Ok(())
}

/// List all Beszel enrollment rows from the local DB.
pub fn list_enrollments(conn: &rusqlite::Connection) -> anyhow::Result<Vec<EnrollmentRow>> {
    let mut stmt =
        conn.prepare("SELECT fleet_id, remote_id FROM enrollment WHERE system='beszel'")?;
    let rows: anyhow::Result<Vec<EnrollmentRow>> = stmt
        .query_map([], |row| {
            Ok(EnrollmentRow {
                fleet_id: row.get(0)?,
                remote_id: row.get(1)?,
            })
        })?
        .map(|r| r.context("enrollment row"))
        .collect();
    rows
}

/// Delete a Beszel enrollment row from the local DB.
pub fn delete_enrollment(conn: &rusqlite::Connection, fleet_id: &str) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM enrollment WHERE fleet_id=?1 AND system='beszel'",
        rusqlite::params![fleet_id],
    )
    .context("delete enrollment (beszel)")?;
    Ok(())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use wiremock::matchers::{header, header_exists, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ── Fixtures (author-recorded, confirm at deploy) ────────────────────────
    //
    // These fixtures are authored to match Beszel 0.9.1 / PocketBase 0.x
    // wire shapes, based on the spec and PocketBase documentation.
    // Mark: "author-recorded" — confirm against a live Beszel 0.9.1 at deploy.

    fn auth_response() -> serde_json::Value {
        serde_json::json!({
            "token": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.test", // # pragma: allowlist secret
            "record": {
                "id": "usr001",
                "email": "caguabot@example.com",
                "collectionId": "users",
                "collectionName": "users"
            }
        })
    }

    fn systems_list_empty() -> serde_json::Value {
        serde_json::json!({
            "page": 1,
            "perPage": 100,
            "totalItems": 0,
            "totalPages": 0,
            "items": []
        })
    }

    fn systems_list_one(id: &str, name: &str, host: &str) -> serde_json::Value {
        serde_json::json!({
            "page": 1,
            "perPage": 100,
            "totalItems": 1,
            "totalPages": 1,
            "items": [{
                "id": id,
                "collectionId": "systems",
                "collectionName": "systems",
                "name": name,
                "host": host,
                "port": 45876,
                "status": "up",
                "users": []
            }]
        })
    }

    fn systems_list_many(items: Vec<serde_json::Value>) -> serde_json::Value {
        let count = items.len();
        serde_json::json!({
            "page": 1,
            "perPage": 100,
            "totalItems": count,
            "totalPages": 1,
            "items": items
        })
    }

    fn system_item(id: &str, name: &str, host: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "collectionId": "systems",
            "collectionName": "systems",
            "name": name,
            "host": host,
            "port": 45876,
            "status": "up",
            "users": []
        })
    }

    // ── T1: auth uses `users` collection, raw token header ───────────────────

    #[tokio::test]
    async fn auth_uses_users_collection_not_superusers() {
        let server = MockServer::start().await;

        // Assert the path is /api/collections/users/... (NOT _superusers)
        Mock::given(method("POST"))
            .and(path("/api/collections/users/auth-with-password"))
            .respond_with(ResponseTemplate::new(200).set_body_json(auth_response()))
            .expect(1)
            .mount(&server)
            .await;

        let client = BeszelClient::new(server.uri());
        let token = client
            .auth_with_password("caguabot@example.com", "s3cr3t")
            .await
            .unwrap();
        assert_eq!(
            token, "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.test",
            "token returned from auth response"
        );
        // wiremock will verify the mock was called exactly once
    }

    #[tokio::test]
    async fn auth_token_sent_raw_not_bearer() {
        let server = MockServer::start().await;

        // Mount auth
        Mock::given(method("POST"))
            .and(path("/api/collections/users/auth-with-password"))
            .respond_with(ResponseTemplate::new(200).set_body_json(auth_response()))
            .mount(&server)
            .await;

        // Assert list_systems sends the token RAW (no "Bearer " prefix).
        // The header value should be exactly the token string.
        let raw_token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.test"; // # pragma: allowlist secret
        Mock::given(method("GET"))
            .and(path("/api/collections/systems/records"))
            // Must have Authorization header, and it must NOT start with "Bearer "
            .and(header("Authorization", raw_token))
            .respond_with(ResponseTemplate::new(200).set_body_json(systems_list_empty()))
            .expect(1)
            .mount(&server)
            .await;

        let client = BeszelClient::new(server.uri());
        let token = client
            .auth_with_password("caguabot@example.com", "s3cr3t")
            .await
            .unwrap();
        // Confirm token does NOT have "Bearer " prepended when used.
        assert!(
            !token.starts_with("Bearer "),
            "token must not start with Bearer"
        );
        client.list_systems(&token, None).await.unwrap();
    }

    // ── T2: parameterized filter — injection chars can't alter filter ─────────

    #[tokio::test]
    async fn filter_is_parameterized_injection_chars_cannot_escape() {
        let server = MockServer::start().await;

        // A host with injection characters.
        let evil_host = "100.1.2.3' OR '1'='1";

        // The filter parameter must be the literal string "host={:h}" —
        // the host value is passed separately as "h=<evil_host>".
        // wiremock must see BOTH parameters with these exact values.
        Mock::given(method("GET"))
            .and(path("/api/collections/systems/records"))
            .and(query_param("filter", "host={:h}"))
            .and(query_param("h", evil_host))
            .respond_with(ResponseTemplate::new(200).set_body_json(systems_list_empty()))
            .expect(1)
            .mount(&server)
            .await;

        let client = BeszelClient::new(server.uri());
        // Even with injection chars in the host value, the filter string itself
        // remains parameterized.
        client
            .list_systems("raw-token", Some(evil_host))
            .await
            .unwrap();
    }

    // ── T3: idempotent — no-dup, second run creates nothing ──────────────────

    #[test]
    fn idempotent_no_patch_when_no_drift() {
        // If the existing system already has the desired name, plan should
        // contain no patches and no missing fleet ids.
        let desired = vec![DesiredAgent {
            fleet_id: "nas-01".to_owned(),
            friendly_name: "nas-01".to_owned(),
            tailnet_ip: "100.64.0.1".to_owned(),
        }];

        let existing = vec![BeszelSystem {
            id: "sys001".to_owned(),
            name: "nas-01".to_owned(), // matches friendly_name
            host: "100.64.0.1".to_owned(),
            port: 45876,
            users: vec![],
            status: "up".to_owned(),
        }];

        let enrolled = vec![EnrollmentRow {
            fleet_id: "nas-01".to_owned(),
            remote_id: "sys001".to_owned(),
        }];

        let plan = plan_reconcile(&desired, &existing, &enrolled);

        assert!(plan.to_patch.is_empty(), "no patch on idempotent run");
        assert!(plan.missing_fleet_ids.is_empty(), "no missing agents");
        assert!(plan.to_delete.is_empty(), "nothing to decommission");
        assert!(!plan.guard_tripped, "guard not tripped");
    }

    #[test]
    fn idempotent_patch_on_name_drift() {
        // If the name has drifted, a patch should be queued.
        let desired = vec![DesiredAgent {
            fleet_id: "nas-01".to_owned(),
            friendly_name: "NAS-01-friendly".to_owned(), // different from existing
            tailnet_ip: "100.64.0.1".to_owned(),
        }];

        let existing = vec![BeszelSystem {
            id: "sys001".to_owned(),
            name: "old-name".to_owned(), // drifted
            host: "100.64.0.1".to_owned(),
            port: 45876,
            users: vec![],
            status: "up".to_owned(),
        }];

        let enrolled = vec![EnrollmentRow {
            fleet_id: "nas-01".to_owned(),
            remote_id: "sys001".to_owned(),
        }];

        let plan = plan_reconcile(&desired, &existing, &enrolled);

        assert_eq!(plan.to_patch.len(), 1, "drift detected → patch queued");
        assert_eq!(plan.to_patch[0].0.fleet_id, "nas-01");
        assert_eq!(plan.to_patch[0].1.id, "sys001");
    }

    // ── T4: never blind-create — missing agent → enable token, no POST ────────

    #[test]
    fn never_blind_create_missing_agent_signals_token_enable() {
        // A desired agent with no matching Beszel system.
        let desired = vec![DesiredAgent {
            fleet_id: "worker-01".to_owned(),
            friendly_name: "worker-01".to_owned(),
            tailnet_ip: "100.64.0.2".to_owned(),
        }];

        let existing: Vec<BeszelSystem> = vec![]; // no systems registered yet
        let enrolled: Vec<EnrollmentRow> = vec![]; // no enrollment rows either

        let plan = plan_reconcile(&desired, &existing, &enrolled);

        // Must NOT queue a create/blind-POST.
        assert!(plan.to_patch.is_empty(), "no patch for missing agent");
        assert!(plan.to_delete.is_empty(), "nothing to delete");
        // Must signal that this agent is missing (so the caller enables the token).
        assert_eq!(
            plan.missing_fleet_ids,
            vec!["worker-01".to_owned()],
            "missing agent flagged"
        );
    }

    // ── T5: decommission delete-guard ─────────────────────────────────────────

    #[test]
    fn delete_guard_triggers_when_more_than_40_pct() {
        // 5 existing systems in Beszel (via enrollment rows).
        // 1 still desired → 4 to delete = 80% → guard trips.
        let desired = vec![DesiredAgent {
            fleet_id: "keep".to_owned(),
            friendly_name: "keep".to_owned(),
            tailnet_ip: "100.64.0.1".to_owned(),
        }];

        let existing: Vec<BeszelSystem> = (0..5)
            .map(|i| BeszelSystem {
                id: format!("sys{i:03}"),
                name: format!("host-{i}"),
                host: format!("100.64.0.{i}"),
                port: 45876,
                users: vec![],
                status: "up".to_owned(),
            })
            .collect();

        // host 0 matches "keep"; hosts 1-4 have enrollment rows that are being
        // decommissioned.
        let enrolled: Vec<EnrollmentRow> = (1..5)
            .map(|i| EnrollmentRow {
                fleet_id: format!("gone-{i}"),
                remote_id: format!("sys{i:03}"),
            })
            .collect();

        let plan = plan_reconcile(&desired, &existing, &enrolled);

        assert!(plan.guard_tripped, "guard must trip at 80%");
        assert!(
            plan.to_delete.is_empty(),
            "to_delete must be empty when guard trips"
        );
    }

    #[test]
    fn delete_guard_does_not_trigger_at_or_below_40_pct() {
        // 5 existing, 2 to delete = 40% → guard does NOT trip (threshold is >40%).
        let desired: Vec<DesiredAgent> = (0..3)
            .map(|i| DesiredAgent {
                fleet_id: format!("keep-{i}"),
                friendly_name: format!("keep-{i}"),
                tailnet_ip: format!("100.64.0.{i}"),
            })
            .collect();

        let existing: Vec<BeszelSystem> = (0..5)
            .map(|i| BeszelSystem {
                id: format!("sys{i:03}"),
                name: format!("host-{i}"),
                host: format!("100.64.0.{i}"),
                port: 45876,
                users: vec![],
                status: "up".to_owned(),
            })
            .collect();

        // 2 enrollment rows not in desired → to_delete.
        let enrolled: Vec<EnrollmentRow> = (3..5)
            .map(|i| EnrollmentRow {
                fleet_id: format!("gone-{i}"),
                remote_id: format!("sys{i:03}"),
            })
            .collect();

        let plan = plan_reconcile(&desired, &existing, &enrolled);

        assert!(
            !plan.guard_tripped,
            "40% = exactly at threshold, should NOT trip"
        );
        assert_eq!(plan.to_delete.len(), 2, "2 systems to decommission");
    }

    // ── T6: on-demand token NOT enabled when no new agents ────────────────────

    #[test]
    fn token_not_enabled_when_no_missing_agents() {
        // All desired agents already have matching Beszel systems.
        let desired = vec![DesiredAgent {
            fleet_id: "nas-01".to_owned(),
            friendly_name: "nas-01".to_owned(),
            tailnet_ip: "100.64.0.1".to_owned(),
        }];

        let existing = vec![BeszelSystem {
            id: "sys001".to_owned(),
            name: "nas-01".to_owned(),
            host: "100.64.0.1".to_owned(),
            port: 45876,
            users: vec![],
            status: "up".to_owned(),
        }];

        let enrolled = vec![EnrollmentRow {
            fleet_id: "nas-01".to_owned(),
            remote_id: "sys001".to_owned(),
        }];

        let plan = plan_reconcile(&desired, &existing, &enrolled);

        // No missing agents → caller must NOT call enable_bootstrap_token.
        assert!(
            plan.missing_fleet_ids.is_empty(),
            "missing_fleet_ids must be empty when all agents registered"
        );
    }

    // ── T7: wiremock — enable endpoint NOT called when no new agents ──────────

    #[tokio::test]
    async fn enable_token_endpoint_not_called_when_all_agents_registered() {
        let server = MockServer::start().await;

        // Mount auth.
        Mock::given(method("POST"))
            .and(path("/api/collections/users/auth-with-password"))
            .respond_with(ResponseTemplate::new(200).set_body_json(auth_response()))
            .mount(&server)
            .await;

        // Mount list — one system already registered.
        Mock::given(method("GET"))
            .and(path("/api/collections/systems/records"))
            .respond_with(ResponseTemplate::new(200).set_body_json(systems_list_one(
                "sys001",
                "nas-01",
                "100.64.0.1",
            )))
            .mount(&server)
            .await;

        // The enable-token endpoint MUST NOT be called.
        Mock::given(method("POST"))
            .and(path("/api/beszel/set-user-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0) // assert never called
            .mount(&server)
            .await;

        let client = BeszelClient::new(server.uri());
        let token = client
            .auth_with_password("caguabot@example.com", "s3cr3t")
            .await
            .unwrap();

        let systems = client.list_systems(&token, None).await.unwrap();
        assert_eq!(systems.len(), 1);

        // Plan: one desired agent that is already registered → missing_fleet_ids empty.
        let desired = vec![DesiredAgent {
            fleet_id: "nas-01".to_owned(),
            friendly_name: "nas-01".to_owned(),
            tailnet_ip: "100.64.0.1".to_owned(),
        }];
        let enrolled = vec![EnrollmentRow {
            fleet_id: "nas-01".to_owned(),
            remote_id: "sys001".to_owned(),
        }];
        let plan = plan_reconcile(&desired, &systems, &enrolled);

        // Only call enable_bootstrap_token if there are missing agents.
        if !plan.missing_fleet_ids.is_empty() {
            client.enable_bootstrap_token(&token).await.unwrap();
        }
        // wiremock verifies the expect(0) mock was never called.
    }

    // ── T8: patch is called via wiremock ─────────────────────────────────────

    #[tokio::test]
    async fn patch_system_sends_correct_request() {
        let server = MockServer::start().await;

        Mock::given(method("PATCH"))
            .and(path("/api/collections/systems/records/sys001"))
            .and(header_exists("Authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(system_item(
                "sys001",
                "new-name",
                "100.64.0.1",
            )))
            .expect(1)
            .mount(&server)
            .await;

        let client = BeszelClient::new(server.uri());
        client
            .patch_system(
                "raw-token",
                "sys001",
                &PatchSystemRequest {
                    name: Some("new-name".to_owned()),
                    users: None,
                },
            )
            .await
            .unwrap();
    }

    // ── T9: delete is called via wiremock ─────────────────────────────────────

    #[tokio::test]
    async fn delete_system_sends_correct_request() {
        let server = MockServer::start().await;

        Mock::given(method("DELETE"))
            .and(path("/api/collections/systems/records/sys001"))
            .and(header_exists("Authorization"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = BeszelClient::new(server.uri());
        client.delete_system("raw-token", "sys001").await.unwrap();
    }

    // ── DB helper tests ───────────────────────────────────────────────────────

    #[test]
    fn enrollment_db_round_trip() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let conn = crate::db::open(f.path()).unwrap();
        // Insert a node first (FK).
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
            tier: crate::model::Tier::Agent,
            dedupe_key_kind: crate::model::DedupeKind::Fuzzy,
            notes: None,
            first_seen: now,
            updated_at: now,
            fuzzy_hint: None,
        };
        crate::db::nodes::upsert_node(&conn, &node).unwrap();

        upsert_enrollment(&conn, "n1", "sys001").unwrap();
        let rows = list_enrollments(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].fleet_id, "n1");
        assert_eq!(rows[0].remote_id, "sys001");

        // Upsert again (idempotent).
        upsert_enrollment(&conn, "n1", "sys001-updated").unwrap();
        let rows2 = list_enrollments(&conn).unwrap();
        assert_eq!(rows2.len(), 1);
        assert_eq!(rows2[0].remote_id, "sys001-updated");

        delete_enrollment(&conn, "n1").unwrap();
        let rows3 = list_enrollments(&conn).unwrap();
        assert!(rows3.is_empty());
    }

    // ── T10: multiple existing systems, full reconcile wiremock ──────────────

    #[tokio::test]
    async fn full_reconcile_decommissions_with_guard_ok() {
        let server = MockServer::start().await;

        // Auth
        Mock::given(method("POST"))
            .and(path("/api/collections/users/auth-with-password"))
            .respond_with(ResponseTemplate::new(200).set_body_json(auth_response()))
            .mount(&server)
            .await;

        // List returns 3 systems: sys001 (keep, exact), sys002 (decommission), sys003 (keep, exact)
        let items = vec![
            system_item("sys001", "nas-01", "100.64.0.1"),
            system_item("sys002", "gone-box", "100.64.0.9"),
            system_item("sys003", "worker-01", "100.64.0.2"),
        ];
        Mock::given(method("GET"))
            .and(path("/api/collections/systems/records"))
            .respond_with(ResponseTemplate::new(200).set_body_json(systems_list_many(items)))
            .mount(&server)
            .await;

        // DELETE sys002 (decommission — 1 of 3 = 33% < 40%, guard ok)
        Mock::given(method("DELETE"))
            .and(path("/api/collections/systems/records/sys002"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = BeszelClient::new(server.uri());
        let token = client
            .auth_with_password("caguabot@example.com", "s3cr3t")
            .await
            .unwrap();

        let systems = client.list_systems(&token, None).await.unwrap();
        assert_eq!(systems.len(), 3);

        let desired = vec![
            DesiredAgent {
                fleet_id: "nas-01".to_owned(),
                friendly_name: "nas-01".to_owned(),
                tailnet_ip: "100.64.0.1".to_owned(),
            },
            DesiredAgent {
                fleet_id: "worker-01".to_owned(),
                friendly_name: "worker-01".to_owned(),
                tailnet_ip: "100.64.0.2".to_owned(),
            },
        ];

        let enrolled = vec![
            EnrollmentRow {
                fleet_id: "nas-01".to_owned(),
                remote_id: "sys001".to_owned(),
            },
            EnrollmentRow {
                fleet_id: "gone-box".to_owned(),
                remote_id: "sys002".to_owned(),
            },
            EnrollmentRow {
                fleet_id: "worker-01".to_owned(),
                remote_id: "sys003".to_owned(),
            },
        ];

        let plan = plan_reconcile(&desired, &systems, &enrolled);

        assert!(!plan.guard_tripped, "1/3 = 33%, under guard");
        assert_eq!(plan.to_delete.len(), 1);
        assert_eq!(plan.to_delete[0].remote_id, "sys002");
        assert!(plan.to_patch.is_empty(), "no drift");
        assert!(plan.missing_fleet_ids.is_empty(), "no missing");

        // Execute the plan.
        for row in &plan.to_delete {
            client.delete_system(&token, &row.remote_id).await.unwrap();
        }
        // wiremock verifies the DELETE was called exactly once.
    }
}

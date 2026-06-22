//! `fleet sync` — the multi-tailnet pull → merge → persist → export pipeline
//! (spec §3.4–§3.6).
//!
//! ## Additive on partial failure (R-source-of-truth)
//! A tailnet that fails to fetch (auth error, 5xx, network) is **logged and
//! skipped** — its existing DB rows are left untouched. Only accounts that
//! fetched successfully take part in the epoch sweep, so a transient blip never
//! wipes a client's inventory.
//!
//! ## Pipeline
//! 1. open DB, insert a `sync_run`, get `run_id`
//! 2. load overrides; build `PriorIds` from `node_seen.fuzzy_hint`
//! 3. per tailnet: resolve secret → oauth → list devices; collect per-account
//!    lists; record successes
//! 4. pure `merge(...)`
//! 5. `overrides::apply` each node (tag layering + tier derivation)
//! 6. upsert each node + its `node_seen` provenance (stamped with `run_id`)
//! 7. record succeeded accounts on the run
//! 8. epoch-sweep each succeeded account; mark orphaned nodes stale
//! 9. write the stable `fleet.yaml` snapshot

use crate::config::Config;
use crate::merge::{self, PriorIds};
use crate::model::TsDevice;
use crate::tailscale::TsClient;
use crate::{db, export, overrides, secrets};
use std::path::Path;
use std::time::Duration;

/// Resolve a tailnet's OAuth secret from its config. The default
/// ([`default_secret_resolver`]) uses env + Keychain; tests inject a stub to
/// avoid env-var races across parallel async tests.
pub type SecretResolver = dyn Fn(&crate::config::TailnetConfig) -> anyhow::Result<String> + Sync;

/// Production secret resolver: env (`FLEET_<env>`) then macOS Keychain.
pub fn default_secret_resolver(tn: &crate::config::TailnetConfig) -> anyhow::Result<String> {
    secrets::resolve(&tn.oauth_secret_env, &tn.oauth_secret_env)
}

/// Run the sync pipeline with the default (env+Keychain) secret resolver.
/// `ts_base_url` is injectable so tests can point at a wiremock server
/// (production passes `https://api.tailscale.com`).
pub async fn run(
    cfg: &Config,
    overrides_path: &Path,
    db_path: &Path,
    ts_base_url: &str,
) -> anyhow::Result<()> {
    run_with_resolver(
        cfg,
        overrides_path,
        db_path,
        ts_base_url,
        &default_secret_resolver,
    )
    .await
}

/// Run the sync pipeline with an explicit secret resolver (testable core).
pub async fn run_with_resolver(
    cfg: &Config,
    overrides_path: &Path,
    db_path: &Path,
    ts_base_url: &str,
    resolve_secret: &SecretResolver,
) -> anyhow::Result<()> {
    let conn = db::open(db_path)?;
    let run_id = db::insert_sync_run(&conn)?;

    let full_overrides = overrides::load(overrides_path)?;
    let merge_overrides = full_overrides.to_merge_overrides();

    // Build PriorIds from existing node_seen.fuzzy_hint rows.
    let prior = build_prior_ids(&conn)?;

    // ── Fetch each tailnet (additive on failure) ────────────────────────────
    let mut per_account: Vec<(String, Vec<TsDevice>)> = Vec::new();
    let mut succeeded: Vec<String> = Vec::new();
    let client = TsClient::new(ts_base_url);

    for tn in &cfg.tailnets {
        match fetch_tailnet(&client, tn, resolve_secret).await {
            Ok(devices) => {
                succeeded.push(tn.name.clone());
                per_account.push((tn.name.clone(), devices));
            }
            Err(e) => {
                eprintln!(
                    "sync: tailnet `{}` failed, skipping (rows preserved): {}",
                    tn.name,
                    secrets::redact(e)
                );
            }
        }
    }

    // ── Merge ───────────────────────────────────────────────────────────────
    let threshold = Duration::from_secs(cfg.online_threshold_secs);
    let mut nodes = merge::merge(
        per_account,
        &merge_overrides,
        &prior,
        threshold,
        cfg.include_unauthorized,
    );

    // ── Layer overrides + derive tier, then persist ─────────────────────────
    for node in &mut nodes {
        overrides::apply(node, &full_overrides);
        db::nodes::upsert_node(&conn, node)?;
        for sref in &node.seen_in {
            // machine_key for this provenance row: look it up from the source
            // device list is not retained post-merge; node_seen.machine_key is
            // best-effort and not load-bearing for re-link (fuzzy_hint is).
            db::nodes::upsert_node_seen(
                &conn,
                &sref.account,
                &sref.device_id,
                &node.fleet_id,
                "",
                node.fuzzy_hint.as_deref(),
                &node.last_seen.to_rfc3339(),
                run_id,
            )?;
        }
    }

    // ── Record successes + epoch sweep (scoped to succeeded accounts) ───────
    db::update_sync_run_accounts(&conn, run_id, &succeeded)?;
    for account in &succeeded {
        db::nodes::sweep_epoch(&conn, account, run_id)?;
    }
    db::nodes::mark_stale_nodes(&conn)?;

    // ── Export the stable snapshot ──────────────────────────────────────────
    let all = db::nodes::list(&conn)?;
    export::write_fleet_yaml(&all, Path::new(&cfg.export_yaml_path))?;

    Ok(())
}

/// Build [`PriorIds`] from `node_seen.fuzzy_hint` (non-empty hints only).
fn build_prior_ids(conn: &rusqlite::Connection) -> anyhow::Result<PriorIds> {
    let mut prior = PriorIds::default();
    let mut stmt =
        conn.prepare("SELECT fuzzy_hint, node_id FROM node_seen WHERE fuzzy_hint != ''")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (hint, node_id) = row?;
        prior.by_fuzzy_hint.entry(hint).or_insert(node_id);
    }
    Ok(prior)
}

/// Resolve a tailnet's secret, exchange for a bearer, and list its devices.
async fn fetch_tailnet(
    client: &TsClient,
    tn: &crate::config::TailnetConfig,
    resolve_secret: &SecretResolver,
) -> anyhow::Result<Vec<TsDevice>> {
    let secret = resolve_secret(tn)?;
    let token = client.oauth_token(&tn.oauth_client_id, &secret).await?;
    let devices = client.devices(&tn.tailnet, &token).await?;
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, TailnetConfig};
    use tempfile::tempdir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn base_config(db_path: &str, yaml_path: &str, tailnets: Vec<TailnetConfig>) -> Config {
        Config {
            db_path: db_path.to_owned(),
            export_yaml_path: yaml_path.to_owned(),
            online_threshold_secs: 900,
            ssh_user: "root".to_owned(),
            include_unauthorized: false,
            include_external: false,
            tailnets,
            beszel: None,
            kuma: None,
            cloudflare: None,
            ntfy: None,
            healthchecks: None,
            probe: None,
            serve: None,
        }
    }

    fn tailnet(name: &str, env: &str) -> TailnetConfig {
        TailnetConfig {
            name: name.to_owned(),
            oauth_client_id: "cid".to_owned(),
            oauth_secret_env: env.to_owned(),
            tailnet: "-".to_owned(),
        }
    }

    /// A secret resolver that always returns a fixed token (no env-var races).
    fn stub_secret(_tn: &TailnetConfig) -> anyhow::Result<String> {
        Ok("secret".to_owned())
    }

    /// Mount an OAuth endpoint that always returns a token.
    async fn mount_oauth(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/api/v2/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "test-token", "token_type": "Bearer"
            })))
            .mount(server)
            .await;
    }

    fn device_json(id: &str, host: &str, last_seen: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "hostname": host, "name": format!("{host}.ts.net"),
            "machineKey": format!("mk:{id}"), "os": "linux",
            "addresses": ["100.64.0.1"], "tags": ["tag:owner-self", "tag:role-worker"],
            "isExternal": false, "authorized": true, "lastSeen": last_seen
        })
    }

    #[tokio::test]
    async fn additive_on_account_failure() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("fleet.db");
        let yaml_path = dir.path().join("fleet.yaml");

        // Pre-seed a client-acme node + provenance directly in the DB.
        {
            let conn = db::open(&db_path).unwrap();
            let run0 = db::insert_sync_run(&conn).unwrap();
            let n = crate::overrides::tests_helper_node("mk:pre", "linux");
            crate::db::nodes::upsert_node(&conn, &n).unwrap();
            db::nodes::upsert_node_seen(
                &conn,
                "client-acme",
                "preDev",
                "mk:pre",
                "mk:pre",
                None,
                "t",
                run0,
            )
            .unwrap();
        }

        // personal succeeds; client-acme returns 500 on devices.
        let server = MockServer::start().await;
        mount_oauth(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/tailnet/-/devices"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "devices": [device_json("p1", "pbox", "2026-06-20T10:00:00Z")]
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // The second devices call (client-acme) — also matches /-/devices; to
        // force a 500 for it we instead mount a failing variant with higher
        // priority via a separate path. Simpler: client-acme uses tailnet "fail".
        Mock::given(method("GET"))
            .and(path("/api/v2/tailnet/fail/devices"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let mut t_ok = tailnet("personal", "FLEET_TEST_OK");
        t_ok.tailnet = "-".to_owned();
        let mut t_fail = tailnet("client-acme", "FLEET_TEST_FAIL");
        t_fail.tailnet = "fail".to_owned();

        let cfg = base_config(
            db_path.to_str().unwrap(),
            yaml_path.to_str().unwrap(),
            vec![t_ok, t_fail],
        );
        let ov = dir.path().join("ov.yaml");
        std::fs::write(&ov, "").unwrap();

        run_with_resolver(&cfg, &ov, &db_path, &server.uri(), &stub_secret)
            .await
            .unwrap();

        let conn = db::open(&db_path).unwrap();
        // personal box upserted
        let pcnt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM node_seen WHERE account='personal'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pcnt, 1, "personal provenance upserted");
        // client-acme provenance STILL present (additive)
        let ccnt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM node_seen WHERE account='client-acme'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ccnt, 1, "client-acme provenance preserved on failure");
        // accounts_ok = ["personal"] for the latest run
        let json: String = conn
            .query_row(
                "SELECT accounts_ok FROM sync_run ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let ok: Vec<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(ok, vec!["personal"]);
    }

    #[tokio::test]
    async fn epoch_scoped_sweep_deletes_absent_provenance() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("fleet.db");
        let yaml_path = dir.path().join("fleet.yaml");
        let ov = dir.path().join("ov.yaml");
        std::fs::write(&ov, "").unwrap();

        let cfg = base_config(
            db_path.to_str().unwrap(),
            yaml_path.to_str().unwrap(),
            vec![tailnet("personal", "FLEET_TEST_OK")],
        );

        // Run 1: personal has devX.
        let server1 = MockServer::start().await;
        mount_oauth(&server1).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/tailnet/-/devices"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "devices": [device_json("devX", "boxX", "2026-06-20T10:00:00Z")]
            })))
            .mount(&server1)
            .await;
        run_with_resolver(&cfg, &ov, &db_path, &server1.uri(), &stub_secret)
            .await
            .unwrap();

        {
            let conn = db::open(&db_path).unwrap();
            let cnt: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM node_seen WHERE device_id='devX'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(cnt, 1, "devX present after run 1");
        }

        // Run 2: personal succeeds but devX is absent (empty device list).
        let server2 = MockServer::start().await;
        mount_oauth(&server2).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/tailnet/-/devices"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "devices": [] })),
            )
            .mount(&server2)
            .await;
        run_with_resolver(&cfg, &ov, &db_path, &server2.uri(), &stub_secret)
            .await
            .unwrap();

        let conn = db::open(&db_path).unwrap();
        let cnt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM node_seen WHERE device_id='devX'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt, 0, "absent devX swept in run 2");
        // The node itself is stale-marked, not deleted.
        let ncnt: i64 = conn
            .query_row("SELECT COUNT(*) FROM node WHERE stale=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ncnt, 1, "orphaned node stale-marked");
    }

    #[tokio::test]
    async fn stale_marked_not_deleted() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("fleet.db");
        let yaml_path = dir.path().join("fleet.yaml");
        let ov = dir.path().join("ov.yaml");
        std::fs::write(&ov, "").unwrap();
        let cfg = base_config(
            db_path.to_str().unwrap(),
            yaml_path.to_str().unwrap(),
            vec![tailnet("personal", "FLEET_TEST_OK")],
        );

        // Run 1: one box.
        let s1 = MockServer::start().await;
        mount_oauth(&s1).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/tailnet/-/devices"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "devices": [device_json("devX", "boxX", "2026-06-20T10:00:00Z")]
            })))
            .mount(&s1)
            .await;
        run_with_resolver(&cfg, &ov, &db_path, &s1.uri(), &stub_secret)
            .await
            .unwrap();

        let fleet_id = {
            let conn = db::open(&db_path).unwrap();
            conn.query_row("SELECT fleet_id FROM node LIMIT 1", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
        };

        // Run 2: box gone.
        let s2 = MockServer::start().await;
        mount_oauth(&s2).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/tailnet/-/devices"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "devices": [] })),
            )
            .mount(&s2)
            .await;
        run_with_resolver(&cfg, &ov, &db_path, &s2.uri(), &stub_secret)
            .await
            .unwrap();

        let conn = db::open(&db_path).unwrap();
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM node WHERE fleet_id=?1",
                [&fleet_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "node row still present");
        assert_eq!(
            db::nodes::is_stale(&conn, &fleet_id).unwrap(),
            Some(true),
            "node is stale-marked"
        );
    }

    #[tokio::test]
    async fn yaml_excludes_volatile_fields() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("fleet.db");
        let yaml_path = dir.path().join("fleet.yaml");
        let ov = dir.path().join("ov.yaml");
        std::fs::write(&ov, "").unwrap();
        let cfg = base_config(
            db_path.to_str().unwrap(),
            yaml_path.to_str().unwrap(),
            vec![tailnet("personal", "FLEET_TEST_OK")],
        );

        // Run 1 with one last_seen.
        let s1 = MockServer::start().await;
        mount_oauth(&s1).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/tailnet/-/devices"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "devices": [device_json("devX", "boxX", "2026-06-20T10:00:00Z")]
            })))
            .mount(&s1)
            .await;
        run_with_resolver(&cfg, &ov, &db_path, &s1.uri(), &stub_secret)
            .await
            .unwrap();
        let yaml1 = std::fs::read_to_string(&yaml_path).unwrap();

        // Run 2 with a DIFFERENT last_seen for the same box.
        let s2 = MockServer::start().await;
        mount_oauth(&s2).await;
        Mock::given(method("GET"))
            .and(path("/api/v2/tailnet/-/devices"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "devices": [device_json("devX", "boxX", "2026-06-21T18:00:00Z")]
            })))
            .mount(&s2)
            .await;
        run_with_resolver(&cfg, &ov, &db_path, &s2.uri(), &stub_secret)
            .await
            .unwrap();
        let yaml2 = std::fs::read_to_string(&yaml_path).unwrap();

        assert_eq!(yaml1, yaml2, "YAML identical across last_seen change");
        assert!(!yaml1.contains("last_seen"));
        assert!(!yaml1.contains("online"));
        assert!(!yaml1.contains("updated_at"));
    }
}

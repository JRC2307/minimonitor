//! `fleet cf-sync` — read-only Cloudflare pull.
//!
//! Pipeline (spec §3.7 / fleet cf-sync):
//! 1. Resolve CF token
//! 2. `verify_token` → abort on failure
//! 3. Paginate `GET /zones` → collect zones, derive `healthy`
//! 4. Per zone: `GET /zones/{id}/ssl/certificate_packs?status=all` → `min_cert_expiry`
//! 5. Upsert each zone into `cf_zone`
//! 6. Evaluate thresholds; ntfy at priority 4 on:
//!    - zone unhealthy
//!    - `min_cert_expiry` within `ssl_warn_days`

use crate::alert;
use crate::cloudflare::CfClient;
use crate::config::{CloudflareConfig, NtfyConfig};
use crate::db;
use crate::db::cf::upsert_cf_zone;
use crate::secrets;
use anyhow::Context;
use chrono::Utc;
use std::path::Path;

const CF_API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// Run cf-sync with production defaults (real CF API + real ntfy + macOS Keychain).
pub async fn run(
    cf_cfg: &CloudflareConfig,
    ntfy_cfg: Option<&NtfyConfig>,
    db_path: &Path,
) -> anyhow::Result<()> {
    run_with_base(cf_cfg, ntfy_cfg, db_path, CF_API_BASE, None).await
}

/// Run cf-sync with injectable base URLs (for tests).
///
/// `cf_base` — override for the CF API base URL (wiremock in tests).
/// `ntfy_base` — override for the ntfy base URL (wiremock in tests).
pub async fn run_with_base(
    cf_cfg: &CloudflareConfig,
    ntfy_cfg: Option<&NtfyConfig>,
    db_path: &Path,
    cf_base: &str,
    ntfy_base: Option<&str>,
) -> anyhow::Result<()> {
    // 1. Resolve token
    let token = secrets::resolve(&cf_cfg.token_env, "fleet-cf-token")
        .context("cf-sync: resolving Cloudflare token")?;

    let cf = CfClient::new(cf_base, &token);

    // 2. Verify token — abort if invalid
    cf.verify_token()
        .await
        .context("cf-sync: token verification failed")?;

    // 3. Fetch all zones (paginated)
    let mut zones = cf.zones().await.context("cf-sync: fetching zones")?;

    // 4. Fetch cert-packs per zone and fold min_cert_expiry
    for zone in &mut zones {
        match cf.cert_packs(&zone.id).await {
            Ok(min_expiry) => zone.min_cert_expiry = min_expiry,
            Err(e) => {
                eprintln!(
                    "cf-sync: cert_packs({}) failed, skipping SSL check: {e}",
                    zone.name
                );
            }
        }
    }

    // 5. Open DB and upsert
    let conn = db::open(db_path)?;
    for zone in &zones {
        upsert_cf_zone(&conn, zone)
            .with_context(|| format!("cf-sync: upserting zone {}", zone.id))?;
    }

    // 6. Evaluate thresholds and alert
    if let Some(ntfy) = ntfy_cfg {
        evaluate_and_alert(&zones, cf_cfg.ssl_warn_days, ntfy, ntfy_base).await;
    }

    Ok(())
}

/// Evaluate zone health and SSL expiry, emit ntfy alerts at priority 4 on breach.
async fn evaluate_and_alert(
    zones: &[crate::cloudflare::CfZone],
    ssl_warn_days: u32,
    ntfy_cfg: &NtfyConfig,
    ntfy_base: Option<&str>,
) {
    let now = Utc::now();
    for zone in zones {
        // Unhealthy zone alert
        if !zone.healthy {
            let msg = format!(
                "Zone {} is unhealthy (status={}, paused={})",
                zone.name, zone.status, zone.paused
            );
            if let Err(e) =
                alert::ntfy_with_base(ntfy_cfg, "Fleet: Unhealthy Zone", &msg, 4, ntfy_base).await
            {
                eprintln!("cf-sync: ntfy alert failed for zone {}: {e}", zone.name);
            }
        }

        // SSL expiry alert
        if let Some(expiry) = zone.min_cert_expiry {
            let days_left = (expiry - now).num_days();
            if days_left <= ssl_warn_days as i64 {
                let msg = format!(
                    "Zone {} SSL cert expires in {} days ({})",
                    zone.name,
                    days_left,
                    expiry.format("%Y-%m-%d")
                );
                if let Err(e) =
                    alert::ntfy_with_base(ntfy_cfg, "Fleet: SSL Cert Expiring", &msg, 4, ntfy_base)
                        .await
                {
                    eprintln!("cf-sync: ntfy SSL alert failed for zone {}: {e}", zone.name);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloudflare::CfZone;
    use crate::config::{CloudflareConfig, NtfyConfig};
    use crate::db;
    use crate::db::cf::get_cf_zone;
    use chrono::{Duration, Utc};
    use tempfile::NamedTempFile;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cf_cfg(token_env: &str) -> CloudflareConfig {
        CloudflareConfig {
            token_env: token_env.to_owned(),
            ssl_warn_days: 14,
        }
    }

    fn open_temp_db() -> (NamedTempFile, std::path::PathBuf) {
        let f = NamedTempFile::new().unwrap();
        let p = f.path().to_path_buf();
        (f, p)
    }

    async fn mount_verify_ok(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/user/tokens/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true, "errors": [], "result": {"id": "tok", "status": "active"}
            })))
            .mount(server)
            .await;
    }

    async fn mount_single_zone(server: &MockServer, zone_id: &str, name: &str, healthy: bool) {
        let status = if healthy { "active" } else { "inactive" };
        Mock::given(method("GET"))
            .and(path("/zones"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true, "errors": [],
                "result": [{"id": zone_id, "name": name, "status": status, "paused": false}],
                "result_info": {"page": 1, "total_pages": 1}
            })))
            .mount(server)
            .await;
    }

    async fn mount_cert_packs_empty(server: &MockServer, zone_id: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/zones/{zone_id}/ssl/certificate_packs")))
            .and(query_param("status", "all"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true, "errors": [],
                "result": [],
                "result_info": {"page": 1, "total_pages": 1}
            })))
            .mount(server)
            .await;
    }

    // ── Token verification failure aborts ───────────────────────────────────

    #[tokio::test]
    async fn verify_failure_aborts_no_zones_call() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/tokens/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": false,
                "errors": [{"code": 1000, "message": "invalid token"}],
                "result": null
            })))
            .mount(&server)
            .await;

        // zones mock — should NEVER be called
        Mock::given(method("GET"))
            .and(path("/zones"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true, "errors": [], "result": [],
                "result_info": {"page": 1, "total_pages": 1}
            })))
            .mount(&server)
            .await;

        let (_f, db_path) = open_temp_db();
        unsafe { std::env::set_var("FLEET_CF_TEST_TOKEN", "bad-token") };
        let cfg = cf_cfg("FLEET_CF_TEST_TOKEN");

        let err = run_with_base(&cfg, None, &db_path, &server.uri(), None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("token verification failed")
                || err.to_string().contains("success=false"),
            "unexpected error: {err}"
        );

        let reqs = server.received_requests().await.unwrap();
        let zones_calls = reqs.iter().filter(|r| r.url.path() == "/zones").count();
        assert_eq!(
            zones_calls, 0,
            "zones must not be called after token failure"
        );
    }

    // ── cf_zone upsert round-trip ────────────────────────────────────────────

    #[tokio::test]
    async fn upsert_round_trip_via_run() {
        let server = MockServer::start().await;
        mount_verify_ok(&server).await;
        mount_single_zone(&server, "zABC", "example.com", true).await;
        mount_cert_packs_empty(&server, "zABC").await;

        let (_f, db_path) = open_temp_db();
        unsafe { std::env::set_var("FLEET_CF_TEST_TOKEN2", "test-token") };
        let cfg = cf_cfg("FLEET_CF_TEST_TOKEN2");

        run_with_base(&cfg, None, &db_path, &server.uri(), None)
            .await
            .unwrap();

        let conn = db::open(&db_path).unwrap();
        let zone = get_cf_zone(&conn, "zABC").unwrap().unwrap();
        assert_eq!(zone.name, "example.com");
        assert_eq!(zone.status, "active");
        assert!(zone.healthy);
        assert!(zone.min_cert_expiry.is_none());
    }

    // ── SSL expiry threshold alert ───────────────────────────────────────────

    #[tokio::test]
    async fn ssl_expiry_within_warn_days_triggers_ntfy_priority_4() {
        let ntfy_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/fleet"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"priority": 4}),
            ))
            .respond_with(ResponseTemplate::new(200))
            .expect(1) // exactly one ntfy call for SSL warn
            .mount(&ntfy_server)
            .await;

        unsafe { std::env::set_var("FLEET_TEST_NTFY_TOKEN", "ntfy-tok") };
        let ntfy = NtfyConfig {
            base_url: ntfy_server.uri(),
            topic: "fleet".to_owned(),
            token_env: "FLEET_TEST_NTFY_TOKEN".to_owned(),
        };

        // A healthy zone with cert expiring in 7 days (within ssl_warn_days=14)
        let expiry = Utc::now() + Duration::days(7);
        let zones = vec![CfZone {
            id: "z1".to_owned(),
            name: "example.com".to_owned(),
            status: "active".to_owned(),
            paused: false,
            healthy: true,
            min_cert_expiry: Some(expiry),
        }];

        evaluate_and_alert(&zones, 14, &ntfy, Some(&ntfy_server.uri())).await;
        ntfy_server.verify().await;
    }

    #[tokio::test]
    async fn ssl_expiry_outside_warn_days_no_ntfy() {
        let ntfy_server = MockServer::start().await;
        // Mount with expect(0) — if called, verify() fails
        Mock::given(method("POST"))
            .and(path("/fleet"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&ntfy_server)
            .await;

        unsafe { std::env::set_var("FLEET_TEST_NTFY_TOKEN", "ntfy-tok") };
        let ntfy = NtfyConfig {
            base_url: ntfy_server.uri(),
            topic: "fleet".to_owned(),
            token_env: "FLEET_TEST_NTFY_TOKEN".to_owned(),
        };

        // cert expires in 30 days — beyond ssl_warn_days=14
        let expiry = Utc::now() + Duration::days(30);
        let zones = vec![CfZone {
            id: "z1".to_owned(),
            name: "safe.com".to_owned(),
            status: "active".to_owned(),
            paused: false,
            healthy: true,
            min_cert_expiry: Some(expiry),
        }];

        evaluate_and_alert(&zones, 14, &ntfy, Some(&ntfy_server.uri())).await;
        ntfy_server.verify().await;
    }

    // ── Unhealthy zone triggers ntfy at priority 4 ───────────────────────────

    #[tokio::test]
    async fn unhealthy_zone_triggers_ntfy_priority_4() {
        let ntfy_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/fleet"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"priority": 4}),
            ))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&ntfy_server)
            .await;

        unsafe { std::env::set_var("FLEET_TEST_NTFY_TOKEN", "ntfy-tok") };
        let ntfy = NtfyConfig {
            base_url: ntfy_server.uri(),
            topic: "fleet".to_owned(),
            token_env: "FLEET_TEST_NTFY_TOKEN".to_owned(),
        };

        let zones = vec![CfZone {
            id: "z1".to_owned(),
            name: "down.com".to_owned(),
            status: "inactive".to_owned(),
            paused: false,
            healthy: false,
            min_cert_expiry: None,
        }];

        evaluate_and_alert(&zones, 14, &ntfy, Some(&ntfy_server.uri())).await;
        ntfy_server.verify().await;
    }

    #[tokio::test]
    async fn healthy_zone_no_expiry_no_ntfy() {
        let ntfy_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/fleet"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&ntfy_server)
            .await;

        unsafe { std::env::set_var("FLEET_TEST_NTFY_TOKEN", "ntfy-tok") };
        let ntfy = NtfyConfig {
            base_url: ntfy_server.uri(),
            topic: "fleet".to_owned(),
            token_env: "FLEET_TEST_NTFY_TOKEN".to_owned(),
        };

        let zones = vec![CfZone {
            id: "z1".to_owned(),
            name: "fine.com".to_owned(),
            status: "active".to_owned(),
            paused: false,
            healthy: true,
            min_cert_expiry: None,
        }];

        evaluate_and_alert(&zones, 14, &ntfy, Some(&ntfy_server.uri())).await;
        ntfy_server.verify().await;
    }
}

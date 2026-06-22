//! `fleet enroll` — idempotent reconcile of agent-tier nodes into Beszel.
//!
//! ## Pipeline (agent tier → Beszel)
//!
//! 1. Auth against the `users` collection (raw token, no Bearer prefix).
//! 2. List all Beszel systems.
//! 3. List desired agent-tier nodes from the local DB.
//! 4. List Beszel enrollment rows from the local DB.
//! 5. `plan_reconcile` (pure) → `ReconcilePlan`.
//! 6. If `--dry-run`, print the plan and exit.
//! 7. Execute: PATCH drifted systems, delete decommissioned (if guard ok),
//!    enable bootstrap token on-demand (only when missing agents exist).
//! 8. Upsert / delete enrollment rows.
//!
//! ## Key constraints
//!
//! - **Never blind-create**: if an agent has no matching Beszel system, enable
//!   the bootstrap token so the agent can self-register; never POST a create.
//! - **On-demand token**: only enabled when ≥1 desired agent has no system.
//! - **40% delete-guard**: abort and delete nothing if >40% of existing systems
//!   would be decommissioned in one run.
//! - **Idempotent**: a second run with no drift is a no-op.

use crate::beszel::{self, BeszelClient, DesiredAgent, PatchSystemRequest, plan_reconcile};
use crate::config::{BeszelConfig, Config};
use crate::db;
use crate::model::Tier;
use crate::secrets;
use anyhow::bail;

/// Run the Beszel enroll pipeline.
///
/// `beszel_base_url` is injectable so tests can point at a wiremock server.
/// `dry_run` prints the plan without making any changes.
pub async fn run(
    cfg: &Config,
    db_path: &std::path::Path,
    beszel_base_url: &str,
    dry_run: bool,
) -> anyhow::Result<()> {
    let bz_cfg = cfg
        .beszel
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("fleet enroll: [beszel] section missing from config"))?;

    let password = resolve_beszel_password(bz_cfg)?;
    run_with_password(cfg, db_path, beszel_base_url, dry_run, &password).await
}

/// Run the Beszel enroll pipeline with an explicit password (testable core).
pub async fn run_with_password(
    cfg: &Config,
    db_path: &std::path::Path,
    beszel_base_url: &str,
    dry_run: bool,
    password: &str,
) -> anyhow::Result<()> {
    let bz_cfg = cfg
        .beszel
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("fleet enroll: [beszel] section missing from config"))?;

    // ── Auth ─────────────────────────────────────────────────────────────────
    let client = BeszelClient::new(beszel_base_url);
    let token = client.auth_with_password(&bz_cfg.user, password).await?;

    // ── Fetch existing systems from Beszel ────────────────────────────────────
    let existing_systems = client.list_systems(&token, None).await?;

    // ── Load desired agents + enrollment rows from local DB ───────────────────
    let conn = db::open(db_path)?;
    let all_nodes = db::nodes::list(&conn)?;

    // Filter to agent-tier nodes; build DesiredAgent list using first 100.x address.
    let desired: Vec<DesiredAgent> = all_nodes
        .iter()
        .filter(|n| n.tier == Tier::Agent)
        .filter_map(|n| {
            // Pick the first tailnet IP (100.x). If a node has no tailnet IP,
            // skip it (can't match against Beszel host).
            let tailnet_ip = n
                .addresses
                .iter()
                .find(|a| a.starts_with("100."))
                .cloned()?;
            Some(DesiredAgent {
                fleet_id: n.fleet_id.clone(),
                friendly_name: n.hostname.clone(),
                tailnet_ip,
            })
        })
        .collect();

    let enrolled = beszel::list_enrollments(&conn)?;

    // ── Plan ──────────────────────────────────────────────────────────────────
    let plan = plan_reconcile(&desired, &existing_systems, &enrolled);

    if dry_run {
        println!("fleet enroll --dry-run:");
        println!("  patch (drift):      {} systems", plan.to_patch.len());
        for (agent, sys) in &plan.to_patch {
            println!(
                "    PATCH {} (remote_id={}) name: {:?} → {:?}",
                agent.fleet_id, sys.id, sys.name, agent.friendly_name
            );
        }
        println!(
            "  missing (no system): {} agents (bootstrap token needed: {})",
            plan.missing_fleet_ids.len(),
            !plan.missing_fleet_ids.is_empty()
        );
        for fid in &plan.missing_fleet_ids {
            println!("    {fid}");
        }
        println!("  decommission:       {} systems", plan.to_delete.len());
        for row in &plan.to_delete {
            println!(
                "    DELETE fleet_id={} remote_id={}",
                row.fleet_id, row.remote_id
            );
        }
        if plan.guard_tripped {
            println!(
                "  GUARD TRIPPED: >{}% of existing systems would be deleted — aborting",
                beszel::DELETE_GUARD_PCT
            );
        }
        return Ok(());
    }

    // ── Guard check ───────────────────────────────────────────────────────────
    if plan.guard_tripped {
        bail!(
            "fleet enroll: delete-guard tripped — more than {}% of existing Beszel \
             systems would be decommissioned in one run. Aborting. \
             Use --dry-run to inspect the plan.",
            beszel::DELETE_GUARD_PCT
        );
    }

    // ── Execute: PATCH drifted systems ────────────────────────────────────────
    for (agent, sys) in &plan.to_patch {
        eprintln!(
            "fleet enroll: PATCH system {} (fleet_id={}) name {:?} → {:?}",
            sys.id, agent.fleet_id, sys.name, agent.friendly_name
        );
        client
            .patch_system(
                &token,
                &sys.id,
                &PatchSystemRequest {
                    name: Some(agent.friendly_name.clone()),
                    users: None,
                },
            )
            .await?;

        // Record/update the enrollment row (remote_id from the existing system).
        beszel::upsert_enrollment(&conn, &agent.fleet_id, &sys.id)?;
    }

    // Also upsert enrollment for existing systems that match desired but had no
    // drift (they may not have an enrollment row yet if this is the first run
    // after an agent self-registered).
    let by_host: std::collections::HashMap<&str, &crate::beszel::BeszelSystem> = existing_systems
        .iter()
        .map(|s| (s.host.as_str(), s))
        .collect();

    for agent in &desired {
        if let Some(sys) = by_host.get(agent.tailnet_ip.as_str()) {
            // Ensure the enrollment row exists (no-op if already correct).
            beszel::upsert_enrollment(&conn, &agent.fleet_id, &sys.id)?;
        }
    }

    // ── Execute: on-demand bootstrap token ───────────────────────────────────
    if !plan.missing_fleet_ids.is_empty() {
        eprintln!(
            "fleet enroll: {} agent(s) not yet registered in Beszel — enabling bootstrap token",
            plan.missing_fleet_ids.len()
        );
        for fid in &plan.missing_fleet_ids {
            eprintln!("  missing: {fid}");
        }
        client.enable_bootstrap_token(&token).await?;
    }

    // ── Execute: decommission ─────────────────────────────────────────────────
    for row in &plan.to_delete {
        eprintln!(
            "fleet enroll: DELETE system {} (fleet_id={})",
            row.remote_id, row.fleet_id
        );
        client.delete_system(&token, &row.remote_id).await?;
        beszel::delete_enrollment(&conn, &row.fleet_id)?;
    }

    eprintln!(
        "fleet enroll: done (patched={}, missing={}, deleted={})",
        plan.to_patch.len(),
        plan.missing_fleet_ids.len(),
        plan.to_delete.len()
    );

    Ok(())
}

/// Resolve the Beszel password from the configured env var / Keychain.
fn resolve_beszel_password(cfg: &BeszelConfig) -> anyhow::Result<String> {
    secrets::resolve(&cfg.password_env, &cfg.password_env)
}

// ─── Kuma half (agentless tier → Uptime-Kuma, spec §3.7) ─────────────────────

use crate::config::KumaConfig;
use crate::kuma::sio::SioKumaClient;
use crate::kuma::{self, DELETE_GUARD_PCT, KumaClient, reconcile};
use crate::model::{MonitorSpec, Node};

/// Build the desired [`MonitorSpec`] for an agentless node (spec §3.7):
///
/// - a `notes` URL (`http(s)://…`) → an **http** monitor (app-level liveness),
/// - otherwise the node's first tailnet `100.x` IP → a **ping** monitor (raw
///   liveness). Nodes with neither are skipped (nothing to monitor).
///
/// `name` = `fleet_id` (the idempotency key). `ntfy_id` wires the configured
/// ntfy notification (`0` → none).
fn monitor_spec_for(node: &Node, ntfy_id: i64) -> Option<MonitorSpec> {
    if let Some(url) = node
        .notes
        .as_deref()
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
    {
        return Some(MonitorSpec::http(&node.fleet_id, url, ntfy_id));
    }
    let ip = node.addresses.iter().find(|a| a.starts_with("100."))?;
    Some(MonitorSpec::ping(&node.fleet_id, ip, ntfy_id))
}

/// Run the Kuma (agentless-tier) enroll pipeline.
///
/// Connects + logs in over socket.io, builds desired `MonitorSpec`s from the
/// agentless nodes, runs the pure [`reconcile`] (40% delete-guard), and records
/// each resolved `monitorID` in `enrollment(system='kuma')`. `--dry-run` prints
/// the desired set and the server inventory without mutating anything.
pub async fn run_kuma(
    cfg: &Config,
    db_path: &std::path::Path,
    dry_run: bool,
) -> anyhow::Result<()> {
    let kuma_cfg = cfg
        .kuma
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("fleet enroll: [kuma] section missing from config"))?;
    let password = resolve_kuma_password(kuma_cfg)?;
    run_kuma_with_password(cfg, db_path, dry_run, &password).await
}

/// Kuma enroll with an explicit password (testable core / injectable URL via cfg).
pub async fn run_kuma_with_password(
    cfg: &Config,
    db_path: &std::path::Path,
    dry_run: bool,
    password: &str,
) -> anyhow::Result<()> {
    let kuma_cfg = cfg
        .kuma
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("fleet enroll: [kuma] section missing from config"))?;

    // ── Desired monitors from agentless nodes ─────────────────────────────────
    let conn = db::open(db_path)?;
    let all_nodes = db::nodes::list(&conn)?;
    let want: Vec<MonitorSpec> = all_nodes
        .iter()
        .filter(|n| n.tier == Tier::Agentless)
        .filter_map(|n| monitor_spec_for(n, kuma_cfg.ntfy_notification_id))
        .collect();

    // ── Connect + login (the push-based dance lives in sio.rs) ────────────────
    let client = SioKumaClient::connect_and_login(&kuma_cfg.url, &kuma_cfg.user, password).await?;
    let have = client.list().await?;

    if dry_run {
        println!("fleet enroll (kuma) --dry-run:");
        println!("  desired agentless monitors: {}", want.len());
        for w in &want {
            println!("    {} ({:?})", w.name, w.monitor_type);
        }
        println!("  existing kuma monitors:     {}", have.len());
        return Ok(());
    }

    // ── Reconcile (pure planner + execution behind the trait) ─────────────────
    reconcile(&client, &want, DELETE_GUARD_PCT as u8).await?;

    // ── Record resolved monitorIDs in enrollment ──────────────────────────────
    // Re-read the server state so freshly-added monitors get their ids stored.
    let after = client.list().await?;
    let id_by_name: std::collections::HashMap<&str, i64> =
        after.iter().map(|m| (m.name.as_str(), m.id)).collect();

    let desired_names: std::collections::HashSet<&str> =
        want.iter().map(|w| w.name.as_str()).collect();

    for w in &want {
        if let Some(id) = id_by_name.get(w.name.as_str()) {
            kuma::upsert_enrollment(&conn, &w.name, &id.to_string())?;
        }
    }
    // Drop enrollment rows for monitors no longer desired (and now deleted).
    for row in kuma::list_enrollments(&conn)? {
        if !desired_names.contains(row.fleet_id.as_str()) {
            kuma::delete_enrollment(&conn, &row.fleet_id)?;
        }
    }

    eprintln!("fleet enroll (kuma): reconciled {} monitor(s)", want.len());
    Ok(())
}

/// Resolve the Kuma password from the configured env var / Keychain.
fn resolve_kuma_password(cfg: &KumaConfig) -> anyhow::Result<String> {
    secrets::resolve(&cfg.password_env, &cfg.password_env)
}

/// Compose both tiers: agent → Beszel, agentless → Kuma. Either section may be
/// absent (then that tier is skipped). This is what `fleet enroll` runs.
pub async fn run_all(
    cfg: &Config,
    db_path: &std::path::Path,
    beszel_base_url: &str,
    dry_run: bool,
) -> anyhow::Result<()> {
    if cfg.beszel.is_some() {
        run(cfg, db_path, beszel_base_url, dry_run).await?;
    } else {
        eprintln!("fleet enroll: [beszel] absent — skipping agent tier");
    }
    if cfg.kuma.is_some() {
        run_kuma(cfg, db_path, dry_run).await?;
    } else {
        eprintln!("fleet enroll: [kuma] absent — skipping agentless tier");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beszel::plan_reconcile;
    use crate::beszel::{BeszelSystem, DesiredAgent, EnrollmentRow};

    // ── Enroll command integration: dry-run prints plan without side effects ──

    #[tokio::test]
    async fn enroll_dry_run_no_api_calls() {
        // Arrange a wiremock server that asserts no calls are made.
        let server = wiremock::MockServer::start().await;
        // Mount auth — expect 1 call (auth is always needed to build the plan).
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/api/collections/users/auth-with-password",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "token": "raw-token-abc",
                    "record": {}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        // Mount list — no existing systems.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/collections/systems/records"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "page": 1, "perPage": 100, "totalItems": 0, "totalPages": 0, "items": []
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        // PATCH/DELETE/enable must NOT be called in dry-run.
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(0)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/beszel/set-user-token"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        // Build a minimal config with the beszel URL pointing at wiremock.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("fleet.db");
        let cfg = make_test_config(server.uri());

        let result = run_with_password(&cfg, &db_path, &server.uri(), true, "test-password").await;
        assert!(result.is_ok(), "dry-run should succeed: {:?}", result);
        // wiremock verify expectations above.
    }

    fn make_test_config(beszel_url: String) -> crate::config::Config {
        crate::config::Config {
            db_path: "/tmp/fleet-test.db".to_owned(),
            export_yaml_path: "/tmp/fleet-test.yaml".to_owned(),
            online_threshold_secs: 900,
            ssh_user: "root".to_owned(),
            include_unauthorized: false,
            include_external: false,
            tailnets: vec![],
            beszel: Some(crate::config::BeszelConfig {
                url: beszel_url,
                user: "caguabot@example.com".to_owned(),
                password_env: "FLEET_BESZEL_PASSWORD_TEST".to_owned(),
                agent_port: 45876,
            }),
            kuma: None,
            cloudflare: None,
            ntfy: None,
            healthchecks: None,
            probe: None,
            serve: None,
        }
    }

    // ── Pure plan tests (no network) ──────────────────────────────────────────

    #[test]
    fn empty_desired_empty_existing_no_ops() {
        let plan = plan_reconcile(&[], &[], &[]);
        assert!(plan.to_patch.is_empty());
        assert!(plan.missing_fleet_ids.is_empty());
        assert!(plan.to_delete.is_empty());
        assert!(!plan.guard_tripped);
    }

    // ── Kuma: agentless node → MonitorSpec mapping ────────────────────────────

    fn agentless_node(fleet_id: &str, addr: &str, notes: Option<&str>) -> crate::model::Node {
        let now = chrono::Utc::now();
        crate::model::Node {
            fleet_id: fleet_id.to_owned(),
            hostname: fleet_id.to_owned(),
            fqdn: format!("{fleet_id}.local"),
            seen_in: vec![],
            addresses: if addr.is_empty() {
                vec![]
            } else {
                vec![addr.to_owned()]
            },
            os: "linux".to_owned(),
            online: true,
            last_seen: now,
            tags: crate::model::Tags::default(),
            tier: Tier::Agentless,
            dedupe_key_kind: crate::model::DedupeKind::Fuzzy,
            notes: notes.map(str::to_owned),
            first_seen: now,
            updated_at: now,
            fuzzy_hint: None,
        }
    }

    #[test]
    fn agentless_ip_becomes_ping_monitor() {
        let n = agentless_node("nas-01", "100.64.0.1", None);
        let spec = super::monitor_spec_for(&n, 3).unwrap();
        assert_eq!(spec.monitor_type, crate::model::MonitorType::Ping);
        assert_eq!(spec.name, "nas-01");
        assert_eq!(spec.hostname.as_deref(), Some("100.64.0.1"));
        assert_eq!(spec.notification_id_list.get("3"), Some(&true));
    }

    #[test]
    fn agentless_url_note_becomes_http_monitor() {
        let n = agentless_node(
            "svc-01",
            "100.64.0.2",
            Some("https://svc.example.com/health"),
        );
        let spec = super::monitor_spec_for(&n, 0).unwrap();
        assert_eq!(spec.monitor_type, crate::model::MonitorType::Http);
        assert_eq!(spec.url.as_deref(), Some("https://svc.example.com/health"));
        assert!(spec.notification_id_list.is_empty(), "ntfy 0 → none");
    }

    #[test]
    fn agentless_without_ip_or_url_is_skipped() {
        let n = agentless_node("ghost", "", None);
        assert!(super::monitor_spec_for(&n, 1).is_none());
    }

    #[test]
    fn second_run_no_drift_is_noop() {
        // First run enrolled nas-01; on the second run no drift.
        let desired = vec![DesiredAgent {
            fleet_id: "nas-01".to_owned(),
            friendly_name: "nas-01".to_owned(),
            tailnet_ip: "100.64.0.1".to_owned(),
        }];
        let existing = vec![BeszelSystem {
            id: "r1".to_owned(),
            name: "nas-01".to_owned(),
            host: "100.64.0.1".to_owned(),
            port: 45876,
            users: vec![],
            status: "up".to_owned(),
        }];
        let enrolled = vec![EnrollmentRow {
            fleet_id: "nas-01".to_owned(),
            remote_id: "r1".to_owned(),
        }];
        let plan = plan_reconcile(&desired, &existing, &enrolled);
        assert!(
            plan.to_patch.is_empty(),
            "no patch on 2nd run with no drift"
        );
        assert!(plan.missing_fleet_ids.is_empty());
        assert!(plan.to_delete.is_empty());
        assert!(!plan.guard_tripped);
    }
}

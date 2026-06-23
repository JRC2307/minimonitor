/// Task-2 integration tests — Config + Secrets + Doctor
///
/// Written first (TDD RED), then the implementation turns them GREEN.
/// These tests must not shell out to the macOS Keychain or read real files.
///
/// Note: Rust edition 2024 requires `unsafe` blocks for `std::env::set_var`
/// and `std::env::remove_var` because they are not thread-safe.
/// Tests that mutate env vars serialize via ENV_LOCK.
/// Global mutex serializing env-mutating tests so parallel test threads
/// don't race on the same FLEET_* variable names.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ─── Config ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod config_tests {
    use crate::ENV_LOCK;
    use fleet::config::load_config;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_toml(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    const MINIMAL_TOML: &str = r#"
db_path = "~/.local/state/fleet/fleet.db"
export_yaml_path = "~/Desktop/1/tools/minimonitor/fleet.yaml"
online_threshold_secs = 900
ssh_user = "caguabot"
include_unauthorized = false
include_external = false

[[tailnets]]
name = "personal"
oauth_client_id = "k123"
oauth_secret_env = "FLEET_TS_PERSONAL_SECRET"
tailnet = "-"

[beszel]
url = "http://100.64.0.1:8090"
user = "test@example.com"
password_env = "FLEET_BESZEL_PASSWORD"
agent_port = 45876

[kuma]
url = "http://100.64.0.1:3001"
user = "testuser"
password_env = "FLEET_KUMA_PASSWORD"
ntfy_notification_id = 1

[cloudflare]
token_env = "FLEET_CF_TOKEN"
ssl_warn_days = 14

[ntfy]
base_url = "http://100.64.0.1:8082"
topic = "fleet"
token_env = "FLEET_NTFY_TOKEN"

[healthchecks]
ping_key_env = "FLEET_HC_PING_KEY"
slug = "mini-heartbeat"

[probe]
cycles = 10
per_hop_timeout_ms = 1500
loss_threshold_pct = 20.0
rtt_threshold_ms = 250.0
retention_days = 30

[serve]
bind = "8099"
beszel_ui_url = "http://100.64.0.1:8090"
kuma_ui_url = "http://100.64.0.1:3001"
"#;

    #[test]
    fn parses_minimal_toml() {
        let _guard = ENV_LOCK.lock().unwrap();
        let f = write_toml(MINIMAL_TOML);
        // Clear any env overrides that other tests may have set.
        // SAFETY: holding ENV_LOCK serializes all env-mutating tests.
        unsafe {
            std::env::remove_var("FLEET_ONLINE_THRESHOLD_SECS");
            std::env::remove_var("FLEET_DB_PATH");
        }
        let cfg = load_config(f.path()).expect("should parse");
        assert_eq!(cfg.ssh_user, "caguabot");
        assert_eq!(cfg.online_threshold_secs, 900);
        assert!(!cfg.include_unauthorized);
        assert_eq!(cfg.tailnets.len(), 1);
        assert_eq!(cfg.tailnets[0].name, "personal");
        assert_eq!(cfg.tailnets[0].oauth_client_id, "k123");
        assert_eq!(cfg.beszel.as_ref().unwrap().agent_port, 45876);
        assert_eq!(cfg.probe.as_ref().unwrap().cycles, 10);
        assert!((cfg.probe.as_ref().unwrap().loss_threshold_pct - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn tilde_expands_db_path() {
        let _guard = ENV_LOCK.lock().unwrap();
        let f = write_toml(MINIMAL_TOML);
        // SAFETY: holding ENV_LOCK.
        unsafe { std::env::remove_var("FLEET_DB_PATH") };
        let cfg = load_config(f.path()).expect("should parse");
        // ~/ must be replaced with the real home dir, not kept verbatim
        assert!(
            !cfg.db_path.starts_with('~'),
            "db_path still has tilde: {}",
            cfg.db_path
        );
        assert!(cfg.db_path.contains("fleet.db"), "db_path: {}", cfg.db_path);
    }

    #[test]
    fn tilde_expands_export_yaml_path() {
        let _guard = ENV_LOCK.lock().unwrap();
        let f = write_toml(MINIMAL_TOML);
        // SAFETY: holding ENV_LOCK.
        unsafe { std::env::remove_var("FLEET_EXPORT_YAML_PATH") };
        let cfg = load_config(f.path()).expect("should parse");
        assert!(
            !cfg.export_yaml_path.starts_with('~'),
            "export_yaml_path still has tilde: {}",
            cfg.export_yaml_path
        );
    }

    #[test]
    fn env_override_online_threshold() {
        let _guard = ENV_LOCK.lock().unwrap();
        let f = write_toml(MINIMAL_TOML);
        // FLEET_ONLINE_THRESHOLD_SECS=600 should override the 900 in TOML
        // SAFETY: holding ENV_LOCK serializes all env-mutating tests.
        unsafe { std::env::set_var("FLEET_ONLINE_THRESHOLD_SECS", "600") };
        let cfg = load_config(f.path()).expect("should parse");
        unsafe { std::env::remove_var("FLEET_ONLINE_THRESHOLD_SECS") };
        assert_eq!(cfg.online_threshold_secs, 600);
    }

    #[test]
    fn env_override_db_path_and_tilde_expands() {
        let _guard = ENV_LOCK.lock().unwrap();
        let f = write_toml(MINIMAL_TOML);
        // SAFETY: holding ENV_LOCK.
        unsafe { std::env::set_var("FLEET_DB_PATH", "~/.local/state/fleet/override.db") };
        let cfg = load_config(f.path()).expect("should parse");
        unsafe { std::env::remove_var("FLEET_DB_PATH") };
        // env-override must also be tilde-expanded
        assert!(
            !cfg.db_path.starts_with('~'),
            "db_path after env override still has tilde: {}",
            cfg.db_path
        );
        assert!(
            cfg.db_path.contains("override.db"),
            "db_path: {}",
            cfg.db_path
        );
    }
}

// ─── CollectConfig + snapshot_stale_secs ─────────────────────────────────────

#[cfg(test)]
mod collect_config_tests {
    use crate::ENV_LOCK;
    use fleet::config::load_config;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_toml(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    /// Minimal TOML with NO [collect] section — all collect fields must be defaults.
    const NO_COLLECT_TOML: &str = r#"
db_path = "~/.local/state/fleet/fleet.db"
export_yaml_path = "~/Desktop/1/tools/minimonitor/fleet.yaml"
"#;

    #[test]
    fn collect_defaults_when_section_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: holding ENV_LOCK serializes all env-mutating tests.
        unsafe {
            std::env::remove_var("FLEET_COLLECT__AGENT_PORT");
            std::env::remove_var("FLEET_COLLECT__CONCURRENCY");
            std::env::remove_var("FLEET_COLLECT__PER_HOST_TIMEOUT_MS");
            std::env::remove_var("FLEET_COLLECT__RETENTION_DAYS");
            std::env::remove_var("FLEET_COLLECT__STALE_AFTER_HOURS");
            std::env::remove_var("FLEET_COLLECT__TOKEN_ENV");
            std::env::remove_var("FLEET_SNAPSHOT_STALE_SECS");
        }
        let f = write_toml(NO_COLLECT_TOML);
        let cfg = load_config(f.path()).expect("should parse");
        assert_eq!(cfg.collect.agent_port, 9909);
        assert_eq!(cfg.collect.concurrency, 8);
        assert_eq!(cfg.collect.per_host_timeout_ms, 10_000);
        assert_eq!(cfg.collect.retention_days, 14);
        assert_eq!(cfg.collect.stale_after_hours, 3);
        assert!(cfg.collect.token_env.is_none());
        assert_eq!(cfg.snapshot_stale_secs, 10_800);
    }

    #[test]
    fn collect_toml_field_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: holding ENV_LOCK.
        unsafe {
            std::env::remove_var("FLEET_COLLECT__PER_HOST_TIMEOUT_MS");
            std::env::remove_var("FLEET_SNAPSHOT_STALE_SECS");
        }
        const TOML: &str = r#"
db_path = "~/.local/state/fleet/fleet.db"
export_yaml_path = "~/Desktop/1/tools/minimonitor/fleet.yaml"
snapshot_stale_secs = 7200

[collect]
per_host_timeout_ms = 5000
token_env = "MY_AGENT_TOKEN"
"#;
        let f = write_toml(TOML);
        let cfg = load_config(f.path()).expect("should parse");
        assert_eq!(cfg.collect.per_host_timeout_ms, 5000);
        assert_eq!(cfg.collect.token_env.as_deref(), Some("MY_AGENT_TOKEN"));
        // other collect fields still default
        assert_eq!(cfg.collect.agent_port, 9909);
        assert_eq!(cfg.collect.concurrency, 8);
        // top-level override
        assert_eq!(cfg.snapshot_stale_secs, 7200);
    }

    #[test]
    fn collect_env_override_concurrency() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: holding ENV_LOCK.
        unsafe {
            std::env::set_var("FLEET_COLLECT__CONCURRENCY", "16");
        }
        let f = write_toml(NO_COLLECT_TOML);
        let cfg = load_config(f.path()).expect("should parse");
        unsafe {
            std::env::remove_var("FLEET_COLLECT__CONCURRENCY");
        }
        assert_eq!(cfg.collect.concurrency, 16);
        // other fields still default
        assert_eq!(cfg.collect.agent_port, 9909);
    }
}

// ─── Secrets ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod secrets_tests {
    use crate::ENV_LOCK;
    use fleet::secrets::{redact, redact_ping, resolve_with_keychain};

    /// A no-op keychain stub — used in tests to avoid shelling to `security`.
    fn keychain_absent(_svc: &str) -> anyhow::Result<String> {
        anyhow::bail!("keychain not available in tests")
    }

    #[test]
    fn env_first_no_keychain_call() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env_var = "FLEET_TEST_SECRET_ENV_FIRST_UNIQUE";
        // SAFETY: holding ENV_LOCK.
        unsafe { std::env::set_var(env_var, "my-token-value") };
        let result = resolve_with_keychain(env_var, "fleet/test-svc", keychain_absent);
        unsafe { std::env::remove_var(env_var) };
        assert_eq!(result.unwrap(), "my-token-value");
    }

    #[test]
    fn empty_env_falls_through_to_keychain() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env_var = "FLEET_TEST_SECRET_EMPTY_ENV_UNIQUE";
        // SAFETY: holding ENV_LOCK.
        unsafe { std::env::set_var(env_var, "") };
        // keychain returns a value
        let result =
            resolve_with_keychain(
                env_var,
                "fleet/test-svc",
                |_| Ok("from-keychain".to_owned()),
            );
        unsafe { std::env::remove_var(env_var) };
        assert_eq!(result.unwrap(), "from-keychain");
    }

    #[test]
    fn unset_env_falls_through_to_keychain() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env_var = "FLEET_TEST_SECRET_UNSET_ENV_UNIQUE";
        // SAFETY: holding ENV_LOCK.
        unsafe { std::env::remove_var(env_var) };
        let result =
            resolve_with_keychain(
                env_var,
                "fleet/test-svc",
                |_| Ok("from-keychain".to_owned()),
            );
        assert_eq!(result.unwrap(), "from-keychain");
    }

    #[test]
    fn both_absent_error_names_both_sources() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env_var = "FLEET_TEST_SECRET_BOTH_ABSENT_UNIQUE";
        let keychain_svc = "fleet/both-absent-svc";
        // SAFETY: holding ENV_LOCK.
        unsafe { std::env::remove_var(env_var) };
        let err = resolve_with_keychain(env_var, keychain_svc, keychain_absent).unwrap_err();
        let msg = format!("{err}");
        // Error message must name BOTH the env var AND the keychain service
        assert!(
            msg.contains(env_var),
            "error doesn't mention env var: {msg}"
        );
        assert!(
            msg.contains(keychain_svc),
            "error doesn't mention keychain service: {msg}"
        );
    }

    #[test]
    fn redact_strips_bearer_token() {
        let err = anyhow::anyhow!("request failed: Authorization: Bearer tk_supersecret123");
        let display = redact(err);
        let s = format!("{display}");
        assert!(!s.contains("tk_supersecret123"), "Bearer token leaked: {s}");
        assert!(
            s.contains("Bearer"),
            "should still mention Bearer keyword: {s}"
        );
    }

    #[test]
    fn redact_strips_hc_ping_key_from_url() {
        let err = anyhow::anyhow!("GET https://hc-ping.com/SECRETPINGKEY/mini-heartbeat failed");
        let display = redact(err);
        let s = format!("{display}");
        assert!(!s.contains("SECRETPINGKEY"), "ping key leaked: {s}");
    }

    #[test]
    fn redact_ping_scrubs_literal_key() {
        let err =
            anyhow::anyhow!("GET https://hc-ping.com/SECRETPINGKEY/mini-heartbeat?create=1 failed");
        // 2nd arg is the ACTUAL secret key value — redact_ping scrubs it literally.
        // The slug "mini-heartbeat" is NOT a secret and need not be absent.
        let display = redact_ping(err, "SECRETPINGKEY");
        let s = format!("{display}");
        assert!(
            !s.contains("SECRETPINGKEY"),
            "ping key leaked in redact_ping: {s}"
        );
        // Slug is not a secret — it may safely appear in the redacted output.
        // (No assertion that "mini-heartbeat" is absent.)
    }

    #[test]
    fn redact_leaves_non_secret_text_intact() {
        let err = anyhow::anyhow!("connection refused to 100.64.0.1:8082");
        let display = redact(err);
        let s = format!("{display}");
        assert!(
            s.contains("connection refused"),
            "non-secret text was mangled: {s}"
        );
        assert!(s.contains("100.64.0.1"), "IP was mangled: {s}");
    }
}

// ─── Doctor invocation checks (Fix 1 / C9) ───────────────────────────────────
//
// These tests confirm that the new doctor checks added in Fix 1 are correctly
// wired by calling the relevant public `fleet::doctor` functions directly and
// asserting the expected outcome for a crafted state.  `run_doctor` itself is
// private to `main.rs` so we exercise the same functions it calls.

#[cfg(test)]
mod doctor_invocation_tests {
    use crate::ENV_LOCK;

    /// `check_agent_live_bind` must return Ok on a machine where :9909 is NOT
    /// listening, or where it is only bound to loopback/CGNAT.  The test machine
    /// may or may not have a real agent running; the function must never panic.
    #[test]
    fn agent_live_bind_does_not_panic_on_any_machine() {
        // This is a best-effort smoke test: the real port scan is OS-dependent.
        // We only assert the call completes without panicking.
        let result = fleet::doctor::check_agent_live_bind();
        // On a machine with no agent, or loopback/CGNAT-bound agent, Ok is expected.
        // On a machine with a wildcard-bound agent it returns Err — that's also fine.
        drop(result); // either Ok or Err is acceptable
    }

    /// Token resolvability: when `token_env` is set AND the env var is present,
    /// `check_secret_resolvability` must return an empty list (no unresolved).
    #[test]
    fn token_resolvability_resolves_when_env_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env_var = "_FLEET_DOCTOR_C9_TOKEN_TEST";
        // SAFETY: holding ENV_LOCK serializes all env-mutating tests.
        unsafe { std::env::set_var(env_var, "not-a-real-token") };
        let unresolved = fleet::doctor::check_secret_resolvability(
            &[(env_var, env_var)],
            fleet::secrets::keychain_absent_fn,
        );
        // SAFETY: paired set/remove.
        unsafe { std::env::remove_var(env_var) };
        assert!(
            unresolved.is_empty(),
            "token env var that IS set must resolve; got unresolved: {unresolved:?}"
        );
    }

    /// Token resolvability: when `token_env` is set but the env var is ABSENT,
    /// `check_secret_resolvability` must return the service name (not the value).
    #[test]
    fn token_resolvability_warns_when_env_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env_var = "_FLEET_DOCTOR_C9_TOKEN_ABSENT_TEST";
        // SAFETY: holding ENV_LOCK.
        unsafe { std::env::remove_var(env_var) };
        let unresolved = fleet::doctor::check_secret_resolvability(
            &[(env_var, env_var)],
            fleet::secrets::keychain_absent_fn,
        );
        assert!(
            unresolved.contains(&env_var.to_owned()),
            "absent token env var must appear in unresolved list; got: {unresolved:?}"
        );
        // Confirm value is never returned — the env var name IS the service name here,
        // which is intentional for this test (name != value is the contract).
    }

    /// `is_agent_bind_safe` — the pure helper — must correctly classify addresses.
    /// This doubles as an invocation-level assertion that the helper is wired.
    #[test]
    fn is_agent_bind_safe_correct_for_doctor_use_cases() {
        // Safe: loopback
        assert!(fleet::doctor::is_agent_bind_safe("127.0.0.1"));
        assert!(fleet::doctor::is_agent_bind_safe("::1"));
        // Safe: CGNAT (Tailscale range)
        assert!(fleet::doctor::is_agent_bind_safe("100.96.0.1"));
        // Unsafe: wildcard
        assert!(!fleet::doctor::is_agent_bind_safe("0.0.0.0"));
        assert!(!fleet::doctor::is_agent_bind_safe("[::]"));
        // Unsafe: public
        assert!(!fleet::doctor::is_agent_bind_safe("203.0.113.5"));
    }
}

// ─── Doctor ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod doctor_tests {
    use crate::ENV_LOCK;
    use fleet::doctor::{check_compose_binds, check_secret_resolvability};
    use fleet::secrets::keychain_absent_fn;

    // ── bind-address checks ──────────────────────────────────────────────────

    const COMPOSE_WILDCARD: &str = r#"
services:
  beszel:
    image: henrygd/beszel:0.9.1
    ports:
      - "0.0.0.0:8090:8090"
"#;

    const COMPOSE_CGNAT: &str = r#"
services:
  beszel:
    image: henrygd/beszel:0.9.1
    ports:
      - "100.71.2.3:8090:8090"
"#;

    const COMPOSE_PUBLIC_IP: &str = r#"
services:
  beszel:
    image: henrygd/beszel:0.9.1
    ports:
      - "203.0.113.10:8090:8090"
"#;

    const COMPOSE_NO_BIND: &str = r#"
services:
  beszel:
    image: henrygd/beszel:0.9.1
    ports:
      - "8090:8090"
"#;

    const COMPOSE_MULTIPLE_PORTS: &str = r#"
services:
  beszel:
    image: henrygd/beszel:0.9.1
    ports:
      - "100.64.0.1:8090:8090"
  kuma:
    image: louislam/uptime-kuma:1.23.16
    ports:
      - "100.64.0.2:3001:3001"
"#;

    const COMPOSE_MIXED_BAD: &str = r#"
services:
  beszel:
    image: henrygd/beszel:0.9.1
    ports:
      - "100.64.0.1:8090:8090"
  kuma:
    image: louislam/uptime-kuma:1.23.16
    ports:
      - "0.0.0.0:3001:3001"
"#;

    #[test]
    fn wildcard_0000_fails() {
        let result = check_compose_binds(COMPOSE_WILDCARD);
        assert!(result.is_err(), "0.0.0.0 should fail bind check");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("0.0.0.0") || msg.contains("wildcard") || msg.contains("CGNAT"),
            "error should mention the bad bind: {msg}"
        );
    }

    #[test]
    fn cgnat_ip_passes() {
        let result = check_compose_binds(COMPOSE_CGNAT);
        assert!(
            result.is_ok(),
            "100.71.2.3 is in CGNAT range and should pass: {:?}",
            result
        );
    }

    #[test]
    fn cgnat_boundary_100_64_passes() {
        let result = check_compose_binds(COMPOSE_MULTIPLE_PORTS);
        assert!(result.is_ok(), "100.64.x.x IPs should pass: {:?}", result);
    }

    #[test]
    fn public_ip_fails() {
        let result = check_compose_binds(COMPOSE_PUBLIC_IP);
        assert!(
            result.is_err(),
            "203.0.113.10 is a public IP and should fail"
        );
    }

    #[test]
    fn port_without_bind_ip_fails() {
        // "8090:8090" with no host IP defaults to 0.0.0.0 behavior — reject it
        let result = check_compose_binds(COMPOSE_NO_BIND);
        assert!(
            result.is_err(),
            "port with no bind IP should fail (implicit 0.0.0.0)"
        );
    }

    #[test]
    fn mixed_one_bad_port_fails() {
        let result = check_compose_binds(COMPOSE_MIXED_BAD);
        assert!(result.is_err(), "one bad port should fail the whole check");
    }

    // ── secret resolvability ─────────────────────────────────────────────────

    #[test]
    fn unresolvable_secret_returned_by_name() {
        let _guard = ENV_LOCK.lock().unwrap();
        // keychain_absent_fn is a public helper that always fails keychain lookup
        let secret_names = &[
            ("FLEET_TEST_UNRESOLVED_A", "fleet/unresolved-a"),
            ("FLEET_TEST_UNRESOLVED_B", "fleet/unresolved-b"),
        ];
        // SAFETY: holding ENV_LOCK.
        unsafe {
            std::env::remove_var("FLEET_TEST_UNRESOLVED_A");
            std::env::remove_var("FLEET_TEST_UNRESOLVED_B");
        }

        let unresolved = check_secret_resolvability(secret_names, keychain_absent_fn);
        // both should be unresolved, returned by keychain service name (not value)
        assert_eq!(unresolved.len(), 2);
        assert!(
            unresolved.contains(&"fleet/unresolved-a".to_string()),
            "expected fleet/unresolved-a in result: {:?}",
            unresolved
        );
        assert!(
            unresolved.contains(&"fleet/unresolved-b".to_string()),
            "expected fleet/unresolved-b in result: {:?}",
            unresolved
        );
    }

    #[test]
    fn resolved_secret_not_in_unresolved_list() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env_var = "FLEET_TEST_RESOLVED_SECRET_UNIQUE";
        // SAFETY: holding ENV_LOCK.
        unsafe { std::env::set_var(env_var, "resolved-value") };
        let secret_names = &[(env_var, "fleet/resolved-svc")];
        let unresolved = check_secret_resolvability(secret_names, keychain_absent_fn);
        unsafe { std::env::remove_var(env_var) };
        assert!(
            unresolved.is_empty(),
            "resolved secret should not appear in unresolved list: {:?}",
            unresolved
        );
    }

    #[test]
    fn mixed_resolved_unresolved_returns_only_unresolved() {
        let _guard = ENV_LOCK.lock().unwrap();
        let resolved_env = "FLEET_TEST_MIXED_RESOLVED_UNIQUE";
        let unresolved_env = "FLEET_TEST_MIXED_UNRESOLVED_UNIQUE";
        // SAFETY: holding ENV_LOCK.
        unsafe {
            std::env::set_var(resolved_env, "has-a-value");
            std::env::remove_var(unresolved_env);
        }

        let secret_names = &[
            (resolved_env, "fleet/mixed-resolved"),
            (unresolved_env, "fleet/mixed-unresolved"),
        ];
        let unresolved = check_secret_resolvability(secret_names, keychain_absent_fn);
        unsafe { std::env::remove_var(resolved_env) };
        assert_eq!(
            unresolved.len(),
            1,
            "only one secret should be unresolved: {:?}",
            unresolved
        );
        assert_eq!(unresolved[0], "fleet/mixed-unresolved");
    }
}

//! `fleet heartbeat` — external dead-man's-switch ping to hc-ping.com.
//!
//! Pings `{base}/{ping_key}/{slug}?create=1` (self-provisioning) every time
//! it is called. Intended to run every minute via LaunchAgent / cron; if the
//! ping stops arriving, hc-ping.com alerts the operator's phone (the one
//! signal path that survives a mini/ISP outage).
//!
//! Security — R-8:
//! - `ping_key` is resolved ENV-PREFERRED: `FLEET_HC_PING_KEY` env var first,
//!   Keychain fallback. The env path works even when Keychain is locked, so
//!   the dead-man's-switch keeps firing across reboots/lock-screen events.
//! - The `ping_key` NEVER appears in error output; every error passes through
//!   `secrets::redact_ping` before reaching the caller.

use crate::config::HealthchecksConfig;
use crate::secrets;

/// Default hc-ping.com base URL (production).
pub const HC_PING_BASE: &str = "https://hc-ping.com";

/// Run the heartbeat subcommand: resolve key, ping, propagate errors (redacted).
pub async fn run(cfg: &HealthchecksConfig) -> anyhow::Result<()> {
    run_with_base(cfg, HC_PING_BASE).await
}

/// Like [`run`] but with an injectable base URL (for tests).
pub async fn run_with_base(cfg: &HealthchecksConfig, base: &str) -> anyhow::Result<()> {
    // ENV-PREFERRED: check env var first, fall back to Keychain.
    // This keeps the dead-man's-switch functional even when Keychain is locked.
    let ping_key = secrets::resolve(&cfg.ping_key_env, "fleet-hc-ping-key")?;
    let slug = &cfg.slug;

    crate::alert::heartbeat(base, &ping_key, slug)
        .await
        .map_err(|e| secrets::redact_ping(e, &ping_key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HealthchecksConfig;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn hc_cfg(ping_key_env: &str, slug: &str) -> HealthchecksConfig {
        HealthchecksConfig {
            ping_key_env: ping_key_env.to_owned(),
            slug: slug.to_owned(),
        }
    }

    /// Test 1: URL is built as `{base}/{ping_key}/{slug}?create=1`
    #[tokio::test]
    async fn heartbeat_url_built_correctly() {
        let server = MockServer::start().await;

        // Expect exactly one GET to /<ping_key>/<slug> with ?create=1
        Mock::given(method("GET"))
            .and(path("/test-ping-key-abc/mini-heartbeat"))
            .and(query_param("create", "1"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        unsafe { std::env::set_var("FLEET_TEST_HC_PING_KEY", "test-ping-key-abc") };

        let cfg = hc_cfg("FLEET_TEST_HC_PING_KEY", "mini-heartbeat");
        run_with_base(&cfg, &server.uri()).await.unwrap();

        server.verify().await;
    }

    /// Test 2: non-2xx response → error returned (error_for_status behavior)
    #[tokio::test]
    async fn heartbeat_non_2xx_returns_error() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/test-key/mini-heartbeat"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        unsafe { std::env::set_var("FLEET_TEST_HC_NON2XX", "test-key") };

        let cfg = hc_cfg("FLEET_TEST_HC_NON2XX", "mini-heartbeat");
        let result = run_with_base(&cfg, &server.uri()).await;

        assert!(result.is_err(), "expected error on 500 response");
        server.verify().await;
    }

    /// Test 3: ping_key NEVER appears in error output — R-8 requirement.
    ///
    /// We force a failure (mock returns 500) and assert that the error's
    /// Display string does NOT contain the ping_key value.
    #[tokio::test]
    async fn heartbeat_ping_key_never_in_error_display() {
        let server = MockServer::start().await;
        let secret_key = "SUPER_SECRET_PING_KEY_SHOULD_NOT_LEAK";

        Mock::given(method("GET"))
            .and(path(format!("/{}/slug-test", secret_key)))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;

        let env_var = "FLEET_TEST_HC_REDACT";
        unsafe { std::env::set_var(env_var, secret_key) };

        let cfg = hc_cfg(env_var, "slug-test");
        let result = run_with_base(&cfg, &server.uri()).await;

        assert!(result.is_err(), "expected error on 403 response");
        let err = result.unwrap_err();
        let display = format!("{err}");
        assert!(
            !display.contains(secret_key),
            "ping_key leaked in error output: {display}"
        );

        server.verify().await;
    }

    /// Test 4: ping_key resolves from FLEET_HC_PING_KEY env WITHOUT Keychain.
    ///
    /// We inject via env var only; no Keychain involved. The function must
    /// succeed when the env var is set (proving env-preferred resolution).
    #[tokio::test]
    async fn heartbeat_resolves_ping_key_from_env_without_keychain() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/env-only-key/env-test"))
            .and(query_param("create", "1"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        // Unique env var for this test; never set before, so Keychain won't be reached
        let env_var = "FLEET_TEST_HC_ENV_ONLY_UNIQUE_34892";
        unsafe { std::env::set_var(env_var, "env-only-key") };

        let cfg = hc_cfg(env_var, "env-test");
        // Must succeed — key came from env alone (Keychain not available in tests)
        run_with_base(&cfg, &server.uri()).await.unwrap();

        server.verify().await;
    }
}

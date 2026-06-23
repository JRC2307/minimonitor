//! Alert publisher — ntfy POST for fleet breaches.
//!
//! Fleet's only direct publishing path: POST JSON to the ntfy topic.
//! Beszel and Kuma publish natively; fleet publishes for probe breaches,
//! cf-sync zone-health failures, and SSL-expiry warnings.
//!
//! Priority discipline (spec §3.9):
//! - 5: true outages / dead-man's-switch
//! - 4: probe breaches, cf-sync failures, SSL warn
//! - ≤3: informational

use crate::config::NtfyConfig;
use crate::secrets;
use anyhow::Context;

// ─── Healthchecks heartbeat ───────────────────────────────────────────────────

/// Ping the hc-ping.com dead-man's-switch endpoint.
///
/// URL form: `{base}/{ping_key}/{slug}?create=1`  (self-provisioning slug).
///
/// Returns an error (with the ping_key already redacted by the caller via
/// `secrets::redact_ping`) on any network failure or non-2xx response.
/// The 10 s timeout prevents this from blocking the cron slot.
///
/// **Security (R-8):** this function does NOT redact internally — the caller
/// (`commands::heartbeat::run_with_base`) applies `secrets::redact_ping` so
/// the key never reaches logs or stderr.
/// Returns `true` if the string contains chars that would malform a URL path
/// segment (`/`, `?`, `#`, or ASCII whitespace).
fn has_url_path_unsafe(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '/' | '?' | '#') || c.is_ascii_whitespace())
}

pub async fn heartbeat(base: &str, ping_key: &str, slug: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !has_url_path_unsafe(ping_key),
        "heartbeat: ping_key contains URL-unsafe characters (/, ?, #, or whitespace)"
    );
    anyhow::ensure!(
        !has_url_path_unsafe(slug),
        "heartbeat: slug contains URL-unsafe characters (/, ?, #, or whitespace)"
    );

    let url = format!(
        "{}/{}/{}?create=1",
        base.trim_end_matches('/'),
        ping_key,
        slug
    );

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("heartbeat: building HTTP client")?;

    http.get(&url)
        .send()
        .await
        .context("heartbeat: GET failed")?
        .error_for_status()
        .context("heartbeat: non-2xx response")?;

    Ok(())
}

/// Publish a message to the configured ntfy topic.
///
/// `priority` must be 1–5. 4 is used for SSL warn and zone-health alerts.
///
/// The token is resolved fresh each call (env first, then macOS Keychain).
pub async fn ntfy(
    cfg: &NtfyConfig,
    title: &str,
    message: &str,
    priority: u8,
) -> anyhow::Result<()> {
    ntfy_with_base(cfg, title, message, priority, None).await
}

/// Like [`ntfy`] but with an injectable `ntfy_base_url` override (for tests).
///
/// When `ntfy_base_url` is `None`, `cfg.base_url` is used.
pub async fn ntfy_with_base(
    cfg: &NtfyConfig,
    title: &str,
    message: &str,
    priority: u8,
    ntfy_base_url: Option<&str>,
) -> anyhow::Result<()> {
    let token =
        secrets::resolve(&cfg.token_env, "fleet-ntfy-token").context("ntfy: resolving token")?;

    let base = ntfy_base_url.unwrap_or(&cfg.base_url);
    let url = format!("{}/{}", base.trim_end_matches('/'), cfg.topic);

    let body = serde_json::json!({
        "title": title,
        "message": message,
        "priority": priority,
    });

    let http = reqwest::Client::new();
    let resp = http
        .post(&url)
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
        .context("ntfy: POST failed")?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("ntfy: server returned HTTP {status}");
    }
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::config::NtfyConfig;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    pub fn ntfy_cfg(server: &MockServer) -> NtfyConfig {
        NtfyConfig {
            base_url: server.uri(),
            topic: "fleet".to_owned(),
            token_env: "FLEET_TEST_NTFY_TOKEN".to_owned(),
        }
    }

    #[tokio::test]
    async fn ntfy_posts_to_topic_with_priority_4() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/fleet"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        // Inject token via env
        unsafe { std::env::set_var("FLEET_TEST_NTFY_TOKEN", "test-token") };

        let cfg = ntfy_cfg(&server);
        ntfy_with_base(
            &cfg,
            "SSL Warning",
            "example.com cert expires soon",
            4,
            Some(&server.uri()),
        )
        .await
        .unwrap();

        server.verify().await;
    }

    #[tokio::test]
    async fn heartbeat_rejects_ping_key_with_slash() {
        // A ping_key containing '/' would malform the URL path — must be rejected.
        let err = heartbeat("https://hc-ping.com", "bad/key", "mini-heartbeat").await;
        assert!(err.is_err(), "expected error for ping_key containing '/'");
        let msg = format!("{}", err.unwrap_err());
        assert!(
            msg.contains("URL-unsafe"),
            "error message should mention URL-unsafe: {msg}"
        );
    }

    #[tokio::test]
    async fn ntfy_request_body_contains_priority_4() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/fleet"))
            // Check the body contains priority 4
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "priority": 4
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        unsafe { std::env::set_var("FLEET_TEST_NTFY_TOKEN", "test-token") };
        let cfg = ntfy_cfg(&server);
        ntfy_with_base(
            &cfg,
            "Zone Unhealthy",
            "zone foo.com is not active",
            4,
            Some(&server.uri()),
        )
        .await
        .unwrap();

        server.verify().await;
    }
}

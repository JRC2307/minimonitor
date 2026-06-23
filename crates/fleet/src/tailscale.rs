//! Tailscale REST API v2 client.
//!
//! Two endpoints are used:
//!   - `POST /api/v2/oauth/token` — exchange OAuth client credentials for a
//!     short-lived bearer token (`grant_type=client_credentials`, scope
//!     `devices:core:read`).
//!   - `GET /api/v2/tailnet/{tailnet}/devices?fields=default` — list devices.
//!
//! The base URL is injectable so tests can point the client at a `wiremock`
//! server. A single `429 Too Many Requests` is honored: the client sleeps for
//! the `Retry-After` header (seconds) and retries exactly once.
//!
//! Devices deserialize into [`crate::model::TsDevice`] (camelCase). The
//! `account` field is NOT on the wire — the caller stamps it after fetching.

use crate::model::TsDevice;
use anyhow::{Context, bail};
use std::time::Duration;

const OAUTH_PATH: &str = "/api/v2/oauth/token";

/// A thin Tailscale API client bound to a base URL.
#[derive(Clone)]
pub struct TsClient {
    base_url: String,
    http: reqwest::Client,
}

#[derive(serde::Deserialize)]
struct OauthResponse {
    access_token: String,
}

impl TsClient {
    /// Construct a client against `base_url` (no trailing slash required).
    /// In production this is `https://api.tailscale.com`; in tests it is the
    /// wiremock server URI.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            http: reqwest::Client::new(),
        }
    }

    /// Exchange OAuth client credentials for a bearer access token.
    ///
    /// `grant_type=client_credentials`, `scope=devices:core:read`. The client id and
    /// secret are sent as form fields (Tailscale accepts both form-body and
    /// HTTP basic; form-body keeps wiremock matching simple).
    pub async fn oauth_token(
        &self,
        client_id: &str,
        client_secret: &str,
    ) -> anyhow::Result<String> {
        let url = format!("{}{}", self.base_url, OAUTH_PATH);
        let resp = self
            .http
            .post(&url)
            .form(&[
                ("grant_type", "client_credentials"),
                // Tailscale's read scope is `devices:core:read`; `devices:read`
                // is rejected with HTTP 403 "OAuth client cannot grant scopes"
                // (verified against the live token endpoint, 2026-06-22).
                ("scope", "devices:core:read"),
                ("client_id", client_id),
                ("client_secret", client_secret),
            ])
            .send()
            .await
            .context("oauth token request failed")?;

        let status = resp.status();
        if !status.is_success() {
            bail!("oauth token endpoint returned HTTP {status}");
        }
        let body: OauthResponse = resp.json().await.context("decoding oauth token response")?;
        Ok(body.access_token)
    }

    /// List devices for `tailnet` using `token` as the bearer.
    ///
    /// `tailnet` is the tailnet name or `-` for the token's own tailnet.
    /// Honors a single `429` + `Retry-After` backoff/retry. Returned devices
    /// have an empty `account`; the caller stamps it.
    pub async fn devices(&self, tailnet: &str, token: &str) -> anyhow::Result<Vec<TsDevice>> {
        let url = format!(
            "{}/api/v2/tailnet/{}/devices?fields=default",
            self.base_url, tailnet
        );

        let mut attempts = 0u8;
        loop {
            let resp = self
                .http
                .get(&url)
                .bearer_auth(token)
                .send()
                .await
                .context("devices request failed")?;

            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS && attempts == 0 {
                attempts += 1;
                let secs = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .unwrap_or(1);
                tokio::time::sleep(Duration::from_secs(secs)).await;
                continue;
            }

            let status = resp.status();
            if !status.is_success() {
                bail!("devices endpoint returned HTTP {status}");
            }

            #[derive(serde::Deserialize)]
            struct DevicesEnvelope {
                #[serde(default)]
                devices: Vec<TsDevice>,
            }
            let env: DevicesEnvelope = resp.json().await.context("decoding devices response")?;
            return Ok(env.devices);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use wiremock::matchers::{body_string_contains, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn oauth_flow_returns_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/oauth/token"))
            // Pin the scope: Tailscale rejects `devices:read` with 403; the
            // valid read scope is `devices:core:read` (form-encoded `%3A`).
            // Without this matcher a wrong scope would 404 the mock, failing
            // the test — which is how the original `devices:read` bug must be
            // prevented from regressing.
            .and(body_string_contains("scope=devices%3Acore%3Aread"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"access_token": "tk_live_abc", "token_type": "Bearer"}),
            ))
            .mount(&server)
            .await;

        let client = TsClient::new(server.uri());
        let token = client.oauth_token("kabc", "secret").await.unwrap();
        assert_eq!(token, "tk_live_abc");
    }

    #[tokio::test]
    async fn devices_deserialize_camelcase_and_offset_to_utc() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v2/tailnet/-/devices"))
            .and(query_param("fields", "default"))
            .and(header("authorization", "Bearer tk_live_abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "devices": [{
                    "id": "111",
                    "hostname": "worker-01",
                    "name": "worker-01.tail1234.ts.net",
                    "machineKey": "mkey:abcd",
                    "nodeKey": "nodekey:zzzz",
                    "os": "linux",
                    "addresses": ["100.64.0.1", "fd7a::1"],
                    "tags": ["tag:role-worker"],
                    "isExternal": false,
                    "authorized": true,
                    "lastSeen": "2026-06-20T10:00:00-05:00"
                }]
            })))
            .mount(&server)
            .await;

        let client = TsClient::new(server.uri());
        let devices = client.devices("-", "tk_live_abc").await.unwrap();
        assert_eq!(devices.len(), 1);
        let d = &devices[0];
        assert_eq!(d.id, "111");
        assert_eq!(d.hostname, "worker-01");
        assert_eq!(d.machine_key, "mkey:abcd");
        assert_eq!(d.node_key, "nodekey:zzzz");
        assert_eq!(d.os, "linux");
        assert_eq!(d.addresses, vec!["100.64.0.1", "fd7a::1"]);
        assert_eq!(d.tags, vec!["tag:role-worker"]);
        assert!(!d.is_external);
        assert!(d.authorized);
        // -05:00 offset must normalize to the same UTC instant (15:00Z).
        assert_eq!(
            d.last_seen,
            Utc.with_ymd_and_hms(2026, 6, 20, 15, 0, 0).unwrap()
        );
    }

    #[tokio::test]
    async fn retries_once_on_429_then_succeeds() {
        let server = MockServer::start().await;

        // First call: 429 with Retry-After: 1
        Mock::given(method("GET"))
            .and(path("/api/v2/tailnet/-/devices"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second call: 200 with one device.
        Mock::given(method("GET"))
            .and(path("/api/v2/tailnet/-/devices"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "devices": [{"id": "222", "hostname": "h", "os": "linux", "lastSeen": "2026-06-20T00:00:00Z"}]
            })))
            .mount(&server)
            .await;

        let client = TsClient::new(server.uri());
        let devices = client.devices("-", "tk").await.unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, "222");
    }
}

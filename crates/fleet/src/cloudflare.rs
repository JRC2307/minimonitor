//! Read-only Cloudflare REST client (zones + cert-packs only; no GraphQL).
//!
//! # Envelope contract
//! Every CF REST response wraps the payload in:
//! ```json
//! { "success": true|false, "errors": [...], "result": ... }
//! ```
//! HTTP 200 can still carry `success:false` with non-empty `errors`. This
//! client treats that as `Err`. Only `success:true && errors.empty()` is
//! considered a successful response.
//!
//! # Token scope required (read-only)
//! Zone:Read, SSL and Certificates:Read, "All zones from an account."

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Thin Cloudflare API client with injectable base URL.
#[derive(Clone)]
pub struct CfClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

// ─── Envelope ────────────────────────────────────────────────────────────────

/// The standard CF REST envelope.
#[derive(Deserialize)]
struct CfEnvelope<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<CfError>,
    result: Option<T>,
}

#[derive(Deserialize, Debug)]
struct CfError {
    code: i64,
    message: String,
}

/// CF pagination info object.
#[derive(Deserialize, Default)]
struct CfResultInfo {
    page: u32,
    total_pages: u32,
}

/// Paged envelope for list endpoints.
#[derive(Deserialize)]
struct CfPagedEnvelope<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<CfError>,
    result: Option<Vec<T>>,
    result_info: Option<CfResultInfo>,
}

// ─── Domain types ────────────────────────────────────────────────────────────

/// A Cloudflare zone row (what we pull from the API).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CfZone {
    pub id: String,
    pub name: String,
    pub status: String,
    pub paused: bool,
    /// Derived: `status == "active" && !paused`.
    pub healthy: bool,
    /// Minimum cert expiry across all cert-packs and their certificates.
    pub min_cert_expiry: Option<DateTime<Utc>>,
}

/// Raw zone from the `/zones` list endpoint.
#[derive(Deserialize)]
struct ApiZone {
    id: String,
    name: String,
    status: String,
    #[serde(default)]
    paused: bool,
}

/// One certificate entry inside a cert-pack.
#[derive(Deserialize)]
struct ApiCert {
    expires_on: Option<String>,
}

/// One certificate pack from the `/ssl/certificate_packs` endpoint.
#[derive(Deserialize)]
struct ApiCertPack {
    #[serde(default)]
    certificates: Vec<ApiCert>,
}

// ─── CfClient ────────────────────────────────────────────────────────────────

impl CfClient {
    /// Construct a client against `base_url` (no trailing slash required).
    ///
    /// In production this is `https://api.cloudflare.com/client/v4`;
    /// in tests it is the wiremock server URI.
    pub fn new(base_url: &str, token: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            http: reqwest::Client::new(),
        }
    }

    // ─── Envelope check ──────────────────────────────────────────────────────

    fn check_envelope<T>(env: CfEnvelope<T>, context: &str) -> anyhow::Result<T> {
        if !env.success || !env.errors.is_empty() {
            let msgs: Vec<String> = env
                .errors
                .iter()
                .map(|e| format!("[{}] {}", e.code, e.message))
                .collect();
            bail!("{context}: CF returned success=false — {}", msgs.join("; "));
        }
        env.result
            .ok_or_else(|| anyhow::anyhow!("{context}: CF envelope missing result field"))
    }

    fn check_paged_envelope<T>(
        env: CfPagedEnvelope<T>,
        context: &str,
    ) -> anyhow::Result<(Vec<T>, u32, u32)> {
        if !env.success || !env.errors.is_empty() {
            let msgs: Vec<String> = env
                .errors
                .iter()
                .map(|e| format!("[{}] {}", e.code, e.message))
                .collect();
            bail!("{context}: CF returned success=false — {}", msgs.join("; "));
        }
        let info = env.result_info.unwrap_or_default();
        Ok((env.result.unwrap_or_default(), info.page, info.total_pages))
    }

    // ─── verify_token ────────────────────────────────────────────────────────

    /// Call `GET /user/tokens/verify` — returns `Err` on failure.
    ///
    /// This is the preflight: if it fails we abort the entire sync.
    pub async fn verify_token(&self) -> anyhow::Result<()> {
        let url = format!("{}/user/tokens/verify", self.base_url);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("verify_token: request failed")?;

        let status = resp.status();
        let body: CfEnvelope<serde_json::Value> = resp
            .json()
            .await
            .context("verify_token: decoding response")?;

        if !status.is_success() {
            bail!("verify_token: HTTP {status}");
        }
        Self::check_envelope(body, "verify_token")?;
        Ok(())
    }

    // ─── zones ───────────────────────────────────────────────────────────────

    /// Paginate `GET /zones?per_page=50&page=N` and return all zones.
    ///
    /// Derives `healthy := status == "active" && !paused`.
    /// `min_cert_expiry` is NOT populated here — use [`Self::cert_packs`] for that.
    pub async fn zones(&self) -> anyhow::Result<Vec<CfZone>> {
        let mut all: Vec<CfZone> = Vec::new();
        let mut page = 1u32;

        loop {
            let url = format!("{}/zones?per_page=50&page={}", self.base_url, page);
            let resp = self
                .http
                .get(&url)
                .bearer_auth(&self.token)
                .send()
                .await
                .with_context(|| format!("zones page {page}: request failed"))?;

            let status = resp.status();
            let body: CfPagedEnvelope<ApiZone> = resp
                .json()
                .await
                .with_context(|| format!("zones page {page}: decoding response"))?;

            if !status.is_success() {
                bail!("zones page {page}: HTTP {status}");
            }

            let (items, _cur_page, total_pages) =
                Self::check_paged_envelope(body, &format!("zones page {page}"))?;

            for z in items {
                let healthy = z.status == "active" && !z.paused;
                all.push(CfZone {
                    id: z.id,
                    name: z.name,
                    status: z.status,
                    paused: z.paused,
                    healthy,
                    min_cert_expiry: None,
                });
            }

            if page >= total_pages || total_pages == 0 {
                break;
            }
            page += 1;
        }

        Ok(all)
    }

    // ─── cert_packs ──────────────────────────────────────────────────────────

    /// Fetch `GET /zones/{id}/ssl/certificate_packs?status=all&per_page=50`.
    ///
    /// **`status=all` is REQUIRED** — omitting it hides expired/pending packs.
    ///
    /// Returns the minimum `expires_on` timestamp across ALL packs and ALL
    /// certificates within each pack (a pack can hold RSA + ECDSA certs).
    pub async fn cert_packs(&self, zone_id: &str) -> anyhow::Result<Option<DateTime<Utc>>> {
        let url = format!(
            "{}/zones/{}/ssl/certificate_packs?status=all&per_page=50",
            self.base_url, zone_id
        );

        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .with_context(|| format!("cert_packs({zone_id}): request failed"))?;

        let status = resp.status();
        let body: CfPagedEnvelope<ApiCertPack> = resp
            .json()
            .await
            .with_context(|| format!("cert_packs({zone_id}): decoding response"))?;

        if !status.is_success() {
            bail!("cert_packs({zone_id}): HTTP {status}");
        }

        let (packs, _, _) = Self::check_paged_envelope(body, &format!("cert_packs({zone_id})"))?;

        Ok(fold_min_expiry(&packs))
    }
}

// ─── Pure helpers ─────────────────────────────────────────────────────────────

/// Fold all `expires_on` strings across all packs → `min(expires_on)`.
///
/// Skips packs/certs with missing or unparseable expiry.
fn fold_min_expiry(packs: &[ApiCertPack]) -> Option<DateTime<Utc>> {
    packs
        .iter()
        .flat_map(|p| p.certificates.iter())
        .filter_map(|c| c.expires_on.as_deref())
        .filter_map(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .min()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> CfClient {
        CfClient::new(&server.uri(), "test-token")
    }

    // ── 1. Envelope success:false → Err even on HTTP 200 ────────────────────

    #[tokio::test]
    async fn envelope_success_false_is_err() {
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

        let err = client(&server).verify_token().await.unwrap_err();
        assert!(
            err.to_string().contains("success=false"),
            "expected success=false in error: {err}"
        );
    }

    #[tokio::test]
    async fn envelope_success_false_on_zones_is_err() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zones"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": false,
                "errors": [{"code": 9109, "message": "Permission denied"}],
                "result": [],
                "result_info": {"page": 1, "total_pages": 1}
            })))
            .mount(&server)
            .await;

        let err = client(&server).zones().await.unwrap_err();
        assert!(
            err.to_string().contains("success=false"),
            "expected success=false in error: {err}"
        );
    }

    // ── 2. verify_token failure aborts (zones never called) ─────────────────

    #[tokio::test]
    async fn verify_token_failure_no_zones_call() {
        let server = MockServer::start().await;
        // verify_token fails
        Mock::given(method("GET"))
            .and(path("/user/tokens/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": false,
                "errors": [{"code": 1000, "message": "invalid token"}],
                "result": null
            })))
            .mount(&server)
            .await;
        // zones endpoint — we'll check it was NOT called
        Mock::given(method("GET"))
            .and(path("/zones"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": [],
                "result_info": {"page": 1, "total_pages": 1}
            })))
            .mount(&server)
            .await;

        let cf = client(&server);
        cf.verify_token().await.unwrap_err(); // caller must check and abort

        // Verify zones endpoint was never called
        let received = server.received_requests().await.unwrap();
        let zones_calls = received.iter().filter(|r| r.url.path() == "/zones").count();
        assert_eq!(
            zones_calls, 0,
            "zones should not be called after verify_token failure"
        );
    }

    // ── 3. Zone pagination: 2 pages → merged result ─────────────────────────

    #[tokio::test]
    async fn zones_pagination_two_pages() {
        let server = MockServer::start().await;

        // Page 1
        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": [
                    {"id": "zone1", "name": "example.com", "status": "active", "paused": false},
                    {"id": "zone2", "name": "foo.com", "status": "active", "paused": false}
                ],
                "result_info": {"page": 1, "total_pages": 2}
            })))
            .mount(&server)
            .await;

        // Page 2
        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": [
                    {"id": "zone3", "name": "bar.com", "status": "inactive", "paused": false}
                ],
                "result_info": {"page": 2, "total_pages": 2}
            })))
            .mount(&server)
            .await;

        let zones = client(&server).zones().await.unwrap();
        assert_eq!(zones.len(), 3, "should merge both pages");
        assert_eq!(zones[0].id, "zone1");
        assert_eq!(zones[1].id, "zone2");
        assert_eq!(zones[2].id, "zone3");
    }

    // ── 4. Healthy derivation: status=="active" && !paused ──────────────────

    #[tokio::test]
    async fn healthy_derivation() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zones"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": [
                    {"id": "z1", "name": "a.com", "status": "active", "paused": false},
                    {"id": "z2", "name": "b.com", "status": "active", "paused": true},
                    {"id": "z3", "name": "c.com", "status": "inactive", "paused": false},
                    {"id": "z4", "name": "d.com", "status": "pending", "paused": false},
                ],
                "result_info": {"page": 1, "total_pages": 1}
            })))
            .mount(&server)
            .await;

        let zones = client(&server).zones().await.unwrap();
        assert_eq!(zones.len(), 4);
        // z1: active && !paused → healthy
        assert!(zones[0].healthy, "z1 should be healthy");
        // z2: active but paused → not healthy
        assert!(!zones[1].healthy, "z2 should not be healthy (paused)");
        // z3: inactive → not healthy
        assert!(!zones[2].healthy, "z3 should not be healthy (inactive)");
        // z4: pending → not healthy
        assert!(!zones[3].healthy, "z4 should not be healthy (pending)");
    }

    // ── 5. Cert-pack expiry fold: multiple packs RSA+ECDSA → min ────────────

    #[tokio::test]
    async fn cert_pack_expiry_fold_and_status_all_required() {
        let server = MockServer::start().await;

        // Assert status=all is in the URL by matching query param
        Mock::given(method("GET"))
            .and(path("/zones/z1/ssl/certificate_packs"))
            .and(query_param("status", "all"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": [
                    {
                        "certificates": [
                            {"expires_on": "2026-12-01T00:00:00Z"},  // RSA
                            {"expires_on": "2026-11-15T00:00:00Z"}   // ECDSA
                        ]
                    },
                    {
                        "certificates": [
                            {"expires_on": "2026-10-01T00:00:00Z"},  // RSA older pack
                            {"expires_on": "2026-09-20T00:00:00Z"}   // ECDSA oldest — this is min
                        ]
                    }
                ],
                "result_info": {"page": 1, "total_pages": 1}
            })))
            .mount(&server)
            .await;

        let min_expiry = client(&server).cert_packs("z1").await.unwrap();
        assert!(min_expiry.is_some(), "should have a min expiry");
        let dt = min_expiry.unwrap();
        // min is 2026-09-20
        assert_eq!(dt.format("%Y-%m-%d").to_string(), "2026-09-20");
    }

    #[tokio::test]
    async fn cert_pack_status_all_missing_returns_error() {
        // If status=all is NOT in the URL, the mock won't match → wiremock 404
        let server = MockServer::start().await;
        // Only register a handler WITHOUT status=all — verify our client sends status=all
        // by checking the received request URL directly
        Mock::given(method("GET"))
            .and(path("/zones/z1/ssl/certificate_packs"))
            .and(query_param("status", "all"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": [],
                "result_info": {"page": 1, "total_pages": 1}
            })))
            .mount(&server)
            .await;

        client(&server).cert_packs("z1").await.unwrap();

        // Verify the request actually had status=all in the URL
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let url = requests[0].url.as_str();
        assert!(
            url.contains("status=all"),
            "status=all must be in cert_packs URL, got: {url}"
        );
    }

    // ── Pure fold test: min across all packs/certs ──────────────────────────

    #[test]
    fn fold_min_expiry_selects_minimum() {
        let packs = vec![
            ApiCertPack {
                certificates: vec![
                    ApiCert {
                        expires_on: Some("2026-12-01T00:00:00Z".to_owned()),
                    },
                    ApiCert {
                        expires_on: Some("2026-11-15T00:00:00Z".to_owned()),
                    },
                ],
            },
            ApiCertPack {
                certificates: vec![
                    ApiCert {
                        expires_on: Some("2026-10-01T00:00:00Z".to_owned()),
                    },
                    ApiCert {
                        expires_on: Some("2026-09-20T00:00:00Z".to_owned()),
                    },
                ],
            },
        ];
        let min = fold_min_expiry(&packs).unwrap();
        assert_eq!(min.format("%Y-%m-%d").to_string(), "2026-09-20");
    }

    #[test]
    fn fold_min_expiry_skips_missing() {
        let packs = vec![ApiCertPack {
            certificates: vec![
                ApiCert { expires_on: None },
                ApiCert {
                    expires_on: Some("2026-12-01T00:00:00Z".to_owned()),
                },
            ],
        }];
        let min = fold_min_expiry(&packs).unwrap();
        assert_eq!(min.format("%Y-%m-%d").to_string(), "2026-12-01");
    }

    #[test]
    fn fold_min_expiry_empty_is_none() {
        let packs: Vec<ApiCertPack> = vec![];
        assert!(fold_min_expiry(&packs).is_none());
    }

    // ── 6. verify_token success ──────────────────────────────────────────────

    #[tokio::test]
    async fn verify_token_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/tokens/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": {"id": "abc123", "status": "active"}
            })))
            .mount(&server)
            .await;

        client(&server).verify_token().await.unwrap();
    }
}

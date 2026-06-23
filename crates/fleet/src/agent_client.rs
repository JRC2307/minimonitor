use anyhow::{Context, bail};
use minimonitor_core::snapshot::MonitorSnapshot;
use std::time::Duration;

pub struct AgentClient {
    http: reqwest::Client,
}

impl AgentClient {
    pub fn new(per_host_timeout: Duration) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(per_host_timeout)
                .build()
                .unwrap(),
        }
    }

    /// Fetch `GET {base_url}/snapshot`, optionally adding a Bearer token.
    ///
    /// Returns `(raw_body_bytes, parsed_snapshot)`.  The raw bytes are returned
    /// so callers can persist them verbatim without re-serialising.  We use
    /// `resp.bytes().await?.to_vec()` — this avoids naming the `bytes::Bytes`
    /// type and keeps the `bytes` crate out of fleet's direct dependencies.
    pub async fn fetch_snapshot(
        &self,
        base_url: &str,
        token: Option<&str>,
    ) -> anyhow::Result<(Vec<u8>, MonitorSnapshot)> {
        let url = format!("{}/snapshot", base_url.trim_end_matches('/'));
        let mut req = self.http.get(&url);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.context("snapshot request failed")?;
        if !resp.status().is_success() {
            bail!("HTTP {}", resp.status());
        }
        let raw = resp
            .bytes()
            .await
            .context("reading snapshot body")?
            .to_vec();
        let snap =
            serde_json::from_slice::<MonitorSnapshot>(&raw).context("decoding MonitorSnapshot")?;
        Ok((raw, snap))
    }
}

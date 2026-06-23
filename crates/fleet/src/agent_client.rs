use anyhow::{Context, bail};
use minimonitor_core::snapshot::MonitorSnapshot;

pub struct AgentClient {
    http: reqwest::Client,
}

impl Default for AgentClient {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentClient {
    /// Build an `AgentClient` with **no** reqwest-level timeout.
    ///
    /// Per spec §4.4 ("one bound, one error path"), the single per-host
    /// wall-clock bound is the `tokio::time::timeout` wrapper applied by
    /// `commands/collect.rs`.  Setting a second timeout here would produce two
    /// competing bounds and two distinct error arms (reqwest `TimedOut` vs
    /// `tokio::time::error::Elapsed`), which complicates error reporting and
    /// retry logic.  The tokio wrapper cancels the entire fetch future, so no
    /// connection or body read can outlive the configured `per_host_timeout_ms`.
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder().build().unwrap(),
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

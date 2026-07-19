//! `fleet.toml` loading via figment (TOML + FLEET_* env layer).
//!
//! Tilde expansion is applied post-deserialization on the three path fields:
//! `db_path`, `export_yaml_path`, and any future path-bearing nested field.
//! The figment ENV layer uses `FLEET_` prefix with `__` as the nested separator
//! (e.g. `FLEET_ONLINE_THRESHOLD_SECS`).

use anyhow::Context;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::{Deserialize, Serialize};
use std::path::Path;

// ─── Top-level config ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Path to the SQLite state file. `~` is expanded.
    pub db_path: String,
    /// Path for the git-tracked YAML snapshot. `~` is expanded.
    pub export_yaml_path: String,
    /// Seconds since last_seen before a device is considered offline. Default 900.
    #[serde(default = "default_threshold")]
    pub online_threshold_secs: u64,
    #[serde(default = "default_ssh_user")]
    pub ssh_user: String,
    #[serde(default)]
    pub include_unauthorized: bool,
    #[serde(default)]
    pub include_external: bool,

    /// Seconds after which a served host snapshot is considered stale. Default 10 800 (3 h).
    #[serde(default = "default_snapshot_stale_secs")]
    pub snapshot_stale_secs: u64,

    #[serde(default)]
    pub tailnets: Vec<TailnetConfig>,

    pub beszel: Option<BeszelConfig>,
    pub kuma: Option<KumaConfig>,
    pub cloudflare: Option<CloudflareConfig>,
    pub ntfy: Option<NtfyConfig>,
    pub healthchecks: Option<HealthchecksConfig>,
    pub probe: Option<ProbeConfig>,
    pub serve: Option<ServeConfig>,

    #[serde(default)]
    pub collect: CollectConfig,
}

fn default_snapshot_stale_secs() -> u64 {
    10_800
}

fn default_threshold() -> u64 {
    900
}

fn default_ssh_user() -> String {
    "root".to_owned()
}

// ─── Per-tailnet ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TailnetConfig {
    pub name: String,
    pub oauth_client_id: String,
    pub oauth_secret_env: String,
    /// `-` means the token's own tailnet.
    #[serde(default = "default_tailnet")]
    pub tailnet: String,
}

fn default_tailnet() -> String {
    "-".to_owned()
}

// ─── Beszel ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeszelConfig {
    pub url: String,
    pub user: String,
    pub password_env: String,
    #[serde(default = "default_agent_port")]
    pub agent_port: u16,
}

fn default_agent_port() -> u16 {
    45876
}

// ─── Kuma ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KumaConfig {
    pub url: String,
    pub user: String,
    pub password_env: String,
    #[serde(default)]
    pub ntfy_notification_id: i64,
}

// ─── Cloudflare ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudflareConfig {
    pub token_env: String,
    #[serde(default = "default_ssl_warn_days")]
    pub ssl_warn_days: u32,
}

fn default_ssl_warn_days() -> u32 {
    14
}

// ─── ntfy ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtfyConfig {
    pub base_url: String,
    #[serde(default = "default_ntfy_topic")]
    pub topic: String,
    pub token_env: String,
}

fn default_ntfy_topic() -> String {
    "fleet".to_owned()
}

// ─── Healthchecks ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthchecksConfig {
    pub ping_key_env: String,
    pub slug: String,
}

// ─── Probe ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeConfig {
    #[serde(default = "default_cycles")]
    pub cycles: u32,
    #[serde(default = "default_hop_timeout")]
    pub per_hop_timeout_ms: u64,
    #[serde(default = "default_loss_threshold")]
    pub loss_threshold_pct: f64,
    #[serde(default = "default_rtt_threshold")]
    pub rtt_threshold_ms: f64,
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    #[serde(default)]
    pub target: Vec<ProbeTarget>,
    #[serde(default)]
    pub selector: Vec<ProbeSelector>,
}

fn default_cycles() -> u32 {
    10
}
fn default_hop_timeout() -> u64 {
    1500
}
fn default_loss_threshold() -> f64 {
    20.0
}
fn default_rtt_threshold() -> f64 {
    250.0
}
fn default_retention_days() -> u32 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeTarget {
    pub name: String,
    pub addr: String,
    #[serde(default = "default_path")]
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeSelector {
    pub match_tag: String,
    #[serde(default = "default_path")]
    pub path: String,
}

fn default_path() -> String {
    "underlay".to_owned()
}

// ─── Serve ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServeConfig {
    /// Port number (as string) or full bind address. Bound to tailnet IP by the command.
    pub bind: String,
    #[serde(default)]
    pub beszel_ui_url: String,
    #[serde(default)]
    pub kuma_ui_url: String,
    /// Path to the port→service-name labels TOML. `~` is expanded.
    /// Defaults (when absent) to `~/.config/fleet/service-labels.toml`.
    #[serde(default)]
    pub service_labels_path: Option<String>,
    /// Path to the caguastore catalog TOML. `~` is expanded.
    /// Defaults (when absent) to `~/.config/fleet/store.toml`; a missing file
    /// falls back to the built-in catalog.
    #[serde(default)]
    pub store_path: Option<String>,
    /// Command Center base URL for the `/hub/cc/*` proxy (loopback in prod —
    /// fleet-serve runs on the same host).
    #[serde(default = "default_cc_url")]
    pub cc_url: String,
    /// cuentas base URL for the `/hub/cuentas/*` proxy (GET only).
    #[serde(default = "default_cuentas_url")]
    pub cuentas_url: String,
    /// hermeshub base URL for the `/hub/hermes/*` proxy (GET only).
    #[serde(default = "default_hermeshub_url")]
    pub hermeshub_url: String,
    /// PIN gating the `/hub/cuentas/*` money proxy (header `X-Money-Pin`).
    /// Unset → the money proxy is disabled entirely (money numbers never leave
    /// the server). Server-enforced — the UI lock is only presentation.
    #[serde(default)]
    pub money_pin: Option<String>,
}

fn default_cc_url() -> String {
    "http://127.0.0.1:8787".to_owned()
}
fn default_cuentas_url() -> String {
    "http://127.0.0.1:8789".to_owned()
}
fn default_hermeshub_url() -> String {
    "http://127.0.0.1:8796".to_owned()
}

// ─── Collect ─────────────────────────────────────────────────────────────────

/// Tunables for the `fleet collect` host-snapshot sweep.
/// All fields have defaults so `[collect]` may be omitted from `fleet.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectConfig {
    /// Port the minimonitor-agent listens on on each host. Default 9909.
    #[serde(default = "default_collect_agent_port")]
    pub agent_port: u16,
    /// Maximum concurrent HTTP requests during a sweep. Default 8.
    #[serde(default = "default_collect_concurrency")]
    pub concurrency: usize,
    /// Per-host HTTP request timeout in milliseconds. Default 10 000.
    #[serde(default = "default_collect_timeout_ms")]
    pub per_host_timeout_ms: u64,
    /// Days to keep snapshots in the database before pruning. Default 14.
    #[serde(default = "default_collect_retention_days")]
    pub retention_days: u32,
    /// Hours without a fresh snapshot before a host is flagged stale. Default 3.
    #[serde(default = "default_collect_stale_after_hours")]
    pub stale_after_hours: u64,
    /// Optional env-var name carrying a bearer token for agent authentication.
    #[serde(default)]
    pub token_env: Option<String>,
}

impl Default for CollectConfig {
    fn default() -> Self {
        Self {
            agent_port: default_collect_agent_port(),
            concurrency: default_collect_concurrency(),
            per_host_timeout_ms: default_collect_timeout_ms(),
            retention_days: default_collect_retention_days(),
            stale_after_hours: default_collect_stale_after_hours(),
            token_env: None,
        }
    }
}

fn default_collect_agent_port() -> u16 {
    9909
}
fn default_collect_concurrency() -> usize {
    8
}
fn default_collect_timeout_ms() -> u64 {
    10_000
}
fn default_collect_retention_days() -> u32 {
    14
}
fn default_collect_stale_after_hours() -> u64 {
    3
}

// ─── Loader ──────────────────────────────────────────────────────────────────

/// Load `fleet.toml` from `path`, merge `FLEET_*` env overrides, expand tildes
/// in path fields, and return the typed `Config`.
pub fn load_config(path: &Path) -> anyhow::Result<Config> {
    let mut cfg: Config = Figment::new()
        .merge(Toml::file(path))
        .merge(Env::prefixed("FLEET_").split("__"))
        .extract()
        .with_context(|| format!("loading config from {}", path.display()))?;

    // Tilde-expand every path field post-deserialization.
    cfg.db_path = expand_tilde(&cfg.db_path);
    cfg.export_yaml_path = expand_tilde(&cfg.export_yaml_path);

    if let Some(serve) = cfg.serve.as_mut() {
        if let Some(p) = serve.service_labels_path.as_mut() {
            *p = expand_tilde(p);
        }
        if let Some(p) = serve.store_path.as_mut() {
            *p = expand_tilde(p);
        }
    }

    Ok(cfg)
}

/// Expand a leading `~/` to the current user's home directory.
/// Non-tilde paths are returned unchanged.
pub fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") || path == "~" {
        let home = dirs_home();
        if let Some(h) = home {
            if path == "~" {
                return h;
            }
            return format!("{}/{}", h, &path[2..]);
        }
    }
    path.to_owned()
}

/// Returns the current user's home directory as a String, or `None`.
/// Uses only `std::env` — no libc dependency needed.
fn dirs_home() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("USERPROFILE").ok().filter(|h| !h.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::expand_tilde;

    #[test]
    fn tilde_slash_expands() {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return; // can't test without HOME
        }
        let result = expand_tilde("~/foo/bar");
        assert_eq!(result, format!("{}/foo/bar", home));
    }

    #[test]
    fn bare_tilde_expands() {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return;
        }
        assert_eq!(expand_tilde("~"), home);
    }

    #[test]
    fn non_tilde_unchanged() {
        assert_eq!(expand_tilde("/absolute/path"), "/absolute/path");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
    }
}

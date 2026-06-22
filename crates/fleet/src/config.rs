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

    #[serde(default)]
    pub tailnets: Vec<TailnetConfig>,

    pub beszel: Option<BeszelConfig>,
    pub kuma: Option<KumaConfig>,
    pub cloudflare: Option<CloudflareConfig>,
    pub ntfy: Option<NtfyConfig>,
    pub healthchecks: Option<HealthchecksConfig>,
    pub probe: Option<ProbeConfig>,
    pub serve: Option<ServeConfig>,
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

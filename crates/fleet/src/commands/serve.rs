//! `fleet serve` command — wire config → `serve::run`.

use std::path::Path;

use crate::config::Config;

/// Start the read-only HTTP server.
///
/// Requires `[serve]` in `fleet.toml`; errors if absent.
pub async fn run(cfg: &Config, db_path: &Path) -> anyhow::Result<()> {
    let serve_cfg = cfg.serve.as_ref().ok_or_else(|| {
        anyhow::anyhow!("fleet serve: [serve] section missing from fleet.toml — add `bind = \"0.0.0.0:8099\"` (or the tailnet IP:port)")
    })?;

    crate::serve::run(serve_cfg, db_path).await
}

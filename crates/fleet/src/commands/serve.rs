//! `fleet serve` command — wire config → `serve::run`.

use std::path::Path;

use crate::config::Config;
use crate::service_label::Labels;

/// Start the read-only HTTP server.
///
/// Requires `[serve]` in `fleet.toml`; errors if absent.
pub async fn run(cfg: &Config, db_path: &Path) -> anyhow::Result<()> {
    let serve_cfg = cfg.serve.as_ref().ok_or_else(|| {
        anyhow::anyhow!("fleet serve: [serve] section missing from fleet.toml — add `bind = \"0.0.0.0:8099\"` (or the tailnet IP:port)")
    })?;

    let online_threshold = std::time::Duration::from_secs(cfg.online_threshold_secs);
    let snapshot_stale_threshold = std::time::Duration::from_secs(cfg.snapshot_stale_secs);

    // Resolve the labels path: explicit config field, else the canonical default.
    let labels_path = serve_cfg
        .service_labels_path
        .clone()
        .unwrap_or_else(|| crate::config::expand_tilde("~/.config/fleet/service-labels.toml"));
    // A missing file is fine (empty labels); a malformed file fails startup.
    let labels = Labels::load(Path::new(&labels_path))?;

    // caguastore catalog: explicit path, else the canonical default. A missing
    // file falls back to the built-in catalog; a malformed file fails startup.
    let store_path = serve_cfg
        .store_path
        .clone()
        .unwrap_or_else(|| crate::config::expand_tilde("~/.config/fleet/store.toml"));
    let store = crate::store::Catalog::load(Path::new(&store_path))?;

    crate::serve::run_with(
        serve_cfg,
        db_path,
        online_threshold,
        snapshot_stale_threshold,
        labels,
        store,
    )
    .await
}

//! caguastore — the app catalog behind `GET /` (the launcher home screen).
//!
//! The catalog is a curated list of self-hosted apps. It ships with a built-in
//! default (the real caguaserver fleet) and can be overridden by a TOML file
//! (`~/.config/fleet/store.toml`) shaped as:
//!
//! ```toml
//! [[app]]
//! slug = "poker-helper"
//! name = "poker"
//! tagline = "odds sidekick"
//! url = "http://caguaserver.tail82f3c6.ts.net:3013"
//! port = 3013          # optional — matched against fresh host_port rows for the LED
//! icon = "spade"       # key into the built-in SVG glyph set (see store.html sprite)
//! hue = 350            # tile accent hue, 0–360
//! ```
//!
//! Liveness is read-time only: an app whose `port` appears in a **non-stale**
//! host snapshot port row is "up". Apps without a `port` render without an LED.

use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

/// One tile on the launcher.
#[derive(Debug, Clone, Deserialize)]
pub struct StoreApp {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub tagline: String,
    pub url: String,
    /// Listening port used for the liveness LED (None → no LED).
    #[serde(default)]
    pub port: Option<u16>,
    /// Hostname (substring, case-insensitive) the app runs on. When set, the
    /// LED only matches port rows from that node — prevents an unrelated
    /// process on another fleet host from lighting the tile. None → any node.
    #[serde(default)]
    pub host: Option<String>,
    /// Glyph key into the inline SVG sprite in `store.html`. Unknown keys fall
    /// back to the `app` glyph at render time.
    #[serde(default = "default_icon")]
    pub icon: String,
    /// Tile accent hue (0–360).
    #[serde(default = "default_hue")]
    pub hue: u16,
}

fn default_icon() -> String {
    "app".to_owned()
}
fn default_hue() -> u16 {
    210
}

/// The full catalog.
#[derive(Debug, Clone)]
pub struct Catalog {
    pub apps: Vec<StoreApp>,
}

impl Catalog {
    /// Load from a TOML file. A **missing file yields the built-in default
    /// catalog**; a malformed file is an error (same policy as `Labels::load`).
    pub fn load(path: &Path) -> anyhow::Result<Catalog> {
        if !path.exists() {
            return Ok(Catalog::builtin());
        }

        #[derive(Deserialize)]
        struct StoreFile {
            #[serde(default, rename = "app")]
            apps: Vec<StoreApp>,
        }

        use figment::Figment;
        use figment::providers::{Format, Toml};
        let file: StoreFile = Figment::new()
            .merge(Toml::file(path))
            .extract()
            .with_context(|| format!("parsing store catalog {}", path.display()))?;
        Ok(Catalog { apps: file.apps })
    }

    /// The built-in catalog: every app currently hosted on caguaserver.
    pub fn builtin() -> Catalog {
        const HOST: &str = "http://caguaserver.tail82f3c6.ts.net";
        let app = |slug: &str, name: &str, tagline: &str, port: u16, icon: &str, hue: u16| {
            StoreApp {
                slug: slug.to_owned(),
                name: name.to_owned(),
                tagline: tagline.to_owned(),
                url: format!("{HOST}:{port}"),
                port: Some(port),
                host: Some("caguaserver".to_owned()),
                icon: icon.to_owned(),
                hue,
            }
        };
        Catalog {
            apps: vec![
                app("poker-helper", "poker", "odds sidekick", 3013, "spade", 350),
                app("crag-finder", "crag", "find climbing", 3014, "mountain", 150),
                app("crux-playground", "crux", "playground", 3012, "hold", 25),
                app("iprep", "iprep", "interview prep", 3011, "cap", 210),
                app(
                    "command-center",
                    "backlog",
                    "command center",
                    8787,
                    "kanban",
                    265,
                ),
                app("cuentas", "cuentas", "facturas & money", 8789, "coin", 45),
                app("uptime-kuma", "kuma", "uptime checks", 3001, "pulse", 130),
                app("beszel", "beszel", "host metrics", 8090, "gauge", 190),
                app("ntfy", "ntfy", "push notifs", 8082, "bell", 320),
            ],
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_builtin() {
        let cat = Catalog::load(Path::new("/nonexistent/store.toml")).unwrap();
        assert!(!cat.apps.is_empty(), "builtin catalog must not be empty");
        assert!(cat.apps.iter().any(|a| a.slug == "cuentas"));
    }

    #[test]
    fn builtin_slugs_unique() {
        let cat = Catalog::builtin();
        let mut slugs: Vec<_> = cat.apps.iter().map(|a| a.slug.as_str()).collect();
        slugs.sort_unstable();
        let before = slugs.len();
        slugs.dedup();
        assert_eq!(before, slugs.len(), "duplicate slugs in builtin catalog");
    }

    #[test]
    fn toml_file_overrides_builtin() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("store.toml");
        std::fs::write(
            &p,
            r#"
[[app]]
slug = "only-one"
name = "solo"
url = "http://x:1"
port = 1
"#,
        )
        .unwrap();
        let cat = Catalog::load(&p).unwrap();
        assert_eq!(cat.apps.len(), 1);
        assert_eq!(cat.apps[0].slug, "only-one");
        assert_eq!(cat.apps[0].icon, "app", "icon should default");
        assert_eq!(cat.apps[0].hue, 210, "hue should default");
    }

    #[test]
    fn malformed_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("store.toml");
        std::fs::write(&p, "[[app]]\nslug = 42\n").unwrap();
        assert!(Catalog::load(&p).is_err());
    }
}

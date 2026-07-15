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
//! category = "apps"    # optional section header on the launcher; defaults to "apps"
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
    /// Launcher section this tile renders under. Sections appear in catalog
    /// order (first tile of a category fixes that category's position).
    #[serde(default = "default_category")]
    pub category: String,
}

fn default_icon() -> String {
    "app".to_owned()
}
fn default_hue() -> u16 {
    210
}
fn default_category() -> String {
    "apps".to_owned()
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

    /// The built-in catalog: the caguaserver apps plus the remote-work tools
    /// on the Mac mini, all reachable over the tailnet. Grouped into launcher
    /// sections via `category` — sections render in catalog order.
    pub fn builtin() -> Catalog {
        const SERVER: &str = "http://caguaserver.tail82f3c6.ts.net";
        const MAC: &str = "http://js-mac-mini.tail82f3c6.ts.net";
        let app = |cat: &str,
                   slug: &str,
                   name: &str,
                   tagline: &str,
                   base: &str,
                   host: &str,
                   port: u16,
                   icon: &str,
                   hue: u16| StoreApp {
            slug: slug.to_owned(),
            name: name.to_owned(),
            tagline: tagline.to_owned(),
            url: format!("{base}:{port}"),
            port: Some(port),
            host: Some(host.to_owned()),
            icon: icon.to_owned(),
            hue,
            category: cat.to_owned(),
        };
        let srv = |cat: &str,
                   slug: &str,
                   name: &str,
                   tagline: &str,
                   port: u16,
                   icon: &str,
                   hue: u16| {
            app(cat, slug, name, tagline, SERVER, "caguaserver", port, icon, hue)
        };
        // Mac snapshots report hostname "Js-Mac-mini.local" — "mac" matches.
        let mac = |cat: &str,
                   slug: &str,
                   name: &str,
                   tagline: &str,
                   port: u16,
                   icon: &str,
                   hue: u16| { app(cat, slug, name, tagline, MAC, "mac", port, icon, hue) };
        Catalog {
            apps: vec![
                // ── daily — the things opened every day ──────────────────────
                // brief page binds via tailscale serve — HTTPS like calendario
                StoreApp {
                    slug: "brief".to_owned(),
                    name: "brief".to_owned(),
                    tagline: "panel del día".to_owned(),
                    url: "https://caguaserver.tail82f3c6.ts.net:8092".to_owned(),
                    port: Some(8092),
                    host: Some("caguaserver".to_owned()),
                    icon: "sun".to_owned(),
                    hue: 15,
                    category: "daily".to_owned(),
                },
                // genealogy binds 127.0.0.1 — reachable only via tailscale serve (HTTPS)
                StoreApp {
                    slug: "genealogy".to_owned(),
                    name: "genealogy".to_owned(),
                    tagline: "arbol familiar".to_owned(),
                    url: "https://caguaserver.tail82f3c6.ts.net:3015".to_owned(),
                    port: Some(3015),
                    host: Some("caguaserver".to_owned()),
                    icon: "mesh".to_owned(),
                    hue: 200,
                    category: "apps".to_owned(),
                },
                // calendario binds 127.0.0.1 — reachable only via tailscale serve (HTTPS)
                StoreApp {
                    slug: "calendario".to_owned(),
                    name: "calendario".to_owned(),
                    tagline: "agenda self-hosted".to_owned(),
                    url: "https://caguaserver.tail82f3c6.ts.net:8791".to_owned(),
                    port: Some(8791),
                    host: Some("caguaserver".to_owned()),
                    icon: "calendar".to_owned(),
                    hue: 38,
                    category: "daily".to_owned(),
                },
                srv(
                    "daily",
                    "command-center",
                    "backlog",
                    "command center",
                    8787,
                    "kanban",
                    265,
                ),
                srv("daily", "cuentas", "cuentas", "facturas & money", 8789, "coin", 45),
                srv("daily", "vuelos", "vuelos", "flight tracker", 8792, "plane", 225),
                srv("daily", "depas", "depas", "depas CDMX", 8794, "house", 160),
                // dilo va por tailscale serve HTTPS — requisito del service worker (PWA)
                StoreApp {
                    slug: "dilo".to_owned(),
                    name: "dilo".to_owned(),
                    tagline: "aprende idiomas".to_owned(),
                    url: "https://caguaserver.tail82f3c6.ts.net:8793".to_owned(),
                    port: Some(8793),
                    host: Some("caguaserver".to_owned()),
                    icon: "speech".to_owned(),
                    hue: 220,
                    category: "daily".to_owned(),
                },
                // ── apps — products & experiments ────────────────────────────
                srv("apps", "poker-helper", "poker", "odds sidekick", 3013, "spade", 350),
                srv("apps", "crag-finder", "crag", "find climbing", 3014, "mountain", 150),
                srv("apps", "crux-playground", "crux", "playground", 3012, "hold", 25),
                srv("apps", "iprep", "iprep", "interview prep", 3011, "cap", 210),
                srv("apps", "portfolio", "portfolio", "inversiones", 3010, "chart", 95),
                srv("apps", "polybot", "polybot", "tradingbot panel", 3006, "bot", 285),
                // external — public Cloudflare Workers site, no port/LED
                // (flip url to https://lds.javierr.com once its DNS record exists)
                StoreApp {
                    slug: "manos".to_owned(),
                    name: "manos".to_owned(),
                    tagline: "aprende LSM".to_owned(),
                    url: "https://lds-javierr.jrckc23.workers.dev".to_owned(),
                    port: None,
                    host: None,
                    icon: "hand".to_owned(),
                    hue: 330,
                    category: "apps".to_owned(),
                },
                // ── dev — remote-work tools (Mac mini over the tailnet) ──────
                mac("dev", "ttyd-main", "terminal", "tmux · claude code", 7681, "term", 120),
                mac("dev", "opencode-web", "opencode", "web ui", 4096, "code", 175),
                mac("dev", "ttyd-opencode", "oc·term", "opencode tty", 7682, "term", 85),
                // ── infra — monitoring & plumbing ────────────────────────────
                srv("infra", "uptime-kuma", "kuma", "uptime checks", 3001, "pulse", 130),
                srv("infra", "beszel", "beszel", "host metrics", 8090, "gauge", 190),
                srv("infra", "ntfy", "ntfy", "push notifs", 8082, "bell", 320),
                // external console — no port, no LED
                StoreApp {
                    slug: "tailscale".to_owned(),
                    name: "tailscale".to_owned(),
                    tagline: "tailnet admin".to_owned(),
                    url: "https://login.tailscale.com/admin/machines".to_owned(),
                    port: None,
                    host: None,
                    icon: "mesh".to_owned(),
                    hue: 200,
                    category: "infra".to_owned(),
                },
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
        assert_eq!(cat.apps[0].category, "apps", "category should default");
    }

    #[test]
    fn malformed_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("store.toml");
        std::fs::write(&p, "[[app]]\nslug = 42\n").unwrap();
        assert!(Catalog::load(&p).is_err());
    }
}

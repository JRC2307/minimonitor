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
    /// Money/sensitive app: the tile renders locked (blurred, no navigation)
    /// until the session is unlocked with the money PIN. Default false.
    #[serde(default)]
    pub private: bool,
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
    /// sections via `category` — sections render in catalog order:
    /// daily · money · life · work · dev · infra.
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
                    private: false,
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
        // Loopback/tailnet-IP binds fronted by `tailscale serve` — HTTPS URL,
        // LED still matched against the raw port on caguaserver.
        let tls = |cat: &str,
                   slug: &str,
                   name: &str,
                   tagline: &str,
                   port: u16,
                   icon: &str,
                   hue: u16| StoreApp {
            slug: slug.to_owned(),
            name: name.to_owned(),
            tagline: tagline.to_owned(),
            url: format!("https://caguaserver.tail82f3c6.ts.net:{port}"),
            port: Some(port),
            host: Some("caguaserver".to_owned()),
            icon: icon.to_owned(),
            hue,
            category: cat.to_owned(),
            private: false,
        };
        // External/public URL — no port, no LED.
        let ext = |cat: &str,
                   slug: &str,
                   name: &str,
                   tagline: &str,
                   url: &str,
                   icon: &str,
                   hue: u16| StoreApp {
            slug: slug.to_owned(),
            name: name.to_owned(),
            tagline: tagline.to_owned(),
            url: url.to_owned(),
            port: None,
            host: None,
            icon: icon.to_owned(),
            hue,
            category: cat.to_owned(),
            private: false,
        };
        let mut apps = vec![
                // ── daily — opened every day ─────────────────────────────────
                tls("daily", "brief", "brief", "panel del día", 8092, "sun", 15),
                tls("daily", "calendario", "calendario", "agenda self-hosted", 8791, "calendar", 38),
                tls("daily", "hermeshub", "hermes", "chat + command center", 8796, "speech", 275),
                srv("daily", "command-center", "backlog", "command center", 8787, "kanban", 265),
                tls("daily", "vitals", "vitals", "whoop health tracker", 3016, "pulse", 350),
                // Install links (doctrine 2026-08-09: every installable app's
                // download link lives in the store). OTA pages are tailscale
                // path-serves, not host ports — ext = no LED, correctly.
                ext("daily", "pulso-app", "pulso·app", "instalar en iPhone",
                    "https://caguaserver.tail82f3c6.ts.net:8803", "pulse", 350),
                // ── money — the private drawer (PIN-locked below) ────────────
                srv("money", "cuentas", "cuentas", "facturas & money", 8789, "coin", 45),
                tls("money", "gastos", "gastos", "expense tracker", 8795, "coin", 5),
                srv("money", "portfolio", "portfolio", "inversiones", 3010, "chart", 95),
                srv("money", "polybot", "polybot", "tradingbot panel", 3006, "bot", 285),
                // ── life — travel, home, family, hobbies ─────────────────────
                srv("life", "vuelos", "vuelos", "flight tracker", 8792, "plane", 225),
                srv("life", "depas", "depas", "depas CDMX", 8794, "house", 160),
                tls("life", "dilo", "dilo", "aprende idiomas", 8793, "speech", 220),
                tls("life", "musica", "musica", "streaming · navidrome", 4533, "music", 300),
                tls("life", "feishin", "musica·pro", "vista tipo iTunes · playlists", 4534, "music", 320),
                tls("life", "mapas", "mapas", "mis lugares · sin google", 8799, "map", 130),
                ext("life", "mapas-app", "mapas·app", "instalar en iPhone",
                    "https://caguaserver.tail82f3c6.ts.net:8802", "map", 130),
                tls("life", "fotos", "fotos", "archivo curado · originales", 8800, "camera", 268),
                ext("life", "fotos-app", "fotos·app", "instalar en iPhone",
                    "https://caguaserver.tail82f3c6.ts.net:8804", "camera", 268),
                tls("life", "genealogy", "genealogy", "arbol familiar", 3015, "mesh", 200),
                srv("life", "crag-finder", "crag", "find climbing", 3014, "mountain", 150),
                ext("life", "paros", "paros", "eventos de escalada",
                    "https://paros-web.jrckc23.workers.dev", "mountain", 100),
                ext("life", "paros-app", "paros·app", "instalar en iPhone",
                    "https://caguaserver.tail82f3c6.ts.net:8805", "mountain", 100),
                ext("life", "locals", "locals", "recomendaciones locales",
                    "https://locals.jrckc23.workers.dev", "speech", 20),
                // ── work — shipped products & client sites ───────────────────
                ext("work", "javierr", "javierr.com", "portfolio + javibot",
                    "https://javierr.com", "sun", 260),
                ext("work", "roners", "roners", "planes de rodaje · bodega · radios",
                    "https://roner.mx", "kanban", 200),
                ext("work", "pablorubin", "pablorubin", "portfolio de pintor (cliente)",
                    "https://pablorubin.com", "camera", 45),
                ext("work", "microcentro", "microcentro", "POS · inventario (cliente)",
                    "https://microcentro-web.jrckc23.workers.dev", "kanban", 145),
                ext("work", "puertacaja", "PuertaCaja", "POS + puerta QR para eventos pop-up (demo)",
                    "https://puertacaja-popup.jrckc23.workers.dev", "door", 28),
                ext("work", "stay", "stay", "rental site (demo)",
                    "https://stay.javierr.com", "house", 185),
                ext("work", "abogados-demo", "abogados·demo", "IA para despachos · PIN 12345",
                    "https://demo.javierr.com", "bot", 230),
                tls("work", "estudio", "estudio", "brief de producción de video", 8798, "camera", 42),
                srv("work", "poker-helper", "poker", "odds sidekick", 3013, "spade", 350),
                srv("work", "crux-playground", "crux", "playground", 3012, "hold", 25),
                ext("work", "crux-app", "crux·app", "instalar en iPhone",
                    "https://caguaserver.tail82f3c6.ts.net:8801", "hold", 25),
                srv("work", "iprep", "iprep", "interview prep", 3011, "cap", 210),
                ext("work", "manos", "manos", "aprende LSM",
                    "https://lds.javierr.com", "hand", 330),
                ext("work", "rawcam", "rawcam", "clean camera + overlays",
                    "https://rawcam.pages.dev", "camera", 12),
                ext("work", "pinpad", "pinpad", "nota compartida con PIN",
                    "https://pad.javierr.com", "app", 305),
                // ── dev — remote-work tools (Mac mini over the tailnet) ──────
                // ttyd is loopback-only behind `tailscale serve`, so its public
                // tailnet endpoint is HTTPS. Plain HTTP on :7681 returns 400.
                StoreApp {
                    slug: "ttyd-main".to_owned(),
                    name: "terminal".to_owned(),
                    tagline: "tmux · claude code".to_owned(),
                    url: "https://js-mac-mini.tail82f3c6.ts.net:7681".to_owned(),
                    port: Some(7681),
                    host: Some("mac".to_owned()),
                    icon: "term".to_owned(),
                    hue: 120,
                    category: "dev".to_owned(),
                    private: false,
                },
                // hermes dashboard (official web UI) on the mini — binds the
                // tailscale IP directly so the app's own auth gate engages
                StoreApp {
                    slug: "hermes-app".to_owned(),
                    name: "hermes·app".to_owned(),
                    tagline: "dashboard oficial".to_owned(),
                    url: "http://100.105.239.50:9119".to_owned(),
                    port: Some(9119),
                    host: Some("mac".to_owned()),
                    icon: "bot".to_owned(),
                    hue: 280,
                    category: "dev".to_owned(),
                    private: false,
                },
                // hermes agent TUI on the mini — cookie-auth proxy binds loopback,
                // fronted by tailscale serve → explicit HTTPS like calendario
                StoreApp {
                    slug: "hermes-tui".to_owned(),
                    name: "hermes·tui".to_owned(),
                    tagline: "agente oficial · web".to_owned(),
                    url: "https://js-mac-mini.tail82f3c6.ts.net:7683".to_owned(),
                    port: Some(7683),
                    host: Some("mac".to_owned()),
                    icon: "bot".to_owned(),
                    hue: 260,
                    category: "dev".to_owned(),
                    private: false,
                },
                mac("dev", "opencode-web", "opencode", "web ui", 4096, "code", 175),
                mac("dev", "ttyd-opencode", "oc·term", "opencode tty", 7682, "term", 85),
                // ── infra — monitoring & plumbing ────────────────────────────
                srv("infra", "uptime-kuma", "kuma", "uptime checks", 3001, "pulse", 130),
                srv("infra", "beszel", "beszel", "host metrics", 8090, "gauge", 190),
                srv("infra", "ntfy", "ntfy", "push notifs", 8082, "bell", 320),
                ext("infra", "tailscale", "tailscale", "tailnet admin",
                    "https://login.tailscale.com/admin/machines", "mesh", 200),
        ];
        // Money-facing apps are locked by default (PIN unlock, session-scoped).
        for a in &mut apps {
            if matches!(a.slug.as_str(), "cuentas" | "gastos" | "portfolio") {
                a.private = true;
            }
        }
        Catalog { apps }
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
    fn puertacaja_is_a_public_portless_app() {
        let cat = Catalog::builtin();
        let app = cat
            .apps
            .iter()
            .find(|a| a.slug == "puertacaja")
            .expect("PuertaCaja must be in the builtin catalog");

        assert_eq!(app.name, "PuertaCaja");
        assert_eq!(
            app.url,
            "https://puertacaja-popup.jrckc23.workers.dev"
        );
        assert_eq!(app.category, "work");
        assert_eq!(app.icon, "door");
        assert_eq!(app.port, None);
        assert_eq!(app.host, None);
        assert!(!app.private);
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

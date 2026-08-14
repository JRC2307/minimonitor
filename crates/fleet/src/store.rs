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
//! url = "https://caguaserver.triceratops-adelie.ts.net:3013"
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
    /// día · cuerpo · vida · medios · aprender · dinero · negocio · clientes ·
    /// contenido · dev · instalar · infra.
    ///
    /// Taxonomy retune 2026-08-13: the old seven-section split (daily ·
    /// instalar · money · life · work · dev · infra) lumped lifestyle, personal
    /// tools and paid work together — `life` was a 14-tile drawer and `work`
    /// held everything from client sites to the poker odds helper. Sections are
    /// now narrower and each answers one question ("¿qué abro a diario?",
    /// "¿escalada/salud?", "¿un cliente?"). Order is by how often the owner
    /// reaches for the section, with the install shelf and the plumbing last.
    ///
    /// **Order matters**: a section's position on the launcher is the position
    /// of its *first* tile here (see `routes::get_store`), so keep each
    /// category's tiles contiguous and in the intended render order.
    pub fn builtin() -> Catalog {
        const SERVER: &str = "http://caguaserver.triceratops-adelie.ts.net";
        const MAC: &str = "http://caguamini.triceratops-adelie.ts.net";
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
        // Plain-HTTP tiles: the process binds a public port itself and there is
        // **no** `tailscale serve` listener in front of it. Use `tls` instead
        // the moment a service moves behind tailscale serve — an `http://` URL
        // against an HTTPS listener returns a bare 400 ("Client sent an HTTP
        // request to an HTTPS server"), which reads as a broken app. Verify with
        // `curl -sk -o /dev/null -w '%{http_code}' https://<host>:<port>`:
        // a connection refused means plain HTTP is correct.
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
            url: format!("https://caguaserver.triceratops-adelie.ts.net:{port}"),
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
                // ── día — the four he opens every single day ─────────────────
                // Deliberately tiny: this section is the launcher's front door,
                // so anything that isn't a genuine daily driver belongs below.
                tls("día", "brief", "brief", "panel del día", 8092, "sun", 15),
                tls("día", "calendario", "calendario", "agenda self-hosted", 8791, "calendar", 38),
                tls("día", "hermeshub", "hermes", "chat + command center", 8796, "speech", 275),
                srv("día", "command-center", "backlog", "command center", 8787, "kanban", 265),
                // ── cuerpo — salud y escalada ────────────────────────────────
                // vitals is health, the other three are climbing (training log,
                // crag search, events). Same drawer because they answer the
                // same question: "¿cómo está el cuerpo y a dónde escalo?".
                tls("cuerpo", "vitals", "vitals", "whoop health tracker", 3016, "pulse", 350),
                tls("cuerpo", "crux-playground", "crux", "playground", 3012, "hold", 25),
                tls("cuerpo", "crag-finder", "crag", "find climbing", 3014, "mountain", 150),
                ext("cuerpo", "paros", "paros", "eventos de escalada",
                    "https://paros-web.jrckc23.workers.dev", "mountain", 100),
                // ── vida — lugares, viajes, casa, comida, familia, ocio ──────
                // El cajón de lo personal. Es la sección más grande a propósito:
                // el dueño pidió explícitamente que mapas y poker vivieran aquí,
                // así que "lo mío, fuera del trabajo" se queda como un solo
                // lugar en vez de partirse en tres subsecciones cosméticas.
                tls("vida", "mapas", "mapas", "mis lugares · sin google", 8799, "map", 130),
                tls("vida", "vuelos", "vuelos", "flight tracker", 8792, "plane", 225),
                tls("vida", "comida", "comida", "log · despensa · menús", 8806, "bowl", 90),
                tls("vida", "depas", "depas", "depas CDMX", 8794, "house", 160),
                tls("vida", "genealogy", "genealogy", "arbol familiar", 3015, "mesh", 200),
                // el poker es ocio, no trabajo — vivía en `work` sólo porque
                // corre en la mini como los demás experimentos
                tls("vida", "poker-helper", "poker", "odds sidekick", 3013, "spade", 350),
                ext("vida", "locals", "locals", "recomendaciones locales",
                    "https://locals.jrckc23.workers.dev", "speech", 20),
                // atlas geológico: `tailscale serve` sirve el directorio, no hay
                // proceso escuchando → `ext`, sin LED. Es PWA y se instala desde
                // la misma URL, por eso también está en la repisa de instalar.
                ext("vida", "crust", "crust", "placas · sismos · rocas",
                    "https://caguaserver.triceratops-adelie.ts.net:8813", "strata", 18),
                // ── medios — la biblioteca personal ──────────────────────────
                tls("medios", "musica", "musica", "streaming · navidrome", 4533, "music", 300),
                tls("medios", "feishin", "musica·pro", "vista tipo iTunes · playlists", 4534, "music", 320),
                tls("medios", "fotos", "fotos", "archivo curado · originales", 8800, "camera", 268),
                // ── aprender — idiomas, señas, entrevistas ───────────────────
                tls("aprender", "dilo", "dilo", "aprende idiomas", 8793, "speech", 220),
                ext("aprender", "manos", "manos", "aprende LSM",
                    "https://lds.javierr.com", "hand", 330),
                tls("aprender", "iprep", "iprep", "interview prep", 3011, "cap", 210),
                // ── dinero — the private drawer (PIN-locked below) ───────────
                tls("dinero", "cuentas", "cuentas", "facturas & money", 8789, "coin", 45),
                tls("dinero", "gastos", "gastos", "expense tracker", 8795, "coin", 5),
                srv("dinero", "portfolio", "portfolio", "inversiones", 3010, "chart", 95),
                srv("dinero", "polybot", "polybot", "tradingbot panel", 3006, "bot", 285),
                // ── negocio — mi propia tienda: sitio, marketing y demos ─────
                // Los demos son material de venta del negocio propio, no
                // entregables de un cliente: por eso viven aquí y no en
                // `clientes`.
                ext("negocio", "javierr", "javierr.com", "portfolio + javibot",
                    "https://javierr.com", "sun", 260),
                tls("negocio", "marketing", "marketing", "calendario de posts", 8811, "megaphone", 340),
                ext("negocio", "puertacaja", "PuertaCaja", "POS + puerta QR para eventos pop-up (demo)",
                    "https://puertacaja-popup.jrckc23.workers.dev", "door", 28),
                ext("negocio", "stay", "stay", "rental site (demo)",
                    "https://stay.javierr.com", "house", 185),
                ext("negocio", "abogados-demo", "abogados·demo", "IA para despachos · PIN 12345",
                    "https://demo.javierr.com", "bot", 230),
                // ── clientes — trabajo pagado, un tile por cliente ───────────
                ext("clientes", "pablorubin", "pablorubin", "portfolio de pintor (cliente)",
                    "https://pablorubin.com", "camera", 45),
                ext("clientes", "oachb", "oachb", "archivo de obra · intake (cliente)",
                    "https://oachb-panel.jrckc23.workers.dev", "camera", 90),
                ext("clientes", "microcentro", "microcentro", "POS · inventario (cliente)",
                    "https://microcentro-web.jrckc23.workers.dev", "kanban", 145),
                ext("clientes", "roners", "roners", "planes de rodaje · bodega · radios",
                    "https://roner.mx", "kanban", 200),
                // ── contenido — producir video, foto y voz ───────────────────
                tls("contenido", "estudio", "estudio", "brief de producción de video", 8798, "camera", 42),
                ext("contenido", "rawcam", "rawcam", "clean camera + overlays",
                    "https://rawcam.pages.dev", "camera", 12),
                // grabadora en la mini (necesita el bridge de hermes + ffmpeg
                // locales); loopback tras `tailscale serve` → HTTPS explícito
                StoreApp {
                    slug: "voz".to_owned(),
                    name: "voz".to_owned(),
                    tagline: "graba · máscaras · notas de voz".to_owned(),
                    url: "https://caguamini.triceratops-adelie.ts.net:8809".to_owned(),
                    port: Some(8809),
                    host: Some("mac".to_owned()),
                    icon: "mic".to_owned(),
                    hue: 12,
                    category: "contenido".to_owned(),
                    private: false,
                },
                // ── dev — remote-work tools (Mac mini over the tailnet) ──────
                // ttyd is loopback-only behind `tailscale serve`, so its public
                // tailnet endpoint is HTTPS. Plain HTTP on :7681 returns 400.
                StoreApp {
                    slug: "ttyd-main".to_owned(),
                    name: "terminal".to_owned(),
                    tagline: "tmux · claude code".to_owned(),
                    url: "https://caguamini.triceratops-adelie.ts.net:7681".to_owned(),
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
                    url: "https://caguamini.triceratops-adelie.ts.net:7683".to_owned(),
                    port: Some(7683),
                    host: Some("mac".to_owned()),
                    icon: "bot".to_owned(),
                    hue: 260,
                    category: "dev".to_owned(),
                    private: false,
                },
                // trackpad: el teléfono mueve el cursor del AIR — corre en el
                // air por naturaleza; el air no corre minimonitor-agent, así
                // que sin port/host (sin LED), como los `ext`
                ext("dev", "tacto", "tacto", "el teléfono como trackpad del air",
                    "https://caguair.triceratops-adelie.ts.net:8810", "hand", 205),
                mac("dev", "opencode-web", "opencode", "web ui", 4096, "code", 175),
                mac("dev", "ttyd-opencode", "oc·term", "opencode tty", 7682, "term", 85),
                // pinpad: portapapeles compartido entre máquinas y teléfono —
                // utilidad de trabajo, no un producto; por eso está aquí y no
                // en `negocio` aunque viva en el dominio propio
                ext("dev", "pinpad", "pinpad", "nota compartida con PIN",
                    "https://pad.javierr.com", "app", 305),
                // ── instalar — the download shelf ────────────────────────────
                // Doctrine 2026-08-09: every installable app's download link
                // lives in the store. These used to be scattered next to each
                // app's web tile, which made "put this on a new phone" a
                // scavenger hunt; they are one section now. OTA pages are
                // `tailscale serve` path-serves rather than host ports, so
                // `ext` is right — no LED to light. Near the bottom on purpose:
                // it's the shelf you visit when setting up a device, not daily.
                ext("instalar", "cagua-app", "cagua·app", "instalar en iPhone",
                    "https://caguaserver.triceratops-adelie.ts.net:8807", "app", 42),
                ext("instalar", "pulso-app", "pulso·app", "instalar en iPhone",
                    "https://caguaserver.triceratops-adelie.ts.net:8803", "pulse", 350),
                ext("instalar", "brujula-app", "brújula·app", "instalar en iPhone",
                    "https://caguaserver.triceratops-adelie.ts.net:8812", "compass", 10),
                ext("instalar", "crux-app", "crux·app", "instalar en iPhone",
                    "https://caguaserver.triceratops-adelie.ts.net:8801", "hold", 25),
                ext("instalar", "mapas-app", "mapas·app", "instalar en iPhone",
                    "https://caguaserver.triceratops-adelie.ts.net:8802", "map", 130),
                ext("instalar", "fotos-app", "fotos·app", "instalar en iPhone",
                    "https://caguaserver.triceratops-adelie.ts.net:8804", "camera", 268),
                ext("instalar", "paros-app", "paros·app", "instalar en iPhone",
                    "https://caguaserver.triceratops-adelie.ts.net:8805", "mountain", 100),
                ext("instalar", "crust-app", "crust·app", "instalar PWA · compartir → añadir",
                    "https://caguaserver.triceratops-adelie.ts.net:8813", "strata", 18),
                // ── infra — monitoring & plumbing ────────────────────────────
                srv("infra", "uptime-kuma", "kuma", "uptime checks", 3001, "pulse", 130),
                tls("infra", "ntfy", "ntfy", "push notifs", 8082, "bell", 320),
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
        assert_eq!(app.category, "negocio");
        assert_eq!(app.icon, "door");
        assert_eq!(app.port, None);
        assert_eq!(app.host, None);
        assert!(!app.private);
    }

    /// The launcher derives a section's position from its **first** tile, so a
    /// category whose tiles are split across the vec would render its stragglers
    /// under the earlier header and silently lose the intended order.
    #[test]
    fn builtin_categories_are_contiguous_and_ordered() {
        const ORDER: &[&str] = &[
            "día", "cuerpo", "vida", "medios", "aprender", "dinero", "negocio", "clientes",
            "contenido", "dev", "instalar", "infra",
        ];
        let cat = Catalog::builtin();
        let mut seen: Vec<&str> = Vec::new();
        for a in &cat.apps {
            if seen.last() != Some(&a.category.as_str()) {
                assert!(
                    !seen.contains(&a.category.as_str()),
                    "category {:?} is not contiguous in the catalog",
                    a.category
                );
                seen.push(a.category.as_str());
            }
        }
        assert_eq!(seen, ORDER, "section render order changed");
    }

    /// The three moves the owner asked for by name.
    #[test]
    fn owner_requested_placements() {
        let cat = Catalog::builtin();
        let cat_of = |slug: &str| {
            cat.apps
                .iter()
                .find(|a| a.slug == slug)
                .unwrap_or_else(|| panic!("{slug} missing from catalog"))
                .category
                .clone()
        };
        assert_eq!(cat_of("poker-helper"), "vida");
        assert_eq!(cat_of("mapas"), "vida");
        assert_eq!(cat_of("calendario"), "día");
    }

    /// Tiles behind `tailscale serve` must use `https://`. An `http://` URL to
    /// an HTTPS listener returns a bare 400 and looks like a dead app — the bug
    /// the owner hit on cuentas (2026-08-13). The two lists below are recorded
    /// probe results (`curl -sk -w '%{http_code}'` against both schemes); a
    /// service that changes sides must be re-probed, not guessed.
    #[test]
    fn tailscale_serve_tiles_are_https() {
        // https → real response, http → 400. Behind `tailscale serve`.
        const HTTPS: &[&str] = &[
            "cuentas",
            "vuelos",
            "depas",
            "crag-finder",
            "poker-helper",
            "crux-playground",
            "iprep",
            "ntfy",
            "gastos",
            "brief",
            "calendario",
            "hermeshub",
            "vitals",
            "mapas",
            "comida",
            "genealogy",
            "musica",
            "feishin",
            "fotos",
            "dilo",
            "marketing",
            "estudio",
        ];
        // https → connection refused: these have no TLS listener at all and
        // MUST stay plain HTTP.
        const PLAIN: &[&str] = &["command-center", "polybot", "portfolio", "uptime-kuma"];

        let cat = Catalog::builtin();
        let url = |slug: &str| {
            cat.apps
                .iter()
                .find(|a| a.slug == slug)
                .unwrap_or_else(|| panic!("{slug} missing from catalog"))
                .url
                .clone()
        };
        for slug in HTTPS {
            assert!(
                url(slug).starts_with("https://"),
                "{slug} is behind tailscale serve — an http:// URL yields a 400"
            );
        }
        for slug in PLAIN {
            assert!(
                url(slug).starts_with("http://"),
                "{slug} has no HTTPS listener — https:// would refuse the connection"
            );
        }
    }

    /// No section may grow back into a junk drawer, and no tile may be left
    /// alone under a header of its own.
    #[test]
    fn builtin_sections_are_reasonably_sized() {
        let cat = Catalog::builtin();
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for a in &cat.apps {
            match counts.iter_mut().find(|(c, _)| *c == a.category) {
                Some((_, n)) => *n += 1,
                None => counts.push((a.category.as_str(), 1)),
            }
        }
        for (c, n) in counts {
            assert!(n >= 2, "section {c:?} has a single lonely tile");
            assert!(n < 15, "section {c:?} has {n} tiles — junk drawer");
        }
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

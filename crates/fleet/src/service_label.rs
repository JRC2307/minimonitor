//! Resolve a listening port to the friendly name of the app behind it.
//!
//! Pure, read-time logic (no DB, no IO except [`Labels::load`]). The resolver
//! tries, in order: a curated per-port override, the project name embedded in
//! the process command path (`projects/<type>/<name>/…`), the argv[0] basename
//! when it is a real binary (not a generic runtime), and finally the raw
//! `process` string — so the result is never worse than the unresolved name.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;

/// Project-type directory tokens that precede a project name in a repo path.
const PROJECT_TYPES: &[&str] = &["startup", "client", "personal", "experiments", "tools"];

/// argv[0] basenames that name a runtime, not an app. Anything starting with
/// `python` (e.g. `python3.13`, `Python`) is also treated as generic.
const GENERIC_RUNTIMES: &[&str] = &[
    "node", "ruby", "sh", "bash", "zsh", "deno", "bun", "perl", "java",
];

/// Curated `port → friendly name` overrides, loaded from a TOML `[ports]` table.
#[derive(Debug, Clone, Default)]
pub struct Labels {
    map: HashMap<u16, String>,
}

impl Labels {
    /// An empty label set (the resolver then relies on auto-derivation only).
    pub fn empty() -> Labels {
        Labels {
            map: HashMap::new(),
        }
    }

    /// Load overrides from a TOML file shaped as:
    /// ```toml
    /// [ports]
    /// 3030 = "uptime-kuma"
    /// ```
    /// A **missing file yields an empty set** (figment's `Toml::file` ignores a
    /// non-existent path); a malformed file is an error.
    pub fn load(path: &Path) -> anyhow::Result<Labels> {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        #[derive(serde::Deserialize)]
        struct LabelsFile {
            #[serde(default)]
            ports: HashMap<String, String>,
        }

        let file: LabelsFile = Figment::new()
            .merge(Toml::file(path))
            .extract()
            .with_context(|| format!("loading service labels from {}", path.display()))?;

        let map = file
            .ports
            .into_iter()
            .map(|(k, v)| {
                let port: u16 = k
                    .parse()
                    .with_context(|| format!("port key {k:?} is not a valid u16"))?;
                Ok((port, v))
            })
            .collect::<anyhow::Result<HashMap<u16, String>>>()?;

        Ok(Labels { map })
    }

    /// The override for `port`, if any.
    pub fn get(&self, port: u16) -> Option<&str> {
        self.map.get(&port).map(String::as_str)
    }
}

/// Resolve `port` to a service name. See module docs for the resolution order.
pub fn resolve_service(port: u16, command: Option<&str>, process: &str, labels: &Labels) -> String {
    if let Some(name) = labels.get(port) {
        return name.to_owned();
    }
    if let Some(cmd) = command {
        if let Some(name) = project_from_command(cmd) {
            return name;
        }
        if let Some(name) = binary_name(cmd) {
            return name;
        }
    }
    process.to_owned()
}

/// Extract the project name from a path segment matching either
/// `projects/<type>/<name>/…` (where `<type>` is a known [`PROJECT_TYPES`] token)
/// or `tools/<name>/…` (the standalone tools directory).
fn project_from_command(cmd: &str) -> Option<String> {
    // First try `projects/<type>/<name>`.
    for (idx, _) in cmd.match_indices("projects/") {
        let after = &cmd[idx + "projects/".len()..];
        let mut segs = after.split('/');
        let typ = segs.next()?;
        if !PROJECT_TYPES.contains(&typ) {
            continue;
        }
        // The name segment is bounded by the next '/'; if the path is the end of
        // an argv token, trim a trailing " --flag …" that got glued on.
        let name = segs.next().unwrap_or("");
        let name = name.split_whitespace().next().unwrap_or("");
        if name.is_empty() {
            continue;
        }
        return Some(name.to_owned());
    }
    // Then try standalone `tools/<name>` (the top-level tools directory).
    for (idx, _) in cmd.match_indices("tools/") {
        let after = &cmd[idx + "tools/".len()..];
        let name = after.split('/').next().unwrap_or("");
        let name = name.split_whitespace().next().unwrap_or("");
        if name.is_empty() {
            continue;
        }
        return Some(name.to_owned());
    }
    None
}

/// The argv[0] basename, unless it is a generic runtime (then `None`).
fn binary_name(cmd: &str) -> Option<String> {
    let argv0 = cmd.split_whitespace().next()?;
    let base = argv0.rsplit('/').next().unwrap_or(argv0);
    if base.is_empty() {
        return None;
    }
    let lower = base.to_ascii_lowercase();
    if lower.starts_with("python") || GENERIC_RUNTIMES.contains(&lower.as_str()) {
        return None;
    }
    Some(base.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels_with(port: u16, name: &str) -> Labels {
        let mut l = Labels::empty();
        l.map.insert(port, name.to_owned());
        l
    }

    #[test]
    fn tier1_override_beats_derivable_command() {
        // Even though the command would derive "cuentas", the override wins.
        let labels = labels_with(8789, "cuentas-prod");
        let cmd = "/Users/x/Desktop/1/projects/experiments/cuentas/.venv/bin/python app";
        assert_eq!(
            resolve_service(8789, Some(cmd), "python3.1", &labels),
            "cuentas-prod"
        );
    }

    #[test]
    fn tier2_path_extraction_each_type() {
        let labels = Labels::empty();
        let cases = [
            (
                "/Users/x/Desktop/1/projects/experiments/cuentas/.venv/bin/python u",
                "cuentas",
            ),
            (
                "/Users/x/Desktop/1/projects/client/consulting/.venv/bin/python -m uvicorn",
                "consulting",
            ),
            (
                "node /Users/x/Desktop/1/projects/personal/javierr/web/node_modules/.bin/astro dev",
                "javierr",
            ),
            ("/Users/x/projects/startup/locals/server.js", "locals"),
            ("/Users/x/Desktop/1/tools/maintenance/run.sh", "maintenance"),
        ];
        for (cmd, want) in cases {
            assert_eq!(
                resolve_service(0, Some(cmd), "proc", &labels),
                want,
                "command: {cmd}"
            );
        }
    }

    #[test]
    fn tier2_ignores_unknown_type_token() {
        // "projects/random/foo" — "random" is not a known type → no tier-2 match;
        // argv0 is a real binary → tier 3 returns it.
        let labels = Labels::empty();
        let cmd = "myserver /var/projects/random/foo/app";
        assert_eq!(resolve_service(0, Some(cmd), "proc", &labels), "myserver");
    }

    #[test]
    fn tier3_binary_name_when_no_project_path() {
        let labels = Labels::empty();
        assert_eq!(
            resolve_service(4096, Some("opencode web --port 4096"), "opencode", &labels),
            "opencode"
        );
        assert_eq!(
            resolve_service(
                7681,
                Some("/opt/homebrew/bin/ttyd -p 7681 tmux"),
                "ttyd",
                &labels
            ),
            "ttyd"
        );
    }

    #[test]
    fn tier3_generic_runtime_falls_through_to_process() {
        // Bare server.py under a framework Python, no projects/ path, generic argv0.
        let labels = Labels::empty();
        let cmd = "/Library/Frameworks/Python.framework/Versions/3.13/Resources/Python server.py";
        // argv0 basename "Python" → generic → tier 3 declines → tier 4 = raw process.
        assert_eq!(
            resolve_service(8800, Some(cmd), "Python", &labels),
            "Python"
        );
    }

    #[test]
    fn tier3_node_runtime_falls_through() {
        let labels = Labels::empty();
        // node with no projects/ path → generic → falls to raw process.
        assert_eq!(
            resolve_service(
                3001,
                Some("/opt/homebrew/bin/node /var/app/portfolio.js"),
                "node",
                &labels
            ),
            "node"
        );
    }

    #[test]
    fn tier4_no_command_uses_process() {
        let labels = Labels::empty();
        assert_eq!(
            resolve_service(5432, None, "com.docke", &labels),
            "com.docke"
        );
    }

    #[test]
    fn tier4_no_command_still_honors_override() {
        let labels = labels_with(5432, "paros-postgres");
        assert_eq!(
            resolve_service(5432, None, "com.docke", &labels),
            "paros-postgres"
        );
    }

    #[test]
    fn empty_command_string_falls_to_process() {
        let labels = Labels::empty();
        assert_eq!(
            resolve_service(1, Some("   "), "rawproc", &labels),
            "rawproc"
        );
    }

    #[test]
    fn load_missing_file_is_empty_not_error() {
        let path = std::path::Path::new("/tmp/fleet-no-such-labels-file-xyz.toml");
        let labels = Labels::load(path).expect("missing file must not error");
        assert_eq!(labels.get(3030), None);
    }

    #[test]
    fn load_reads_ports_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service-labels.toml");
        std::fs::write(
            &path,
            "[ports]\n3030 = \"uptime-kuma\"\n8090 = \"beszel-hub\"\n",
        )
        .unwrap();
        let labels = Labels::load(&path).unwrap();
        assert_eq!(labels.get(3030), Some("uptime-kuma"));
        assert_eq!(labels.get(8090), Some("beszel-hub"));
        assert_eq!(labels.get(1234), None);
    }

    #[test]
    fn load_malformed_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "this is not = = valid toml [[[").unwrap();
        assert!(Labels::load(&path).is_err(), "malformed TOML must error");
    }
}

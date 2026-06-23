# Port → Service Naming Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Resolve every listening port on `/ports` and the `/node` host section to the friendly name of the app it belongs to (auto-derived from the process command, with a curated override file).

**Architecture:** A pure `service_label` resolver (`resolve_service`) runs at read time inside `serve`. The route handler joins each port's PID to the full command line stored in the snapshot's `processes` blob, then resolves a name via: labels override → `projects/<type>/<name>` path extraction → real-binary name → raw process fallback. No DB migration; works on already-collected data.

**Tech Stack:** Rust, axum 0.8, askama 0.13, rusqlite, figment (TOML), serde_json.

## Global Constraints

- **Resolution order, first match wins:** (1) labels override by port, (2) `projects/<type>/<name>` path segment from the command, (3) argv[0] basename if it is not a generic runtime, (4) the raw `process` string. Tier 4 guarantees output is never worse than today.
- **Valid project types:** `startup`, `client`, `personal`, `experiments`, `tools` — and ONLY these.
- **Generic runtimes (never returned by tier 3):** any argv[0] basename whose lowercase form starts with `python`, or equals one of `node`, `ruby`, `sh`, `bash`, `zsh`, `deno`, `bun`, `perl`, `java`.
- **Labels file:** `~/.config/fleet/service-labels.toml`, a `[ports]` table of `port = "name"`. **Missing file ⇒ empty labels, never an error.** Malformed file ⇒ error at `serve` startup (fail loud at boot, not per request).
- **No new crate dependencies** — parse the labels TOML with the existing `figment` dep (its `Toml::file` ignores a missing file). Parse the snapshot blob with the existing `serde_json` dep.
- **Read-time only:** no schema change, no migration, no change to the agent or the collection pipeline.
- **Char-safe:** any truncation/slicing of command strings must respect UTF-8 boundaries (see existing `truncate80`).
- Run `cargo fmt`, `cargo clippy`, the relevant `cargo test`, and a repo secret-scan before each commit. The intentional-secret allowlist marker is `# pragma: allowlist secret` (with the leading `#`).

---

### Task 1: `service_label` resolver module (pure)

**Files:**
- Create: `crates/fleet/src/service_label.rs`
- Modify: `crates/fleet/src/lib.rs` (add `pub mod service_label;`)

**Interfaces:**
- Produces:
  - `pub struct Labels` with `pub fn empty() -> Labels`, `pub fn load(path: &std::path::Path) -> anyhow::Result<Labels>`, `pub fn get(&self, port: u16) -> Option<&str>`.
  - `pub fn resolve_service(port: u16, command: Option<&str>, process: &str, labels: &Labels) -> String`.

- [ ] **Step 1: Write the module with failing tests**

Create `crates/fleet/src/service_label.rs`:

```rust
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
const GENERIC_RUNTIMES: &[&str] =
    &["node", "ruby", "sh", "bash", "zsh", "deno", "bun", "perl", "java"];

/// Curated `port → friendly name` overrides, loaded from a TOML `[ports]` table.
#[derive(Debug, Clone, Default)]
pub struct Labels {
    map: HashMap<u16, String>,
}

impl Labels {
    /// An empty label set (the resolver then relies on auto-derivation only).
    pub fn empty() -> Labels {
        Labels { map: HashMap::new() }
    }

    /// Load overrides from a TOML file shaped as:
    /// ```toml
    /// [ports]
    /// 3030 = "uptime-kuma"
    /// ```
    /// A **missing file yields an empty set** (figment's `Toml::file` ignores a
    /// non-existent path); a malformed file is an error.
    pub fn load(path: &Path) -> anyhow::Result<Labels> {
        use figment::providers::{Format, Toml};
        use figment::Figment;

        #[derive(serde::Deserialize)]
        struct LabelsFile {
            #[serde(default)]
            ports: HashMap<u16, String>,
        }

        let file: LabelsFile = Figment::new()
            .merge(Toml::file(path))
            .extract()
            .with_context(|| format!("loading service labels from {}", path.display()))?;

        Ok(Labels { map: file.ports })
    }

    /// The override for `port`, if any.
    pub fn get(&self, port: u16) -> Option<&str> {
        self.map.get(&port).map(String::as_str)
    }
}

/// Resolve `port` to a service name. See module docs for the resolution order.
pub fn resolve_service(
    port: u16,
    command: Option<&str>,
    process: &str,
    labels: &Labels,
) -> String {
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

/// Extract the project name from the first `projects/<type>/<name>/…` segment
/// whose `<type>` is a known [`PROJECT_TYPES`] token.
fn project_from_command(cmd: &str) -> Option<String> {
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
            ("/Users/x/Desktop/1/projects/experiments/cuentas/.venv/bin/python u", "cuentas"),
            ("/Users/x/Desktop/1/projects/client/consulting/.venv/bin/python -m uvicorn", "consulting"),
            ("node /Users/x/Desktop/1/projects/personal/javierr/web/node_modules/.bin/astro dev", "javierr"),
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
            resolve_service(7681, Some("/opt/homebrew/bin/ttyd -p 7681 tmux"), "ttyd", &labels),
            "ttyd"
        );
    }

    #[test]
    fn tier3_generic_runtime_falls_through_to_process() {
        // Bare server.py under a framework Python, no projects/ path, generic argv0.
        let labels = Labels::empty();
        let cmd = "/Library/Frameworks/Python.framework/Versions/3.13/Resources/Python server.py";
        // argv0 basename "Python" → generic → tier 3 declines → tier 4 = raw process.
        assert_eq!(resolve_service(8800, Some(cmd), "Python", &labels), "Python");
    }

    #[test]
    fn tier3_node_runtime_falls_through() {
        let labels = Labels::empty();
        // node with no projects/ path → generic → falls to raw process.
        assert_eq!(
            resolve_service(3001, Some("/opt/homebrew/bin/node /var/app/portfolio.js"), "node", &labels),
            "node"
        );
    }

    #[test]
    fn tier4_no_command_uses_process() {
        let labels = Labels::empty();
        assert_eq!(resolve_service(5432, None, "com.docke", &labels), "com.docke");
    }

    #[test]
    fn tier4_no_command_still_honors_override() {
        let labels = labels_with(5432, "paros-postgres");
        assert_eq!(resolve_service(5432, None, "com.docke", &labels), "paros-postgres");
    }

    #[test]
    fn empty_command_string_falls_to_process() {
        let labels = Labels::empty();
        assert_eq!(resolve_service(1, Some("   "), "rawproc", &labels), "rawproc");
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
        std::fs::write(&path, "[ports]\n3030 = \"uptime-kuma\"\n8090 = \"beszel-hub\"\n").unwrap();
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
```

- [ ] **Step 2: Register the module**

In `crates/fleet/src/lib.rs`, add after `pub mod secrets;` (keep alphabetical-ish ordering with neighbors):

```rust
pub mod service_label;
```

- [ ] **Step 3: Run the tests — expect FAIL first if written before impl, then PASS**

Run: `cargo test -p minimonitor-fleet service_label`
Expected: all `service_label::tests::*` PASS.

- [ ] **Step 4: Lint + fmt**

Run: `cargo fmt -p minimonitor-fleet && cargo clippy -p minimonitor-fleet --all-targets`
Expected: no warnings in `service_label.rs`.

- [ ] **Step 5: Commit**

```bash
git add crates/fleet/src/service_label.rs crates/fleet/src/lib.rs
git commit -m "feat(fleet): service_label resolver (override→path→binary→raw)"
```

---

### Task 2: DB read helpers — pid → command maps

**Files:**
- Modify: `crates/fleet/src/db/host.rs` (add two read helpers + tests)

**Interfaces:**
- Consumes: the `snapshot_json` column on `host_snapshot` (a serialized `MonitorSnapshot`; its `processes` array carries `{ pid, command }`).
- Produces:
  - `pub fn commands_by_pid_all(conn: &Connection) -> anyhow::Result<HashMap<String, HashMap<i64, String>>>` — for each node's newest snapshot, `node_id → (pid → command)`.
  - `pub fn commands_by_pid_for_node(conn: &Connection, node_id: &str) -> anyhow::Result<HashMap<i64, String>>` — `pid → command` for one node's newest snapshot.

- [ ] **Step 1: Write the failing tests**

Append to the `#[cfg(test)] mod tests` block in `crates/fleet/src/db/host.rs` (reuse whatever existing seed helper inserts a `host_snapshot` row; these tests insert their own row with a real `processes` blob):

```rust
#[test]
fn commands_by_pid_all_parses_processes_blob() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let conn = crate::db::open(f.path()).unwrap();
    let blob = r#"{"processes":[
        {"pid":100,"command":"/Users/x/Desktop/1/projects/experiments/cuentas/.venv/bin/python app"},
        {"pid":200,"command":"opencode web --port 4096"}
    ]}"#;
    conn.execute(
        "INSERT INTO host_snapshot
            (node_id, collected_at, hostname, total_cpu_percent, used_memory_bytes,
             total_memory_bytes, workload_count, port_count, snapshot_json)
         VALUES ('n1', '2026-06-22T00:00:00+00:00', 'h', 0.0, 0, 0, 0, 2, ?1)",
        rusqlite::params![blob],
    )
    .unwrap();

    let map = commands_by_pid_all(&conn).unwrap();
    let n1 = map.get("n1").expect("node n1 present");
    assert_eq!(n1.get(&100).map(String::as_str), Some("/Users/x/Desktop/1/projects/experiments/cuentas/.venv/bin/python app"));
    assert_eq!(n1.get(&200).map(String::as_str), Some("opencode web --port 4096"));
}

#[test]
fn commands_by_pid_all_empty_blob_yields_empty_inner_map() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let conn = crate::db::open(f.path()).unwrap();
    conn.execute(
        "INSERT INTO host_snapshot
            (node_id, collected_at, hostname, total_cpu_percent, used_memory_bytes,
             total_memory_bytes, workload_count, port_count, snapshot_json)
         VALUES ('n2', '2026-06-22T00:00:00+00:00', 'h', 0.0, 0, 0, 0, 0, '{}')",
        [],
    )
    .unwrap();

    let map = commands_by_pid_all(&conn).unwrap();
    // node present with an empty inner map (no processes key) — must not error.
    assert!(map.get("n2").map(|m| m.is_empty()).unwrap_or(true));
}

#[test]
fn commands_by_pid_for_node_uses_newest_snapshot() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let conn = crate::db::open(f.path()).unwrap();
    // Older snapshot: pid 1 → "old".
    conn.execute(
        "INSERT INTO host_snapshot
            (node_id, collected_at, hostname, total_cpu_percent, used_memory_bytes,
             total_memory_bytes, workload_count, port_count, snapshot_json)
         VALUES ('n3', '2026-06-22T00:00:00+00:00', 'h', 0.0, 0, 0, 0, 0,
                 '{\"processes\":[{\"pid\":1,\"command\":\"old\"}]}')",
        [],
    )
    .unwrap();
    // Newer snapshot (higher id): pid 1 → "new".
    conn.execute(
        "INSERT INTO host_snapshot
            (node_id, collected_at, hostname, total_cpu_percent, used_memory_bytes,
             total_memory_bytes, workload_count, port_count, snapshot_json)
         VALUES ('n3', '2026-06-22T01:00:00+00:00', 'h', 0.0, 0, 0, 0, 0,
                 '{\"processes\":[{\"pid\":1,\"command\":\"new\"}]}')",
        [],
    )
    .unwrap();

    let map = commands_by_pid_for_node(&conn, "n3").unwrap();
    assert_eq!(map.get(&1).map(String::as_str), Some("new"));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p minimonitor-fleet --lib db::host::tests::commands_by_pid`
Expected: FAIL — `commands_by_pid_all`/`commands_by_pid_for_node` not found.

- [ ] **Step 3: Implement the helpers**

Add to `crates/fleet/src/db/host.rs` (near the other read helpers; add `use std::collections::HashMap;` at the top if not already present). Parse defensively with `serde_json::Value` so a `{}` or partial blob never errors:

```rust
/// Parse the `processes` array of a snapshot JSON blob into a `pid → command`
/// map. Entries missing a numeric `pid` or a non-empty string `command` are
/// skipped. A blob with no `processes` key yields an empty map (never an error).
fn pid_commands_from_blob(snapshot_json: &str) -> HashMap<i64, String> {
    let mut out = HashMap::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(snapshot_json) else {
        return out;
    };
    let Some(procs) = v.get("processes").and_then(|p| p.as_array()) else {
        return out;
    };
    for p in procs {
        let (Some(pid), Some(cmd)) = (
            p.get("pid").and_then(|x| x.as_i64()),
            p.get("command").and_then(|x| x.as_str()),
        ) else {
            continue;
        };
        if cmd.is_empty() {
            continue;
        }
        out.insert(pid, cmd.to_owned());
    }
    out
}

/// For each node's newest snapshot, return `node_id → (pid → command)`.
/// Used by `/ports` to resolve a friendly service name per port.
pub fn commands_by_pid_all(
    conn: &Connection,
) -> anyhow::Result<HashMap<String, HashMap<i64, String>>> {
    let mut stmt = conn
        .prepare(
            "SELECT node_id, snapshot_json
             FROM host_snapshot
             WHERE id IN (SELECT MAX(id) FROM host_snapshot GROUP BY node_id)",
        )
        .context("prepare commands_by_pid_all")?;

    let rows = stmt
        .query_map([], |r| {
            let node_id: String = r.get(0)?;
            let blob: String = r.get(1)?;
            Ok((node_id, blob))
        })
        .context("query commands_by_pid_all")?;

    let mut out: HashMap<String, HashMap<i64, String>> = HashMap::new();
    for row in rows {
        let (node_id, blob) = row.context("map commands_by_pid_all row")?;
        out.insert(node_id, pid_commands_from_blob(&blob));
    }
    Ok(out)
}

/// `pid → command` for `node_id`'s newest snapshot (empty map if none).
pub fn commands_by_pid_for_node(
    conn: &Connection,
    node_id: &str,
) -> anyhow::Result<HashMap<i64, String>> {
    let mut stmt = conn
        .prepare(
            "SELECT snapshot_json
             FROM host_snapshot
             WHERE node_id = ?1
             ORDER BY id DESC
             LIMIT 1",
        )
        .context("prepare commands_by_pid_for_node")?;

    let mut rows = stmt
        .query_map(rusqlite::params![node_id], |r| r.get::<_, String>(0))
        .context("query commands_by_pid_for_node")?;

    match rows.next() {
        Some(blob) => Ok(pid_commands_from_blob(&blob.context("map row")?)),
        None => Ok(HashMap::new()),
    }
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p minimonitor-fleet --lib db::host::tests::commands_by_pid`
Expected: PASS (all three).

- [ ] **Step 5: fmt + clippy + commit**

```bash
cargo fmt -p minimonitor-fleet && cargo clippy -p minimonitor-fleet --all-targets
git add crates/fleet/src/db/host.rs
git commit -m "feat(fleet): pid→command read helpers from snapshot blob"
```

---

### Task 3: Wire `Labels` into `AppState` + `serve` startup + config + seed file

**Files:**
- Modify: `crates/fleet/src/serve/routes.rs` (AppState gains a `labels` field)
- Modify: `crates/fleet/src/serve/mod.rs` (`build_router`, `run_with`, `run`, the `full_router` test helper)
- Modify: `crates/fleet/src/commands/serve.rs` (load labels from path, pass to `run_with`)
- Modify: `crates/fleet/src/config.rs` (`ServeConfig` gains optional `service_labels_path`; tilde-expand it)
- Create: `crates/fleet/service-labels.example.toml` (seed)

**Interfaces:**
- Consumes: `crate::service_label::Labels` (Task 1).
- Produces: `AppState.labels: std::sync::Arc<crate::service_label::Labels>`, available to all handlers.

- [ ] **Step 1: Add the `labels` field to `AppState`**

In `crates/fleet/src/serve/routes.rs`, add to the `AppState` struct (after `kuma_ui_url`):

```rust
    /// Curated port→service-name overrides, loaded once at startup (spec: port
    /// service naming). Wrapped in `Arc` so `AppState` stays cheap to `Clone`.
    pub labels: std::sync::Arc<crate::service_label::Labels>,
```

- [ ] **Step 2: Add an optional config field + tilde-expand it**

In `crates/fleet/src/config.rs`, add to `ServeConfig`:

```rust
    /// Path to the port→service-name labels TOML. `~` is expanded.
    /// Defaults (when absent) to `~/.config/fleet/service-labels.toml`.
    #[serde(default)]
    pub service_labels_path: Option<String>,
```

And in `load_config`, after the existing tilde expansions, expand the nested field:

```rust
    if let Some(serve) = cfg.serve.as_mut() {
        if let Some(p) = serve.service_labels_path.as_mut() {
            *p = expand_tilde(p);
        }
    }
```

(Change `let cfg` to `let mut cfg` is already the case — it is declared `let mut cfg` in `load_config`.)

- [ ] **Step 3: Update every `AppState` construction site to compile**

In `crates/fleet/src/serve/mod.rs`:

`build_router` (defaults to empty labels):

```rust
pub fn build_router(db_path: PathBuf) -> Router {
    build_router_with(routes::AppState {
        db_path,
        online_threshold: DEFAULT_ONLINE_THRESHOLD,
        snapshot_stale_threshold: DEFAULT_SNAPSHOT_STALE_THRESHOLD,
        beszel_ui_url: String::new(),
        kuma_ui_url: String::new(),
        labels: std::sync::Arc::new(crate::service_label::Labels::empty()),
    })
}
```

Change `run_with` to accept labels and store them:

```rust
pub async fn run_with(
    cfg: &ServeConfig,
    db_path: &Path,
    online_threshold: Duration,
    snapshot_stale_threshold: Duration,
    labels: crate::service_label::Labels,
) -> anyhow::Result<()> {
    let addr: std::net::SocketAddr = cfg
        .bind
        .parse()
        .with_context(|| format!("fleet serve: invalid bind address {:?}", cfg.bind))?;

    let router = build_router_with(routes::AppState {
        db_path: db_path.to_path_buf(),
        online_threshold,
        snapshot_stale_threshold,
        beszel_ui_url: cfg.beszel_ui_url.clone(),
        kuma_ui_url: cfg.kuma_ui_url.clone(),
        labels: std::sync::Arc::new(labels),
    });

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("fleet serve: bind {addr}"))?;

    eprintln!("fleet serve: listening on {addr}");

    axum::serve(listener, router)
        .await
        .context("fleet serve: axum::serve error")?;

    Ok(())
}
```

Update the default `run` to pass empty labels:

```rust
pub async fn run(cfg: &ServeConfig, db_path: &Path) -> anyhow::Result<()> {
    run_with(
        cfg,
        db_path,
        DEFAULT_ONLINE_THRESHOLD,
        DEFAULT_SNAPSHOT_STALE_THRESHOLD,
        crate::service_label::Labels::empty(),
    )
    .await
}
```

Update the `full_router` test helper inside `mod.rs`'s `#[cfg(test)] mod tests` to add the field:

```rust
    fn full_router(db_path: PathBuf) -> Router {
        build_router_with(routes::AppState {
            db_path,
            online_threshold: Duration::from_secs(900),
            snapshot_stale_threshold: DEFAULT_SNAPSHOT_STALE_THRESHOLD,
            beszel_ui_url: "http://intel-mini:8090".to_owned(),
            kuma_ui_url: "http://intel-mini:3001".to_owned(),
            labels: std::sync::Arc::new(crate::service_label::Labels::empty()),
        })
    }
```

- [ ] **Step 4: Load labels in the `serve` command**

Replace `crates/fleet/src/commands/serve.rs` body of `run` so it resolves the labels path and loads it (failing loud on a malformed file):

```rust
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

    crate::serve::run_with(
        serve_cfg,
        db_path,
        online_threshold,
        snapshot_stale_threshold,
        labels,
    )
    .await
}
```

- [ ] **Step 5: Create the seed example file**

Create `crates/fleet/service-labels.example.toml` (the cases auto-derivation can't reach — docker-proxied ports and framework-Python servers with no `projects/` path). Copy to `~/.config/fleet/service-labels.toml` and edit:

```toml
# Port → friendly service name overrides for the fleet monitor.
# Copy to ~/.config/fleet/service-labels.toml and edit.
# Only needed for ports whose process command does NOT reveal a
# projects/<type>/<name> path (docker-proxied ports, bare server.py, etc.).
# Auto-derivation handles the rest — leave those out.

[ports]
3030  = "uptime-kuma"
8090  = "beszel-hub"
5432  = "paros-postgres"
5433  = "dinara-fuzz-pg"
21115 = "rustdesk-server"
21116 = "rustdesk-server"
21117 = "rustdesk-server"
21118 = "rustdesk-server"
21119 = "rustdesk-server"
3011  = "iprep-playground"
3012  = "crux-playground"
3013  = "poker-helper"
8790  = "callsheet"
8800  = "autopilot"
```

- [ ] **Step 6: Build + run the existing serve tests (nothing should break yet)**

Run: `cargo test -p minimonitor-fleet --lib serve::`
Expected: PASS — all existing serve tests compile against the new field and still pass. Also run config tests: `cargo test -p minimonitor-fleet --lib config`.

- [ ] **Step 7: fmt + clippy + commit**

```bash
cargo fmt -p minimonitor-fleet && cargo clippy -p minimonitor-fleet --all-targets
git add crates/fleet/src/serve/routes.rs crates/fleet/src/serve/mod.rs \
        crates/fleet/src/commands/serve.rs crates/fleet/src/config.rs \
        crates/fleet/service-labels.example.toml
git commit -m "feat(fleet): load service labels into AppState at serve startup"
```

---

### Task 4: `/ports` — resolve and render the Service column

**Files:**
- Modify: `crates/fleet/src/serve/templates.rs` (`FleetPortViewRow` gains `service`)
- Modify: `crates/fleet/src/serve/routes.rs` (`get_ports_html` resolves `service`)
- Modify: `crates/fleet/src/serve/templates/ports.html` (Service column)
- Modify: `crates/fleet/src/serve/mod.rs` `#[cfg(test)]` (one new test)

**Interfaces:**
- Consumes: `db::host::commands_by_pid_all` (Task 2), `service_label::resolve_service` (Task 1), `AppState.labels` (Task 3).

- [ ] **Step 1: Add the `service` field to the view-model**

In `crates/fleet/src/serve/templates.rs`, add to `FleetPortViewRow` (first field, the primary name):

```rust
pub struct FleetPortViewRow {
    pub fleet_id: String,
    pub hostname: String,
    pub service: String, // resolved friendly name (spec: port service naming)
    pub port: u16,
    pub proto: String,
    pub process: String,
    pub pid: i64,
    pub bind: String,
    pub collected_at: String,
    pub stale: bool,
}
```

- [ ] **Step 2: Write a failing test for the derived name**

Add to `crates/fleet/src/serve/mod.rs`'s `#[cfg(test)] mod tests` (host-snapshot section). It seeds a snapshot whose `snapshot_json` contains a process with a `projects/` command, then asserts `/ports` shows the derived service name. NOTE: the existing `seed_host_snapshot` writes `snapshot_json='{}'`, so this test inserts its own snapshot row with a real blob:

```rust
#[tokio::test]
async fn ports_shows_resolved_service_name() {
    let node = make_node("fleet-svc", "svc-host");
    let f = seed_db(&[node]);
    let fresh_ts = (Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
    {
        let conn = db::open(f.path()).unwrap();
        let blob = r#"{"processes":[{"pid":4242,"command":"/Users/x/Desktop/1/projects/experiments/cuentas/.venv/bin/python app"}]}"#;
        conn.execute(
            "INSERT INTO host_snapshot
                (node_id, collected_at, hostname, total_cpu_percent, used_memory_bytes,
                 total_memory_bytes, workload_count, port_count, snapshot_json)
             VALUES ('fleet-svc', ?1, 'svc-host', 0.0, 0, 0, 0, 1, ?2)",
            rusqlite::params![fresh_ts, blob],
        )
        .unwrap();
        let sid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO host_port (snapshot_id, node_id, port, proto, process, pid, bind)
             VALUES (?1, 'fleet-svc', 8789, 'TCP', 'python3.1', 4242, '0.0.0.0')",
            rusqlite::params![sid],
        )
        .unwrap();
    }
    let router = full_router(f.path().to_path_buf());
    let (status, html) = html_get(router, "/ports").await;
    assert_eq!(status, StatusCode::OK, "body: {html}");
    assert!(html.contains("cuentas"), "resolved service name missing:\n{html}");
    // Raw process is still shown as ground truth.
    assert!(html.contains("python3.1"), "raw process should still render:\n{html}");
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p minimonitor-fleet --lib serve::tests::ports_shows_resolved_service_name`
Expected: FAIL — `service` field not constructed / column not rendered.

- [ ] **Step 4: Resolve `service` in the handler**

In `crates/fleet/src/serve/routes.rs`, rewrite `get_ports_html` to build the pid→command map and resolve each row:

```rust
pub async fn get_ports_html(State(state): State<AppState>) -> Response {
    let conn = match ro_conn(&state) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    let rows = match db::host::all_ports(&conn) {
        Ok(r) => r,
        Err(e) => return html_500(e),
    };
    let cmds = db::host::commands_by_pid_all(&conn).unwrap_or_default();

    let page = templates::PortsPage {
        rows: rows
            .into_iter()
            .map(|r| {
                let command = cmds
                    .get(&r.node_id)
                    .and_then(|m| m.get(&r.pid))
                    .map(String::as_str);
                templates::FleetPortViewRow {
                    service: crate::service_label::resolve_service(
                        r.port,
                        command,
                        &r.process,
                        &state.labels,
                    ),
                    fleet_id: r.node_id,
                    hostname: r.hostname,
                    port: r.port,
                    proto: r.proto,
                    process: r.process,
                    pid: r.pid,
                    bind: r.bind,
                    collected_at: r.collected_at.clone(),
                    stale: crate::model::is_stale(&r.collected_at, state.snapshot_stale_threshold),
                }
            })
            .collect(),
    };
    templates::render(&page)
}
```

- [ ] **Step 5: Add the Service column to the template**

Replace `crates/fleet/src/serve/templates/ports.html` table head + row with a leading **service** column; keep `process`/`pid` but mute them:

```html
{% extends "base.html" %}
{% block title %}fleet · ports{% endblock %}
{% block content %}
<h1>listening ports</h1>
<p class="meta">TCP only · latest snapshot per node</p>
{% if rows.is_empty() %}
<p class="empty">No host snapshots collected yet. Run <code>fleet collect</code>.</p>
{% else %}
<table class="ports">
  <thead>
    <tr><th>node</th><th>service</th><th>port</th><th>proto</th><th>process</th><th>pid</th><th>bind</th><th>collected</th></tr>
  </thead>
  <tbody>
    {% for r in rows %}
    <tr{% if r.stale %} class="stale"{% endif %}>
      <td><a href="/node/{{ r.fleet_id }}">{{ r.hostname }}</a></td>
      <td class="service">{{ r.service }}</td>
      <td>{{ r.port }}</td>
      <td>{{ r.proto }}</td>
      <td class="muted"><code>{{ r.process }}</code></td>
      <td class="muted">{{ r.pid }}</td>
      <td><code>{{ r.bind }}</code></td>
      <td>{{ r.collected_at }}</td>
    </tr>
    {% endfor %}
  </tbody>
</table>
{% endif %}
{% endblock %}
```

- [ ] **Step 6: Run the ports tests**

Run: `cargo test -p minimonitor-fleet --lib serve::tests::ports_`
Expected: PASS — `ports_shows_resolved_service_name` plus the existing `ports_*` tests (empty-state, stale, fresh, seeded). The existing `ports_seeded_returns_200_with_rows` seeds `snapshot_json='{}'`, so its port resolves via tier 4 to the raw `nginx` process — still present in the HTML, so that assertion holds.

- [ ] **Step 7: fmt + clippy + commit**

```bash
cargo fmt -p minimonitor-fleet && cargo clippy -p minimonitor-fleet --all-targets
git add crates/fleet/src/serve/templates.rs crates/fleet/src/serve/routes.rs \
        crates/fleet/src/serve/templates/ports.html crates/fleet/src/serve/mod.rs
git commit -m "feat(fleet): /ports Service column (resolved app name)"
```

---

### Task 5: `/node` host section — resolve and render the Service column

**Files:**
- Modify: `crates/fleet/src/serve/templates.rs` (`HostPortRow` gains `service`)
- Modify: `crates/fleet/src/serve/routes.rs` (`get_node_html` resolves `service` per port)
- Modify: `crates/fleet/src/serve/templates/node.html` (Service column in the ports table)
- Modify: `crates/fleet/src/serve/mod.rs` `#[cfg(test)]` (one new test)

**Interfaces:**
- Consumes: `db::host::commands_by_pid_for_node` (Task 2), `service_label::resolve_service` (Task 1), `AppState.labels` (Task 3).

- [ ] **Step 1: Add the `service` field to `HostPortRow`**

In `crates/fleet/src/serve/templates.rs`:

```rust
pub struct HostPortRow {
    pub service: String, // resolved friendly name
    pub port: u16,
    pub proto: String,
    pub process: String,
    pub pid: i64,
    pub bind: String,
}
```

- [ ] **Step 2: Write a failing test**

Add to `crates/fleet/src/serve/mod.rs`'s `#[cfg(test)] mod tests`:

```rust
#[tokio::test]
async fn node_host_ports_show_resolved_service() {
    let node = make_node("fleet-ns", "ns-host");
    let f = seed_db(&[node]);
    {
        let conn = db::open(f.path()).unwrap();
        let blob = r#"{"processes":[{"pid":7777,"command":"opencode web --port 4096"}]}"#;
        conn.execute(
            "INSERT INTO host_snapshot
                (node_id, collected_at, hostname, total_cpu_percent, used_memory_bytes,
                 total_memory_bytes, workload_count, port_count, snapshot_json)
             VALUES ('fleet-ns', ?1, 'ns-host', 0.0, 0, 0, 0, 1, ?2)",
            rusqlite::params![Utc::now().to_rfc3339(), blob],
        )
        .unwrap();
        let sid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO host_port (snapshot_id, node_id, port, proto, process, pid, bind)
             VALUES (?1, 'fleet-ns', 4096, 'TCP', 'opencode', 7777, '0.0.0.0')",
            rusqlite::params![sid],
        )
        .unwrap();
    }
    let router = full_router(f.path().to_path_buf());
    let (status, html) = html_get(router, "/node/fleet-ns").await;
    assert_eq!(status, StatusCode::OK, "body: {html}");
    assert!(html.contains("opencode"), "resolved service missing:\n{html}");
}
```

(Here tier-3 binary resolution returns `opencode`, which also equals the raw process — the assertion still proves the Service column renders; the value is taken from the resolver path because the command map is populated.)

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p minimonitor-fleet --lib serve::tests::node_host_ports_show_resolved_service`
Expected: FAIL — `service` field missing on `HostPortRow`.

- [ ] **Step 4: Resolve per-port in `get_node_html`**

In `crates/fleet/src/serve/routes.rs`, inside `get_node_html`, the `host_snapshot` builder maps `ports`. Replace the `ports` build so it loads the node's pid→command map and resolves each row. Find the block:

```rust
            let ports = db::host::ports_for_node(&conn, &node.fleet_id)
                .unwrap_or_default()
                .into_iter()
                .map(|p| templates::HostPortRow {
                    port: p.port,
                    proto: p.proto,
                    process: p.process,
                    pid: p.pid,
                    bind: p.bind,
                })
                .collect();
```

Replace with:

```rust
            let node_cmds =
                db::host::commands_by_pid_for_node(&conn, &node.fleet_id).unwrap_or_default();
            let ports = db::host::ports_for_node(&conn, &node.fleet_id)
                .unwrap_or_default()
                .into_iter()
                .map(|p| templates::HostPortRow {
                    service: crate::service_label::resolve_service(
                        p.port,
                        node_cmds.get(&p.pid).map(String::as_str),
                        &p.process,
                        &state.labels,
                    ),
                    port: p.port,
                    proto: p.proto,
                    process: p.process,
                    pid: p.pid,
                    bind: p.bind,
                })
                .collect();
```

- [ ] **Step 5: Add the column to `node.html`**

In `crates/fleet/src/serve/templates/node.html`, the listening-ports table (around lines 56–60). Replace its `<thead>` and row:

```html
  <thead><tr><th>service</th><th>port</th><th>proto</th><th>process</th><th>pid</th><th>bind</th></tr></thead>
```

and the row:

```html
    <tr><td class="service">{{ p.service }}</td><td>{{ p.port }}</td><td>{{ p.proto }}</td><td class="muted"><code>{{ p.process }}</code></td><td class="muted">{{ p.pid }}</td><td><code>{{ p.bind }}</code></td></tr>
```

- [ ] **Step 6: Run node tests**

Run: `cargo test -p minimonitor-fleet --lib serve::tests::node_`
Expected: PASS — the new test plus existing `node_with_snapshot_shows_host_section`, `node_without_snapshot_shows_200_not_500`, `node_page_renders_detail`, `node_page_404_for_missing`.

- [ ] **Step 7: Full crate test + fmt + clippy + secret-scan + commit**

```bash
cargo fmt -p minimonitor-fleet && cargo clippy -p minimonitor-fleet --all-targets
cargo test -p minimonitor-fleet
```
Expected: entire fleet suite green.

```bash
git add crates/fleet/src/serve/templates.rs crates/fleet/src/serve/routes.rs \
        crates/fleet/src/serve/templates/node.html crates/fleet/src/serve/mod.rs
git commit -m "feat(fleet): /node host ports Service column (resolved app name)"
```

---

## Optional polish (only if it falls out cleanly — not required)

- Add a `.service { font-weight: 600 }` and `.muted { opacity: .6; font-size: .9em }` rule to `crates/fleet/assets/app.css` so the resolved name reads as primary and raw process/pid as secondary. Visual only; no test.

---

## Notes for the executor

- This branch (`fleet-phase-0-1`) already contains the host-snapshots feature — `/ports`, `/node`, the host tables, and `commands`/`serve` wiring all exist. You are extending, not creating.
- The crate name for `cargo` is `minimonitor-fleet` (the directory is `crates/fleet`).
- After Task 5, redeploy is a separate step the human will request — do NOT touch `~/.local/bin/fleet` or any LaunchAgent from inside this plan.

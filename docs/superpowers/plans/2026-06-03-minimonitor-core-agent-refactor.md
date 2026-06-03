# MiniMonitor core + agent + menubar refactor — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Declutter the macOS menu-bar monitor (drop the token/provider cluster), enrich it (ports→process, connections, disk space, uptime, network identity, sustained-CPU), and split it into a Cargo workspace (`core` lib / headless `agent` / macOS `menubar`) so a fleet hub can grow on top without a rewrite.

**Architecture:** A cross-platform `minimonitor-core` library owns all collection and serde types. The macOS `menubar` binary links `core` directly and samples in-process. A headless `agent` binary samples via `core` and serves the latest `Snapshot` as JSON on `127.0.0.1:9909` (hub push is a stub, deferred behind a parallel Beszel spike).

**Tech Stack:** Rust (edition 2024), `sysinfo` 0.37 (cross-platform collection), `tao`/`wry`/`tray-icon` (macOS UI), `tiny_http` (agent server), `serde`/`serde_json`. Removed: `tiktoken-rs`, `keyring`, `keyring-core`, `reqwest`.

**Reference spec:** `docs/superpowers/specs/2026-06-03-minimonitor-core-agent-refactor-design.md`

---

## File structure (end state)

```
minimonitor/
  Cargo.toml                       # workspace
  crates/
    core/
      Cargo.toml
      src/lib.rs                   # re-exports
      src/snapshot.rs              # Snapshot + Sampler (+ DiskVolume, EWMA)
      src/ai.rs                    # unchanged detection logic
      src/net.rs                   # NEW ports / connections / identity
      src/util.rs
    agent/
      Cargo.toml
      src/main.rs                  # serve / --once
      src/push.rs                  # Sink trait + NoopSink (stub)
    menubar/
      Cargo.toml
      src/main.rs
      src/app.rs                   # token/provider removed; caffeinate state
      src/tray.rs                  # condensed header + ports + quick actions
      src/inspector.rs             # token/provider views removed; net panels
      src/inspector.html           # reorganized UI
      src/actions.rs               # NEW caffeinate + flush-DNS
```

**Note on "move" steps:** when a step says *move* a file, it means `git mv` then fix `use crate::…` paths to the new crate layout. The logic inside is unchanged unless the step shows a diff. New logic and all tests are given in full.

---

## Task 1: Convert to a Cargo workspace, move existing code into `menubar`

**Files:**
- Create: `Cargo.toml` (workspace), `crates/menubar/Cargo.toml`
- Move: `src/*.rs` → `crates/menubar/src/*.rs`, `src/inspector.html` → `crates/menubar/src/inspector.html`
- Modify: delete the old top-level `[package]` `Cargo.toml` content (becomes the workspace)

- [ ] **Step 1: Move sources into the menubar crate**

```bash
mkdir -p crates/menubar/src
git mv src/main.rs src/app.rs src/ai.rs src/inspector.rs src/services.rs \
       src/snapshot.rs src/tray.rs src/util.rs src/inspector.html \
       crates/menubar/src/
rmdir src
```

- [ ] **Step 2: Write the workspace `Cargo.toml`**

Replace the entire contents of `Cargo.toml` with:

```toml
[workspace]
resolver = "2"
members = ["crates/core", "crates/agent", "crates/menubar"]

[workspace.package]
version = "0.2.0"
edition = "2024"

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sysinfo = "0.37"

[profile.release]
lto = true
codegen-units = 1
strip = true

[profile.dev]
debug = "line-tables-only"
incremental = false
```

- [ ] **Step 3: Write `crates/menubar/Cargo.toml`**

```toml
[package]
name = "minimonitor"
version.workspace = true
edition.workspace = true

[[bin]]
name = "minimonitor"
path = "src/main.rs"

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
sysinfo = { workspace = true }
keyring = "4.0.0-rc.3"
keyring-core = "0.7.2"
reqwest = { version = "0.12", default-features = false, features = ["blocking", "json", "rustls-tls"] }
tiktoken-rs = "0.9"
tao = "0.34"
tray-icon = { version = "0.21", default-features = false }
wry = "0.54"
```

> Deps that will be deleted (keyring/reqwest/tiktoken) are kept *for now* so this task compiles unchanged; Task 4 removes them.

- [ ] **Step 4: Verify it still builds and runs**

Run: `cargo build`
Expected: builds successfully (same code, new layout). `crates/core` and `crates/agent` don't exist yet — temporarily remove them from `members` to compile this task, OR proceed to Task 2 which creates `core` before the next build. To keep this task self-contained, set `members = ["crates/menubar"]` here; Task 2 re-adds `core`, Task 14 re-adds `agent`.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor: convert to cargo workspace, move app into menubar crate"
```

---

## Task 2: Extract the `core` crate (snapshot, ai, util)

**Files:**
- Create: `crates/core/Cargo.toml`, `crates/core/src/lib.rs`
- Move: `crates/menubar/src/{snapshot,ai,util}.rs` → `crates/core/src/`
- Modify: `crates/menubar/src/*` imports; workspace `members`

- [ ] **Step 1: Move the collection modules into core**

```bash
mkdir -p crates/core/src
git mv crates/menubar/src/snapshot.rs crates/menubar/src/ai.rs \
       crates/menubar/src/util.rs crates/core/src/
```

- [ ] **Step 2: Write `crates/core/Cargo.toml`**

```toml
[package]
name = "minimonitor-core"
version.workspace = true
edition.workspace = true

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
sysinfo = { workspace = true }
```

- [ ] **Step 3: Write `crates/core/src/lib.rs`**

```rust
pub mod ai;
pub mod net;
pub mod snapshot;
pub mod util;
```

> `net` is created in Task 5. To compile now, create an empty placeholder so the module resolves:

```bash
printf '// ports / connections / identity — populated in Task 5\n' > crates/core/src/net.rs
```

- [ ] **Step 4: Fix intra-core imports**

In `crates/core/src/snapshot.rs` the existing `use crate::ai::…` and `use crate::util::…` already resolve within the crate — no change needed. The same file references `capture_label` from util; keep as is.

- [ ] **Step 5: Point menubar at core**

In `crates/menubar/Cargo.toml`, add under `[dependencies]`:

```toml
minimonitor-core = { path = "../core" }
```

In every `crates/menubar/src/*.rs`, replace collection-module imports:
- `use crate::ai::…`   → `use minimonitor_core::ai::…`
- `use crate::snapshot::…` → `use minimonitor_core::snapshot::…`
- `use crate::util::…`  → `use minimonitor_core::util::…`

Remove the now-dead `mod ai; mod snapshot; mod util;` lines from `crates/menubar/src/main.rs`.

- [ ] **Step 6: Restore workspace members and build**

Set `members = ["crates/core", "crates/menubar"]` in the root `Cargo.toml`.

Run: `cargo build`
Expected: builds. If `util` functions are flagged unused in `core`, that is fine (menubar uses them).

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "refactor: extract minimonitor-core (snapshot/ai/util/net)"
```

---

## Task 3: Make the core `Snapshot` serde-`Serialize`

**Files:**
- Modify: `crates/core/src/snapshot.rs`

- [ ] **Step 1: Add Serialize to the snapshot types**

In `crates/core/src/snapshot.rs`:
- Add `Serialize` to the import: `use serde::Serialize;` (already present for ProcessRow/CoreUsage — confirm).
- Change `#[derive(Clone)]` on `MonitorSnapshot` to `#[derive(Clone, Serialize)]`.
- Make `SortMode` serializable. Change `#[derive(Clone, Copy, PartialEq, Eq)]` to `#[derive(Clone, Copy, PartialEq, Eq, Serialize)]` and add `#[serde(rename_all = "lowercase")]`.

- [ ] **Step 2: Build**

Run: `cargo build -p minimonitor-core`
Expected: builds. (`load_average: (f64,f64,f64)` serializes as a 3-array — fine.)

- [ ] **Step 3: Commit**

```bash
git add crates/core/src/snapshot.rs
git commit -m "feat(core): make MonitorSnapshot serde-Serialize for agent JSON"
```

---

## Task 4: Declutter — delete services, strip token + provider

**Files:**
- Delete: `crates/menubar/src/services.rs`
- Modify: `crates/menubar/src/app.rs`, `crates/menubar/src/inspector.rs`, `crates/menubar/src/inspector.html`, `crates/menubar/src/main.rs`, `crates/menubar/Cargo.toml`

- [ ] **Step 1: Remove the dependencies**

In `crates/menubar/Cargo.toml`, delete these lines:

```toml
keyring = "4.0.0-rc.3"
keyring-core = "0.7.2"
reqwest = { version = "0.12", default-features = false, features = ["blocking", "json", "rustls-tls"] }
tiktoken-rs = "0.9"
```

- [ ] **Step 2: Delete the services module**

```bash
git rm crates/menubar/src/services.rs
```
Remove `mod services;` from `crates/menubar/src/main.rs`.

- [ ] **Step 3: Strip token/provider from `app.rs`**

In `crates/menubar/src/app.rs`:
- Remove `use keyring::use_native_store;` and the `let _ = use_native_store(false);` call in `AppState::new`.
- Remove `use crate::services::{self, TokenEstimateResult};` (or the `minimonitor_core` path if moved) — services is gone.
- Delete the `token_result: Option<TokenEstimateResult>` field and its initializer.
- In `handle_inspector_cmd`, delete the match arms: `SetProviderKey`, `ClearProviderKey`, `ValidateProvider`, `EstimateTokens`.
- In `push_inspector`, delete the `providers` vector and the `token_result`/providers arguments to `inspector::build_view` (the new signature lands in Step 4).

- [ ] **Step 4: Strip token/provider from `inspector.rs`**

In `crates/menubar/src/inspector.rs`:
- Remove `use crate::services::{ProviderState, TokenEstimateResult};`.
- In `enum InspectorCommand`, delete the variants `SetProviderKey`, `ClearProviderKey`, `ValidateProvider`, `EstimateTokens`.
- In `struct InspectorView`, delete fields `providers`, `token_result`.
- Update `build_view`'s signature to drop the `providers` and `token_result` params:

```rust
pub fn build_view(
    snapshot: &MonitorSnapshot,
    filters: &FilterState,
    status_message: Option<String>,
) -> InspectorView {
```
and delete the `providers,` and `token_result,` lines from the returned struct literal.

- [ ] **Step 5: Strip token/provider panels from `inspector.html`**

In `crates/menubar/src/inspector.html`, delete the three panels: `Token Checker`, `Provider Keys`, and keep `Status`. Remove the JS functions `estimateTokens`, `pasteTokens`, `clearTokens`, `saveProvider`, `validateProvider`, `clearProvider`, and the `token-result`/`providers` render blocks. (The full reorganized HTML is rewritten in Task 13 — this step just makes it compile/run without the removed IPC.)

- [ ] **Step 6: Build and run**

Run: `cargo build -p minimonitor`
Expected: builds with no reference to keyring/reqwest/tiktoken.

Run: `cargo run -p minimonitor` and open the inspector.
Expected: tray + inspector work; no token/provider panels.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "refactor(menubar): remove token checker + provider keys cluster"
```

---

## Task 5: `net.rs` — parse listening ports from lsof

**Files:**
- Modify: `crates/core/src/net.rs`
- Test: inline `#[cfg(test)]` in `crates/core/src/net.rs`

- [ ] **Step 1: Write the failing test**

Put this in `crates/core/src/net.rs`:

```rust
use serde::Serialize;

#[derive(Clone, Serialize, PartialEq, Debug)]
pub struct PortRow {
    pub port: u16,
    pub proto: String,
    pub process: String,
    pub pid: u32,
    pub bind: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipv4_listen_line() {
        let line = "node      8412 caguabot   23u  IPv4 0x1234      0t0  TCP 127.0.0.1:3000 (LISTEN)";
        let row = parse_listen_line(line).unwrap();
        assert_eq!(row, PortRow {
            port: 3000, proto: "TCP".into(), process: "node".into(),
            pid: 8412, bind: "127.0.0.1".into(),
        });
    }

    #[test]
    fn parses_wildcard_and_ipv6() {
        let v4 = "ttyd       901 caguabot    3u  IPv4 0x1 0t0 TCP *:7681 (LISTEN)";
        assert_eq!(parse_listen_line(v4).unwrap().bind, "*");
        let v6 = "postgres   455 caguabot    5u  IPv6 0x2 0t0 TCP [::1]:5432 (LISTEN)";
        let r = parse_listen_line(v6).unwrap();
        assert_eq!((r.port, r.bind.as_str()), (5432, "[::1]"));
    }

    #[test]
    fn skips_header_and_garbage() {
        assert!(parse_listen_line("COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME").is_none());
        assert!(parse_listen_line("too few cols").is_none());
    }

    #[test]
    fn parse_output_skips_header_row() {
        let out = "COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME\n\
                   node 8412 me 23u IPv4 0x1 0t0 TCP 127.0.0.1:3000 (LISTEN)\n";
        let rows = parse_listen_output(out);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].port, 3000);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p minimonitor-core net::`
Expected: FAIL — `parse_listen_line` / `parse_listen_output` not found.

- [ ] **Step 3: Implement the parser**

Add to `crates/core/src/net.rs` (above the tests):

```rust
use std::process::Command;

/// Parse one `lsof -nP -iTCP -sTCP:LISTEN` row into a PortRow.
/// Columns: COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME (STATE)
pub fn parse_listen_line(line: &str) -> Option<PortRow> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 9 {
        return None;
    }
    let pid: u32 = parts[1].parse().ok()?; // header row's "PID" fails here → skipped
    let proto = parts[7].to_owned();
    let name = parts[8];
    // NAME is addr:port (no "->" for LISTEN). Split on the last ':'.
    let (addr, port_str) = name.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    let bind = if addr.is_empty() { "*".to_owned() } else { addr.to_owned() };
    Some(PortRow { port, proto, process: parts[0].to_owned(), pid, bind })
}

pub fn parse_listen_output(output: &str) -> Vec<PortRow> {
    output.lines().filter_map(parse_listen_line).collect()
}

pub fn listening_ports() -> Vec<PortRow> {
    let Ok(out) = Command::new("lsof").args(["-nP", "-iTCP", "-sTCP:LISTEN"]).output() else {
        return Vec::new();
    };
    parse_listen_output(&String::from_utf8_lossy(&out.stdout))
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p minimonitor-core net::`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/net.rs
git commit -m "feat(core): parse listening ports (port→process) from lsof"
```

---

## Task 6: `net.rs` — group established connections per process

**Files:**
- Modify: `crates/core/src/net.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/core/src/net.rs`:

```rust
#[test]
fn groups_established_by_process() {
    let out = "COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME\n\
        firefox 700 me 50u IPv4 0x1 0t0 TCP 192.168.1.5:54321->1.1.1.1:443 (ESTABLISHED)\n\
        firefox 700 me 51u IPv4 0x2 0t0 TCP 192.168.1.5:54322->1.1.1.2:443 (ESTABLISHED)\n\
        claude  900 me 10u IPv4 0x3 0t0 TCP 192.168.1.5:54400->2.2.2.2:443 (ESTABLISHED)\n";
    let mut groups = parse_estab_output(out);
    groups.sort_by(|a, b| b.count.cmp(&a.count));
    assert_eq!(groups.len(), 2);
    assert_eq!((groups[0].process.as_str(), groups[0].pid, groups[0].count), ("firefox", 700, 2));
    assert_eq!((groups[1].process.as_str(), groups[1].count), ("claude", 1));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p minimonitor-core net::groups_established`
Expected: FAIL — `parse_estab_output` / `ConnGroup` not found.

- [ ] **Step 3: Implement**

Add to `crates/core/src/net.rs`:

```rust
use std::collections::HashMap;

#[derive(Clone, Serialize, PartialEq, Debug)]
pub struct ConnGroup {
    pub process: String,
    pub pid: u32,
    pub count: usize,
}

pub fn parse_estab_output(output: &str) -> Vec<ConnGroup> {
    let mut counts: HashMap<(String, u32), usize> = HashMap::new();
    for line in output.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 9 {
            continue;
        }
        let Ok(pid) = parts[1].parse::<u32>() else { continue };
        *counts.entry((parts[0].to_owned(), pid)).or_insert(0) += 1;
    }
    counts.into_iter()
        .map(|((process, pid), count)| ConnGroup { process, pid, count })
        .collect()
}

pub fn established_connections() -> Vec<ConnGroup> {
    let Ok(out) = Command::new("lsof").args(["-nP", "-iTCP", "-sTCP:ESTABLISHED"]).output() else {
        return Vec::new();
    };
    parse_estab_output(&String::from_utf8_lossy(&out.stdout))
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p minimonitor-core net::`
Expected: PASS (5 tests total).

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/net.rs
git commit -m "feat(core): group established connections per process"
```

---

## Task 7: `net.rs` — network identity (host / LAN / tailnet)

**Files:**
- Modify: `crates/core/src/net.rs`

- [ ] **Step 1: Add the type and collector (no unit test — shells out)**

Add to `crates/core/src/net.rs`:

```rust
#[derive(Clone, Serialize, Default, PartialEq, Debug)]
pub struct NetIdentity {
    pub hostname: String,
    pub lan_ip: Option<String>,
    pub tailnet_ip: Option<String>,
}

fn first_line(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if s.is_empty() { None } else { Some(s) }
}

pub fn network_identity(hostname: String) -> NetIdentity {
    NetIdentity {
        hostname,
        lan_ip: first_line("ipconfig", &["getifaddr", "en0"]),
        tailnet_ip: first_line("tailscale", &["ip", "-4"])
            .and_then(|s| s.lines().next().map(|l| l.to_owned())),
    }
}
```

> `hostname` is passed in (the Sampler already holds a `System`, which exposes `System::host_name()`), keeping `net.rs` free of a sysinfo dependency for this function and trivially testable in future.

- [ ] **Step 2: Build**

Run: `cargo build -p minimonitor-core`
Expected: builds.

- [ ] **Step 3: Commit**

```bash
git add crates/core/src/net.rs
git commit -m "feat(core): network identity (hostname / LAN IP / tailnet IP)"
```

---

## Task 8: Wire ports/connections/identity into the Sampler & Snapshot

**Files:**
- Modify: `crates/core/src/snapshot.rs`

- [ ] **Step 1: Add fields to MonitorSnapshot**

In `crates/core/src/snapshot.rs`, add to `struct MonitorSnapshot` (after `disk_write_bps`):

```rust
    pub ports: Vec<crate::net::PortRow>,
    pub connections: Vec<crate::net::ConnGroup>,
    pub identity: crate::net::NetIdentity,
```

- [ ] **Step 2: Cache them in the Sampler on the throttled cadence**

The Sampler already has a `last_localhost` / `LOCALHOST_REFRESH` 10s gate that recomputes `localhost_pids`. Reuse it. Add cached fields to `struct Sampler`:

```rust
    ports: Vec<crate::net::PortRow>,
    connections: Vec<crate::net::ConnGroup>,
    identity: crate::net::NetIdentity,
```

Initialize them in `Sampler::new()` (after `localhost_pids: collect_localhost_pids(),`):

```rust
            ports: crate::net::listening_ports(),
            connections: crate::net::established_connections(),
            identity: crate::net::network_identity(
                System::host_name().unwrap_or_default(),
            ),
```

In `sample()`, inside the existing `if now.duration_since(self.last_localhost) >= LOCALHOST_REFRESH { … }` block, add:

```rust
            self.ports = crate::net::listening_ports();
            self.connections = crate::net::established_connections();
            self.identity = crate::net::network_identity(
                System::host_name().unwrap_or_default(),
            );
```

- [ ] **Step 3: Emit them in the returned MonitorSnapshot**

In the `MonitorSnapshot { … }` literal at the end of `sample()`, add:

```rust
            ports: self.ports.clone(),
            connections: self.connections.clone(),
            identity: self.identity.clone(),
```

- [ ] **Step 4: Build and smoke-test via a temporary print**

Run: `cargo build -p minimonitor-core`
Expected: builds.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/snapshot.rs
git commit -m "feat(core): surface ports/connections/identity in snapshots"
```

---

## Task 9: Disk volumes + uptime in the Snapshot

**Files:**
- Modify: `crates/core/src/snapshot.rs`

- [ ] **Step 1: Add the DiskVolume type**

In `crates/core/src/snapshot.rs`, add near `CoreUsage`:

```rust
#[derive(Clone, Serialize)]
pub struct DiskVolume {
    pub name: String,
    pub mount: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
}
```

Add to `MonitorSnapshot`:

```rust
    pub disks: Vec<DiskVolume>,
    pub uptime_secs: u64,
    pub boot_epoch: u64,
```

- [ ] **Step 2: Collect disks (throttled) and uptime (each sample)**

Add `use sysinfo::Disks;` to the imports. Add a cached field to `Sampler`:

```rust
    disks: Disks,
```

Initialize in `Sampler::new()`:

```rust
            disks: Disks::new_with_refreshed_list(),
```

In `sample()`, inside the same 10s throttle block, refresh:

```rust
            self.disks.refresh(true);
```

Build the volume list near the end of `sample()` (before the snapshot literal):

```rust
        let disks: Vec<DiskVolume> = self
            .disks
            .iter()
            .map(|d| DiskVolume {
                name: d.name().to_string_lossy().into_owned(),
                mount: d.mount_point().to_string_lossy().into_owned(),
                total_bytes: d.total_space(),
                available_bytes: d.available_space(),
            })
            .collect();
```

Add to the `MonitorSnapshot { … }` literal:

```rust
            disks,
            uptime_secs: System::uptime(),
            boot_epoch: System::boot_time(),
```

- [ ] **Step 3: Build**

Run: `cargo build -p minimonitor-core`
Expected: builds.

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/snapshot.rs
git commit -m "feat(core): disk-volume capacity + uptime/boot in snapshot"
```

---

## Task 10: Sustained-CPU EWMA (honest energy proxy)

**Files:**
- Modify: `crates/core/src/snapshot.rs`

- [ ] **Step 1: Write the failing test for the EWMA helper**

Add to the bottom of `crates/core/src/snapshot.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::ewma_update;

    #[test]
    fn ewma_seeds_with_first_value() {
        assert_eq!(ewma_update(None, 40.0, 0.2), 40.0);
    }

    #[test]
    fn ewma_decays_toward_current() {
        // prev 0, current 100, alpha 0.2 → 20
        assert!((ewma_update(Some(0.0), 100.0, 0.2) - 20.0).abs() < 1e-4);
        // prev 50, current 0, alpha 0.2 → 40
        assert!((ewma_update(Some(50.0), 0.0, 0.2) - 40.0).abs() < 1e-4);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p minimonitor-core ewma`
Expected: FAIL — `ewma_update` not found.

- [ ] **Step 3: Implement the helper + wire it in**

Add the pure helper to `crates/core/src/snapshot.rs`:

```rust
pub fn ewma_update(prev: Option<f32>, current: f32, alpha: f32) -> f32 {
    match prev {
        Some(p) => p * (1.0 - alpha) + current * alpha,
        None => current,
    }
}
```

Add a field to `ProcessRow`:

```rust
    pub sustained_cpu: f32,
```

Add a cache to `Sampler`:

```rust
    cpu_ewma: std::collections::HashMap<u32, f32>,
```

Initialize in `Sampler::new()`:

```rust
            cpu_ewma: std::collections::HashMap::new(),
```

In `sample()`, while building each `ProcessRow`, compute and store the EWMA. Replace the `ProcessRow { … }` construction so it includes:

```rust
                let pid_u32 = pid.as_u32();
                let sustained_cpu = ewma_update(
                    self.cpu_ewma.get(&pid_u32).copied(),
                    process.cpu_usage(),
                    0.2,
                );
```

and add `sustained_cpu,` to the struct literal, and after the `.collect()` of processes, prune dead PIDs and store fresh values:

```rust
        let alive: std::collections::HashSet<u32> =
            processes.iter().map(|p| p.pid).collect();
        self.cpu_ewma.retain(|pid, _| alive.contains(pid));
        for p in &processes {
            self.cpu_ewma.insert(p.pid, p.sustained_cpu);
        }
```

> Note: the EWMA is computed from the *previous* stored value, then written back — so the prune/store block runs after the per-process map. Ensure `sustained_cpu` is read into the row before this write-back.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p minimonitor-core`
Expected: PASS (ewma + net tests).

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/snapshot.rs
git commit -m "feat(core): sustained-CPU EWMA per process (energy proxy)"
```

---

## Task 11: `menubar/actions.rs` — caffeinate + flush-DNS

**Files:**
- Create: `crates/menubar/src/actions.rs`
- Modify: `crates/menubar/src/main.rs` (add `mod actions;`)

- [ ] **Step 1: Write the module**

Create `crates/menubar/src/actions.rs`:

```rust
use std::process::{Child, Command};

/// Holds the `caffeinate` child while keep-awake is on.
pub struct Caffeinate {
    child: Option<Child>,
}

impl Caffeinate {
    pub fn new() -> Self {
        Self { child: None }
    }

    pub fn is_on(&self) -> bool {
        self.child.is_some()
    }

    /// Turn keep-awake on/off. Returns the resulting state.
    pub fn set(&mut self, on: bool) -> bool {
        if on {
            if self.child.is_none() {
                self.child = Command::new("caffeinate").arg("-dimsu").spawn().ok();
            }
        } else if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.is_on()
    }
}

impl Drop for Caffeinate {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
        }
    }
}

/// Flush the macOS DNS cache. Requires root, so this runs via osascript and
/// pops the native administrator-password dialog.
pub fn flush_dns() -> Result<(), String> {
    let script = "do shell script \"dscacheutil -flushcache; killall -HUP mDNSResponder\" \
                  with administrator privileges";
    let status = Command::new("osascript")
        .args(["-e", script])
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("flush-dns exited with {status}"))
    }
}
```

- [ ] **Step 2: Register the module**

Add `mod actions;` to `crates/menubar/src/main.rs`.

- [ ] **Step 3: Build**

Run: `cargo build -p minimonitor`
Expected: builds (functions unused until Task 12 — that warning is fine for this step).

- [ ] **Step 4: Commit**

```bash
git add crates/menubar/src/actions.rs crates/menubar/src/main.rs
git commit -m "feat(menubar): caffeinate keep-awake + flush-DNS actions"
```

---

## Task 12: Condensed tray header + ports submenu + quick actions

**Files:**
- Modify: `crates/menubar/src/tray.rs`, `crates/menubar/src/app.rs`

- [ ] **Step 1: Add tray imports and condensed header**

In `crates/menubar/src/tray.rs`, ensure these imports:

```rust
use minimonitor_core::snapshot::{MonitorSnapshot, SortMode, is_visible};
use minimonitor_core::util::{format_bytes, format_bytes_pair, format_rate, slugify, truncate_name};
```

Replace the body of `build_menu` so the header is three dense lines plus the submenus. Replace the existing per-metric `MenuItem`s and the `append_items` call with:

```rust
    let line1 = MenuItem::new(
        format!(
            "CPU {:.0}%   RAM {}   {}",
            snapshot.total_cpu_percent,
            format_bytes_pair(snapshot.used_memory_bytes, snapshot.total_memory_bytes),
            match snapshot.gpu_percent { Some(v) => format!("GPU {v:.0}%"), None => "GPU n/a".into() },
        ),
        false, None,
    );
    let line2 = MenuItem::new(
        format!(
            "Net ↓{} ↑{}   Disk ↓{} ↑{}",
            format_rate(snapshot.net_rx_bps), format_rate(snapshot.net_tx_bps),
            format_rate(snapshot.disk_read_bps), format_rate(snapshot.disk_write_bps),
        ),
        false, None,
    );
    let line3 = MenuItem::new(
        format!(
            "Load {:.2} {:.2} {:.2}   Up {}",
            snapshot.load_average.0, snapshot.load_average.1, snapshot.load_average.2,
            format_uptime(snapshot.uptime_secs),
        ),
        false, None,
    );

    let ports = build_ports_submenu(snapshot);
    let processes = build_processes_submenu(snapshot);
    let ai_sub = build_ai_submenu(snapshot);
    let quick = build_quick_actions_submenu();

    let show_inspector = MenuItem::with_id("show-inspector", "Show Inspector", true, None);
    let refresh = MenuItem::with_id("refresh-menu", "Refresh snapshot", true, None);
    let sort_cpu = MenuItem::with_id("sort:cpu",
        if sort_mode == SortMode::Cpu { "Sort: CPU •" } else { "Sort: CPU" }, true, None);
    let sort_ram = MenuItem::with_id("sort:ram",
        if sort_mode == SortMode::Memory { "Sort: RAM •" } else { "Sort: RAM" }, true, None);
    let quit = MenuItem::with_id("quit", "Quit MiniMonitor", true, None);
    let sep1 = PredefinedMenuItem::separator();
    let sep2 = PredefinedMenuItem::separator();
    let sep3 = PredefinedMenuItem::separator();

    let _ = menu.append_items(&[
        &line1, &line2, &line3, &sep1,
        &ports, &processes, &ai_sub, &quick, &sep2,
        &show_inspector, &refresh, &sort_cpu, &sort_ram,
    ]);

    if let Some(msg) = status {
        let s = PredefinedMenuItem::separator();
        let item = MenuItem::new(msg, false, None);
        let _ = menu.append(&s);
        let _ = menu.append(&item);
    }
    let _ = menu.append(&sep3);
    let _ = menu.append(&quit);
    menu
```

Delete the now-unused `cpu_ram`, `swap`, `gpu`, `load`, `net`, `disk`, `ai_summary`, `captured` locals from the old body.

- [ ] **Step 2: Add the uptime formatter + ports submenu + quick actions submenu**

Append to `crates/menubar/src/tray.rs`:

```rust
fn format_uptime(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3_600;
    let m = (secs % 3_600) / 60;
    if d > 0 { format!("{d}d {h}h") } else if h > 0 { format!("{h}h {m}m") } else { format!("{m}m") }
}

fn build_ports_submenu(snapshot: &MonitorSnapshot) -> Submenu {
    let submenu = Submenu::new(
        format!("Listening ports ({})", snapshot.ports.len()), true);
    if snapshot.ports.is_empty() {
        let _ = submenu.append(&MenuItem::new("No listening TCP ports", false, None));
        return submenu;
    }
    let mut ports = snapshot.ports.clone();
    ports.sort_by_key(|p| p.port);
    for p in ports.iter().take(MAX_MENU_PROCESSES * 2) {
        let child = Submenu::with_id(
            format!("port:{}", p.pid),
            format!("{} • {} • {}", p.port, truncate_name(&p.process, 18), p.bind),
            true,
        );
        let pid_item = MenuItem::new(format!("PID {}", p.pid), false, None);
        let bind = MenuItem::new(format!("Bind {} {}", p.proto, p.bind), false, None);
        let sep = PredefinedMenuItem::separator();
        let kill = MenuItem::with_id(format!("kill:{}", p.pid), "Kill owner", true, None);
        let _ = child.append_items(&[&pid_item, &bind, &sep, &kill]);
        let _ = submenu.append(&child);
    }
    submenu
}

fn build_quick_actions_submenu() -> Submenu {
    let submenu = Submenu::new("Quick actions", true);
    let caffeinate = MenuItem::with_id("action:caffeinate", "Toggle keep-awake", true, None);
    let flush = MenuItem::with_id("action:flush-dns", "Flush DNS (admin)", true, None);
    let _ = submenu.append_items(&[&caffeinate, &flush]);
    submenu
}
```

- [ ] **Step 3: Handle the new menu IDs in `app.rs`**

In `crates/menubar/src/app.rs`, add a `caffeinate: crate::actions::Caffeinate` field to `AppState`, initialized with `crate::actions::Caffeinate::new()`.

In `handle_menu`, add arms before the `_ if id.starts_with("kill:")` arm:

```rust
            "action:caffeinate" => {
                let on = self.caffeinate.set(!self.caffeinate.is_on());
                self.status_message = Some(
                    if on { "Keep-awake ON (caffeinate)" } else { "Keep-awake OFF" }.to_owned());
            }
            "action:flush-dns" => {
                self.status_message = Some(match crate::actions::flush_dns() {
                    Ok(()) => "Flushed DNS cache".to_owned(),
                    Err(e) => format!("Flush DNS failed: {e}"),
                });
            }
```

(The existing `kill:` arm already covers `port:` submenu kills, since those use `kill:<pid>` IDs.)

- [ ] **Step 4: Build and run**

Run: `cargo run -p minimonitor`
Expected: 3-line header; "Listening ports (n)" submenu lists port • proc • bind with a "Kill owner"; "Quick actions" toggles keep-awake and flushes DNS (admin prompt).

- [ ] **Step 5: Commit**

```bash
git add crates/menubar/src/tray.rs crates/menubar/src/app.rs
git commit -m "feat(menubar): condensed tray header + ports & quick-action submenus"
```

---

## Task 13: Reorganize the inspector (HTML + view)

**Files:**
- Modify: `crates/menubar/src/inspector.rs`, `crates/menubar/src/inspector.html`

- [ ] **Step 1: Extend SummaryView and InspectorView for the new data**

In `crates/menubar/src/inspector.rs`:

Add imports:
```rust
use minimonitor_core::net::{ConnGroup, NetIdentity, PortRow};
use minimonitor_core::snapshot::DiskVolume;
```

Add to `struct SummaryView`:
```rust
    pub uptime_secs: u64,
    pub hostname: String,
    pub lan_ip: Option<String>,
    pub tailnet_ip: Option<String>,
```

Add to `struct InspectorView` (and the `build_view` returned literal):
```rust
    pub ports: Vec<PortRow>,
    pub connections: Vec<ConnGroup>,
    pub disks: Vec<DiskVolume>,
```

In `build_view`, populate the new summary fields from `snapshot.identity` / `snapshot.uptime_secs`, and pass `ports: snapshot.ports.clone()`, `connections: snapshot.connections.clone()`, `disks: snapshot.disks.clone()`.

- [ ] **Step 2: Replace `inspector.html` with the reorganized layout**

Overwrite `crates/menubar/src/inspector.html` with:

```html
<!doctype html>
<html>
<head>
  <meta charset="utf-8" />
  <title>MiniMonitor Inspector</title>
  <style>
    :root { color-scheme: dark; --bg:#0d1014; --panel:#151a20; --line:#27313b;
      --text:#edf1f6; --muted:#9aa8b9; --ok:#55c27b; --warn:#ffb86c; --bad:#ff6b6b; --accent:#6aa9ff; }
    body { margin:0; font:12px/1.35 ui-monospace,SFMono-Regular,Menlo,monospace;
      background:radial-gradient(circle at top,#1d2630 0%,#0d1014 55%); color:var(--text); }
    .app { padding:12px; display:grid; gap:10px; }
    .row { display:flex; gap:8px; align-items:center; flex-wrap:wrap; }
    .panel { background:rgba(21,26,32,.94); border:1px solid var(--line); border-radius:12px; padding:10px; }
    input, select, button { background:#0f1419; color:var(--text); border:1px solid #32404e; border-radius:8px; padding:6px 8px; font:inherit; }
    button { cursor:pointer; } button.danger { border-color:#6a3434; color:#ff9e9e; }
    .stats { display:grid; grid-template-columns:repeat(6,minmax(0,1fr)); gap:8px; }
    .stat { background:#0f1419; border:1px solid #24303b; border-radius:10px; padding:8px; position:relative; overflow:hidden; }
    .stat .fill { position:absolute; bottom:0; left:0; height:3px; background:var(--accent); opacity:.75; }
    .label { color:var(--muted); font-size:10px; text-transform:uppercase; letter-spacing:.08em; }
    .value { font-size:15px; margin-top:3px; font-weight:500; }
    .sub { color:var(--muted); font-size:10px; margin-top:2px; }
    .muted { color:var(--muted); }
    table { width:100%; border-collapse:collapse; }
    th, td { text-align:left; padding:5px 6px; border-bottom:1px solid #1f2831; vertical-align:middle; }
    th { color:var(--muted); font-size:10px; text-transform:uppercase; }
    .pill { display:inline-block; padding:1px 6px; border-radius:999px; border:1px solid #354556; color:var(--muted); font-size:10px; margin-right:4px; }
    .split { display:grid; grid-template-columns:1.5fr 1fr; gap:10px; }
    .trio { display:grid; grid-template-columns:repeat(3,1fr); gap:10px; }
    .cores { display:grid; grid-template-columns:repeat(auto-fill,minmax(88px,1fr)); gap:4px; margin-top:8px; }
    .core { background:#0f1419; border:1px solid #24303b; border-radius:6px; padding:4px 6px; font-size:10px; position:relative; overflow:hidden; }
    .core .bar { position:absolute; top:0; bottom:0; left:0; background:linear-gradient(90deg,rgba(106,169,255,.25),rgba(85,194,123,.35)); }
    .core .tag { position:relative; z-index:1; display:flex; justify-content:space-between; }
    .scroll { max-height:360px; overflow:auto; }
    .proc-name { max-width:240px; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
    h3 { margin:0 0 8px 0; font-size:12px; letter-spacing:.04em; }
    .dot { display:inline-block; width:6px; height:6px; border-radius:50%; margin-right:4px; }
    .dot.ok { background:var(--ok);} .dot.warn{background:var(--warn);} .dot.bad{background:var(--bad);}
    .sys { display:flex; gap:18px; flex-wrap:wrap; }
    .sys b { color:var(--text); } .sys span { color:var(--muted); }
  </style>
</head>
<body>
  <div class="app">
    <div class="row">
      <button onclick="send({type:'refresh'})">Refresh</button>
      <button onclick="send({type:'close'})">Hide</button>
      <input id="search" placeholder="Search pid or name" style="min-width:200px" oninput="render()" />
      <label class="muted"><input id="current-user" type="checkbox" checked onchange="render()" /> me only</label>
      <label class="muted"><input id="localhost-only" type="checkbox" onchange="render()" /> localhost</label>
      <select id="sort" onchange="setSort(this.value)">
        <option value="CPU">CPU</option><option value="RAM">RAM</option><option value="ENERGY">Energy</option>
      </select>
      <span id="capture" class="muted" style="margin-left:auto"></span>
    </div>

    <div class="panel sys" id="system"></div>
    <div id="stats" class="stats"></div>
    <div class="panel"><h3>Per-core CPU</h3><div id="cores" class="cores"></div></div>

    <div class="split">
      <div class="panel">
        <h3>Processes</h3>
        <div class="scroll">
          <table>
            <thead><tr><th>Process</th><th>PID</th><th>CPU</th><th>Sust</th><th>RAM</th><th>Flags</th><th></th></tr></thead>
            <tbody id="process-rows"></tbody>
          </table>
        </div>
      </div>
      <div class="panel">
        <h3>Listening ports</h3>
        <div class="scroll">
          <table>
            <thead><tr><th>Port</th><th>Process</th><th>PID</th><th>Bind</th><th></th></tr></thead>
            <tbody id="port-rows"></tbody>
          </table>
        </div>
      </div>
    </div>

    <div class="trio">
      <div class="panel"><h3>Disk volumes</h3><div id="disks" class="muted"></div></div>
      <div class="panel"><h3>Connections</h3><div id="connections" class="muted"></div></div>
      <div class="panel"><h3>AI workloads</h3><div id="ai-workloads" class="muted"></div></div>
    </div>

    <div class="split">
      <div class="panel">
        <h3>Quick actions</h3>
        <div class="row">
          <button onclick="send({type:'action-caffeinate'})">Toggle keep-awake</button>
          <button onclick="send({type:'action-flush-dns'})">Flush DNS (admin)</button>
        </div>
      </div>
      <div class="panel"><h3>Status</h3><div id="status" class="muted"></div></div>
    </div>
  </div>

  <script>
    const state = { data: null };
    function send(p) { window.ipc.postMessage(JSON.stringify(p)); }
    function setSort(v) { send({ type:'set-sort', value:v }); }
    function confirmKill(pid, name) { if (confirm('Kill PID ' + pid + ' (' + name + ')?')) send({ type:'kill', pid }); }
    function esc(v){ return String(v ?? '').replaceAll('&','&amp;').replaceAll('<','&lt;').replaceAll('>','&gt;'); }
    function bytes(b){ const u=['B','KB','MB','GB','TB']; let v=b,i=0; while(v>=1024&&i<u.length-1){v/=1024;i++;} return i===0?b+' B':v.toFixed(1)+' '+u[i]; }
    function heatDot(p){ return p>80?'bad':p>50?'warn':'ok'; }
    function uptime(s){ const d=Math.floor(s/86400),h=Math.floor(s%86400/3600),m=Math.floor(s%3600/60); return d>0?`${d}d ${h}h`:h>0?`${h}h ${m}m`:`${m}m`; }

    function render() {
      if (!state.data) return;
      const d = state.data, s = d.summary;
      document.getElementById('capture').textContent = 'Snapshot ' + d.captured_at;
      document.getElementById('sort').value = d.filters.sort_mode;

      document.getElementById('system').innerHTML =
        `<span>host <b>${esc(s.hostname || '—')}</b></span>` +
        `<span>LAN <b>${esc(s.lan_ip || '—')}</b></span>` +
        `<span>Tailnet <b>${esc(s.tailnet_ip || '—')}</b></span>` +
        `<span>Up <b>${uptime(s.uptime_secs)}</b></span>`;

      const stats = [
        ['CPU', s.cpu_percent.toFixed(0)+'%', 'load '+s.load_average[0].toFixed(2), s.cpu_percent],
        ['RAM', s.ram_label, s.ram_percent.toFixed(0)+'% used', s.ram_percent],
        ['Swap', s.swap_label, '', 0],
        ['GPU', s.gpu_percent==null?'n/a':s.gpu_percent.toFixed(0)+'%', '', s.gpu_percent ?? 0],
        ['Net', '↓ '+s.net_rx, '↑ '+s.net_tx, 0],
        ['Disk I/O', '↓ '+s.disk_read, '↑ '+s.disk_write, 0],
      ];
      document.getElementById('stats').innerHTML = stats.map(([l,v,sub,pct])=>`
        <div class="stat"><div class="label">${esc(l)}</div><div class="value">${esc(v)}</div>
        <div class="sub">${esc(sub)}</div>${pct?`<div class="fill" style="width:${Math.min(100,pct)}%"></div>`:''}</div>`).join('');

      document.getElementById('cores').innerHTML = d.cores.map(c=>`
        <div class="core"><div class="bar" style="width:${Math.min(100,c.percent)}%"></div>
        <div class="tag"><span>C${c.index}</span><span>${c.percent.toFixed(0)}%</span></div></div>`).join('');

      const q=document.getElementById('search').value.toLowerCase();
      const meOnly=document.getElementById('current-user').checked;
      const lhOnly=document.getElementById('localhost-only').checked;
      const rows=d.processes.filter(p=>(!meOnly||p.current_user)&&(!lhOnly||p.localhost)&&
        (!q||p.name.toLowerCase().includes(q)||String(p.pid).includes(q)));
      document.getElementById('process-rows').innerHTML = rows.map(p=>`
        <tr><td class="proc-name" title="${esc(p.command)}">${esc(p.name)}</td><td>${p.pid}</td>
        <td><span class="dot ${heatDot(p.cpu_percent)}"></span>${p.cpu_percent.toFixed(1)}%</td>
        <td class="muted">${p.sustained_cpu.toFixed(1)}%</td><td>${esc(bytes(p.memory_bytes))}</td>
        <td>${p.localhost?'<span class="pill">local</span>':''}${p.ai_label?`<span class="pill">${esc(p.ai_label)}</span>`:''}</td>
        <td><button class="danger" onclick="confirmKill(${p.pid}, ${JSON.stringify(p.name)})">Kill</button></td></tr>`).join('');

      const ports=[...d.ports].sort((a,b)=>a.port-b.port);
      document.getElementById('port-rows').innerHTML = ports.length ? ports.map(p=>`
        <tr><td>${p.port}</td><td class="proc-name">${esc(p.process)}</td><td>${p.pid}</td>
        <td class="muted">${esc(p.bind)}</td>
        <td><button class="danger" onclick="confirmKill(${p.pid}, ${JSON.stringify(p.process)})">Kill</button></td></tr>`).join('')
        : '<tr><td colspan="5" class="muted">No listening ports</td></tr>';

      document.getElementById('disks').innerHTML = d.disks.length ? d.disks.map(v=>{
        const used=v.total_bytes-v.available_bytes, pct=v.total_bytes? used/v.total_bytes*100:0;
        return `<div style="margin-bottom:6px"><b>${esc(v.name||v.mount)}</b>
          <div class="muted">${bytes(v.available_bytes)} free of ${bytes(v.total_bytes)} (${pct.toFixed(0)}% used)</div></div>`;
      }).join('') : 'No volumes';

      const conns=[...d.connections].sort((a,b)=>b.count-a.count).slice(0,12);
      document.getElementById('connections').innerHTML = conns.length
        ? conns.map(c=>`<div>${esc(c.process)} <span class="muted">→ ${c.count}</span></div>`).join('')
        : 'No established connections';

      document.getElementById('ai-workloads').innerHTML = d.ai_workloads.length
        ? d.ai_workloads.map(w=>`<div style="margin-bottom:6px"><b>${esc(w.label)}</b>
          <div class="muted">${esc(w.category)} · ${w.process_count} proc · ${w.total_cpu_percent.toFixed(0)}% CPU · ${bytes(w.total_memory_bytes)}</div></div>`).join('')
        : 'No inferred AI workloads';

      document.getElementById('status').textContent = d.status_message
        || 'Tray updates live; inspector freezes on a snapshot until you refresh.';
    }
    window.updateFromRust = function(next){ state.data = next; render(); };
  </script>
</body>
</html>
```

- [ ] **Step 3: Add the `ENERGY` sort + new IPC commands**

In `crates/menubar/src/inspector.rs`, add IPC variants to `enum InspectorCommand`:

```rust
    ActionCaffeinate,
    ActionFlushDns,
```

In `crates/menubar/src/app.rs` `handle_inspector_cmd`, add arms:

```rust
            InspectorCommand::ActionCaffeinate => {
                let on = self.caffeinate.set(!self.caffeinate.is_on());
                self.status_message = Some(
                    if on { "Keep-awake ON (caffeinate)" } else { "Keep-awake OFF" }.to_owned());
            }
            InspectorCommand::ActionFlushDns => {
                self.status_message = Some(match crate::actions::flush_dns() {
                    Ok(()) => "Flushed DNS cache".to_owned(),
                    Err(e) => format!("Flush DNS failed: {e}"),
                });
            }
```

Extend the `SetSort` handling in `app.rs` to recognize energy. Replace the existing `SetSort` arm body with:

```rust
            InspectorCommand::SetSort { value } => {
                self.sort_mode = match value.to_ascii_lowercase().as_str() {
                    "ram" => SortMode::Memory,
                    "energy" => SortMode::Energy,
                    _ => SortMode::Cpu,
                };
                self.refresh_live();
                self.presentation = Some(self.live.clone());
            }
```

- [ ] **Step 4: Add the `Energy` SortMode variant in core**

In `crates/core/src/snapshot.rs`:
- Add `Energy` to `enum SortMode`.
- In `SortMode::label`, add `Self::Energy => "Energy"`.
- In `sample()`'s `match sort_mode`, add an `Energy` arm:

```rust
            SortMode::Energy => processes.sort_by(|a, b| {
                b.sustained_cpu
                    .partial_cmp(&a.sustained_cpu)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.memory_bytes.cmp(&a.memory_bytes))
            }),
```

Update the serde rename: `SortMode` serializes lowercase, so `Energy` → `"energy"`, matching the HTML `<option value="ENERGY">` after the JS sends it; the view's `filters.sort_mode` uses `SortMode::label()` which returns `"Energy"` — set the `<select>` value accordingly. To keep the `<select>` in sync, the JS sets `document.getElementById('sort').value = d.filters.sort_mode` where `sort_mode` is the label `"CPU"`/`"RAM"`/`"Energy"`; ensure the `<option>` values match those exact strings. Change the HTML options to `value="CPU"`, `value="RAM"`, `value="Energy"` and `setSort` sends them verbatim; the Rust match lowercases, so it still resolves.

> Fix the HTML option in Step 2's file accordingly: `<option value="Energy">Energy</option>` (not `ENERGY`). Apply that one-character correction.

- [ ] **Step 5: Build and run**

Run: `cargo run -p minimonitor`
Expected: System strip shows host/LAN/tailnet/uptime; Processes table has a "Sust" column; Listening-ports table lists port→process→pid→bind with kill; Disk volumes show free/total; Connections list process→count; Energy sort reorders by sustained CPU; quick-action buttons work.

- [ ] **Step 6: Commit**

```bash
git add crates/menubar/src/inspector.rs crates/menubar/src/inspector.html crates/menubar/src/app.rs crates/core/src/snapshot.rs
git commit -m "feat(menubar): reorganized inspector — ports/disk/connections/identity + energy sort"
```

---

## Task 14: The headless `agent` crate

**Files:**
- Create: `crates/agent/Cargo.toml`, `crates/agent/src/main.rs`, `crates/agent/src/push.rs`
- Modify: root `Cargo.toml` (`members`)

- [ ] **Step 1: Write `crates/agent/Cargo.toml`**

```toml
[package]
name = "minimonitor-agent"
version.workspace = true
edition.workspace = true

[[bin]]
name = "minimonitor-agent"
path = "src/main.rs"

[dependencies]
minimonitor-core = { path = "../core" }
serde_json = { workspace = true }
tiny_http = "0.12"
```

- [ ] **Step 2: Write the push stub `crates/agent/src/push.rs`**

```rust
/// A sink for snapshots. The real HTTP-to-hub sink is intentionally deferred
/// until the Beszel build-vs-buy spike resolves (see the refactor spec §1).
pub trait Sink {
    fn send(&self, _snapshot_json: &str) {}
}

/// Default no-op sink: serve-only, no hub push.
pub struct NoopSink;
impl Sink for NoopSink {}
```

- [ ] **Step 3: Write `crates/agent/src/main.rs`**

```rust
mod push;

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use minimonitor_core::snapshot::{Sampler, SortMode};

fn main() {
    let once = std::env::args().any(|a| a == "--once");
    let mut sampler = Sampler::new();

    if once {
        let snap = sampler.sample(SortMode::Cpu);
        println!("{}", serde_json::to_string_pretty(&snap).unwrap());
        return;
    }

    let first = serde_json::to_string(&sampler.sample(SortMode::Cpu)).unwrap();
    let latest = Arc::new(Mutex::new(first));

    {
        let latest = latest.clone();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(1));
            let snap = sampler.sample(SortMode::Cpu);
            if let Ok(json) = serde_json::to_string(&snap) {
                *latest.lock().unwrap() = json;
            }
        });
    }

    let addr = "127.0.0.1:9909";
    let server = tiny_http::Server::http(addr).expect("agent failed to bind 127.0.0.1:9909");
    eprintln!("minimonitor-agent serving http://{addr}/snapshot");

    for request in server.incoming_requests() {
        let (body, content_type) = match request.url() {
            "/healthz" => ("ok".to_owned(), "text/plain"),
            _ => (latest.lock().unwrap().clone(), "application/json"),
        };
        let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
            .unwrap();
        let _ = request.respond(tiny_http::Response::from_string(body).with_header(header));
    }
}
```

- [ ] **Step 4: Add the crate to the workspace**

In the root `Cargo.toml`, set:

```toml
members = ["crates/core", "crates/agent", "crates/menubar"]
```

- [ ] **Step 5: Build and smoke-test**

Run: `cargo run -p minimonitor-agent -- --once | head -c 400`
Expected: prints a JSON object with `total_cpu_percent`, `ports`, `disks`, etc.

Run (in one terminal): `cargo run -p minimonitor-agent`
Then: `curl -s localhost:9909/snapshot | head -c 200` and `curl -s localhost:9909/healthz`
Expected: JSON snapshot; `ok`.

- [ ] **Step 6: Commit**

```bash
git add crates/agent root Cargo.toml
git commit -m "feat(agent): headless snapshot server (/snapshot, /healthz) + push stub"
```

---

## Task 15: README + final verification

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Rewrite the README to match the new shape**

Replace `README.md` content describing the workspace, removing token/provider claims, documenting:
- `cargo run -p minimonitor` (menu bar), `cargo run -p minimonitor-agent` (headless), `--once`.
- Tray header + Listening ports + Quick actions; inspector panels.
- A "Roadmap / fleet" note pointing at the two spec docs and that hub push is deferred behind the Beszel spike.

```markdown
# MiniMonitor

Lean macOS menu-bar system monitor + a cross-platform collection core and a
headless agent — the seed of a lightweight fleet control center.

## Workspace
- `crates/core` — cross-platform collection library (`sysinfo` + macOS `lsof`/`ioreg`).
- `crates/agent` — headless; serves `GET /snapshot` (JSON) on `127.0.0.1:9909`.
- `crates/menubar` — macOS tray + inspector (links `core`, samples in-process).

## Run
```bash
cargo run -p minimonitor          # menu-bar app (macOS)
cargo run -p minimonitor-agent    # headless server on :9909
cargo run -p minimonitor-agent -- --once   # one JSON snapshot to stdout
```

## What it shows
Tray: 3-line header (CPU/RAM/GPU · Net/Disk · Load/Uptime), Listening ports
(port→process, kill the owner), Top processes, AI workloads, Quick actions
(keep-awake via caffeinate, flush DNS). Inspector adds per-core CPU, a process
table with sustained-CPU/energy sort, listening ports, disk-volume capacity,
established-connection counts, and network identity (host/LAN/tailnet).

## Roadmap
A fleet hub (scrape/store/dashboard) is deferred pending a build-vs-buy spike of
Beszel + Uptime-Kuma. See `docs/superpowers/specs/2026-06-03-*`.
```

- [ ] **Step 2: Full workspace verification**

Run: `cargo build`
Expected: all three crates build.

Run: `cargo test`
Expected: PASS — `core` net + ewma tests green.

Run: `cargo clippy --workspace`
Expected: no errors (warnings acceptable).

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: README for the workspace (menubar + core + agent)"
```

---

## Self-review notes (coverage check vs spec)

- §2 structure → Tasks 1,2,14. §3.1 removals → Task 4. §3.2 core additions: ports/conns/identity → Tasks 5–8; disk/uptime → Task 9; EWMA → Task 10. §3.3 menubar: actions → Task 11; tray → Task 12; inspector → Task 13. §3.4 agent → Task 14. §4 deps → Tasks 1,3,4,14. §5 testing → Tasks 5,6,10 + run steps. §6 sequencing mirrored by task order. §7 out-of-scope honored (push stubbed in Task 14; Linux ports/GPU not implemented; DND omitted).
- Type consistency: `PortRow`, `ConnGroup`, `NetIdentity`, `DiskVolume`, `sustained_cpu`, `SortMode::Energy`, `InspectorCommand::{ActionCaffeinate,ActionFlushDns}` defined once and referenced consistently. `kill:<pid>` IDs reused by the ports submenu (no separate command).
- Known follow-ups (named, not silent): Linux `ss`/GPU parsers; agent→hub `ureq` push; menu-bar-as-agent-client.
```

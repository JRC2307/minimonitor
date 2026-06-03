# MiniMonitor → core + agent + menubar refactor

**Date:** 2026-06-03
**Status:** Approved design, ready for implementation plan
**Scope:** This repo (`tools/minimonitor`). One self-contained PR.

---

## 1. Why

MiniMonitor today is a single macOS binary that *both* collects and displays system
metrics, on one machine, in memory, with no history. That is the right shape for a
menu-bar widget and the wrong shape for the eventual goal: a **lightweight hub that
monitors a fleet** (the Mac mini today; a NAS, a Linux inference box, a few Hetzners,
and eventually client servers later).

This refactor does two things at once:

1. **Declutters and enriches** the menu-bar tool (remove the AI/LLM token + provider
   cluster; add ports, disk space, uptime, network identity, quick actions).
2. **Restructures** the code into a cross-platform collection library plus a headless
   agent, so the fleet vision can grow on top **without a rewrite** — while keeping the
   menu-bar app fully working and standalone.

### Deliberately deferred (de-risking the parallel Beszel spike)

A parallel ~1hr spike evaluates Beszel + Uptime-Kuma as off-the-shelf fleet tooling
(see `2026-06-03-beszel-spike-runbook.md`). To ensure this refactor is **not wasted**
whatever that spike concludes, the parts that *compete* with Beszel are stubbed:

- **The hub itself** — not built here.
- **Agent → hub push protocol** — stubbed (a clean hook, no wire format committed). The
  spike decides whether we ever build our own transport or adopt Beszel's.

The guaranteed-value parts (decluttered/enriched menu bar, clean `core` lib, a thin
local `/snapshot` agent for testing) land regardless.

---

## 2. Target structure (Cargo workspace)

```
minimonitor/
  Cargo.toml                       # workspace manifest
  crates/
    core/        (lib)             # collection + types + serde — CROSS-PLATFORM
      src/lib.rs
      src/snapshot.rs              # Snapshot type + Sampler (moved, cross-platform)
      src/ai.rs                    # AI-workload detection (unchanged logic)
      src/net.rs                   # NEW: ports, connections, network identity
      src/util.rs                  # formatting helpers (moved)
      src/platform/mod.rs          # cfg-gated dispatch
      src/platform/macos.rs        # ioreg GPU, lsof ports/connections
      src/platform/linux.rs        # /proc, `ss`, nvidia-smi/sysfs (basic now)
    agent/       (bin, headless)   # serves GET /snapshot (JSON) on localhost
      src/main.rs                  # arg parse, sample loop, tiny_http server
      src/push.rs                  # STUB: trait + no-op impl; hub wire format TBD
    menubar/     (bin, macOS only) # tray + wry inspector; links `core` directly
      src/main.rs
      src/app.rs                   # event loop + AppState (token/provider removed)
      src/tray.rs                  # condensed header + ports/quick-action submenus
      src/inspector.rs             # view builder (token/provider views removed)
      src/inspector.html           # reorganized UI
      src/actions.rs               # NEW: caffeinate toggle, flush-DNS
```

**Crate boundaries / responsibilities:**

- `core` — pure collection. Knows nothing about tray icons, webviews, or HTTP. Produces
  a serde-`Serialize` `Snapshot`. Depends on `sysinfo` (+ `serde`). Platform specifics
  live behind `platform::` so the rest of `core` is OS-agnostic.
- `agent` — headless host process. Samples on an interval via `core`, serves the latest
  `Snapshot` as JSON over `tiny_http` on `127.0.0.1:9909` (`GET /snapshot`). Push is a
  stub. This is what a future hub would talk to, and what you can `curl` today.
- `menubar` — the macOS UI. Links `core` directly and samples in-process (works offline,
  no agent required, no double-sampling concern). macOS-only (`tao`/`wry`/`tray-icon`).

### Menubar ↔ core coupling (confirmed judgment call)

The menu bar **links `core` and samples directly**, as it does today. It is *not* a
pure HTTP client of the agent in this phase. `core` is designed so a future
"menu bar reads the agent over HTTP" is a drop-in: both paths produce the same
`Snapshot` type, whether sampled locally or deserialized from the agent's JSON.

### Cross-platform honesty

`sysinfo` already gives CPU, RAM, swap, disk usage, network throughput, processes,
load, uptime, and disk-volume capacity on **both macOS and Linux**. So the Linux agent
gets those immediately. The OS-specific gaps:

| Capability      | macOS (now)          | Linux (now)            | Linux (fast-follow) |
|-----------------|----------------------|------------------------|---------------------|
| Listening ports | `lsof -iTCP -sLISTEN`| —                      | `ss -tlnp`          |
| Connections     | `lsof -sTCP:ESTAB`   | —                      | `ss -tnp`           |
| GPU %           | `ioreg IOAccelerator`| —                      | `nvidia-smi`/sysfs  |

Linux ports/connections/GPU are explicitly a **fast-follow**, not part of this PR. The
NAS / inference box still get full CPU/RAM/disk/net/uptime on day one.

---

## 3. Content changes

### 3.1 Removals (declutter)

- Delete `services.rs` (Token Checker + Provider Keys live here).
- Remove the inspector's **Token Checker** and **Provider Keys** panels and all related
  `InspectorCommand`s (`SetProviderKey`, `ClearProviderKey`, `ValidateProvider`,
  `EstimateTokens`) and `AppState.token_result`.
- **Drop dependencies:** `tiktoken-rs`, `keyring`, `keyring-core`, `reqwest`.
- **Keep:** AI-workload detection (`ai.rs`) and the `AI{n}` tray-title hint.

### 3.2 Additions to `core`

- **`net.rs` — listening ports:** parse `lsof -nP -iTCP -sTCP:LISTEN` into
  `PortRow { port: u16, proto, process: String, pid: u32, bind: String }` where `bind`
  is normalized to `127.0.0.1` / `0.0.0.0` (all) / `[::1]` / `*`. (Generalizes the
  existing localhost-only PID collection, which currently discards the port.)
- **`net.rs` — established connections:** parse `lsof -nP -iTCP -sTCP:ESTABLISHED`,
  grouped per process → `ConnGroup { process, pid, count }`.
- **`net.rs` — network identity:** `NetIdentity { hostname, lan_ip, tailnet_ip }` via
  `System::host_name()`, `ipconfig getifaddr en0` (LAN), and `tailscale ip -4` if the
  binary exists (else `None`). All best-effort; missing pieces render as `—`.
- **Disk volumes:** `Vec<DiskVolume { name, mount, total_bytes, available_bytes }>` from
  sysinfo `Disks`. (Today only disk *I/O throughput* is shown, never *capacity*.)
- **Uptime / boot:** `uptime_secs` + `boot_epoch` from `System::uptime()` /
  `System::boot_time()`.
- **Sustained-CPU EWMA:** `Sampler` keeps `cpu_ewma: HashMap<u32, f32>`, updated each
  sample as `ewma = 0.8*prev + 0.2*current`, pruned for dead PIDs. Exposed as a
  per-process field powering an **"energy (sustained CPU)"** sort. Labeled honestly as a
  sustained-CPU proxy — **not** Activity Monitor's energy metric, which needs `sudo
  powermetrics` (deliberately avoided to keep the agent privilege-free).

**Refresh cadence:** ports/connections/identity are `lsof`/shell-heavy, so they refresh
on the existing throttled ~10s cadence (and on manual Refresh), **not** at 1 Hz. CPU/
RAM/net/disk-IO stay at 1 Hz.

### 3.3 Additions to `menubar`

- **Condensed tray header** — collapse today's ~8 disabled lines to 3 dense lines:
  ```
  CPU 23%   RAM 18.2/32 GB   GPU 14%
  Net ↓1.2M ↑0.3M   Disk ↓5M ↑0
  Load 2.1 1.8 1.5   Up 6d 4h
  ──────────────
  ▸ Listening ports (5)      ← port • proc, click → kill
  ▸ Top processes
  ▸ AI workloads (2)
  ▸ Quick actions            ← keep-awake ✓, flush DNS
  ──────────────
  Show Inspector · Refresh · Sort: CPU•/RAM
  ──────────────
  Quit
  ```
- **Inspector reorganized** (token/provider panels gone):
  ```
  toolbar: [Refresh][Hide] search ☑me ☐localhost Sort:CPU         snapshot ts
  ┌ System: host · LAN ip · Tailnet · uptime ───────────────────────────────┐
  [CPU][RAM][Swap][GPU][Net][Disk I/O]   stat tiles
  per-core CPU bars
  ┌ Processes (kill) ───────────┐ ┌ Listening ports (port·proc·pid·bind·kill)┐
  ┌ Disk volumes ──┐ ┌ Connections (proc→n) ─┐ ┌ AI workloads ─┐
  ┌ Quick actions: [Keep awake OFF][Flush DNS] ┐ ┌ Status ─┐
  ```
  "Energy (sustained CPU)" is a sort option on the Processes table, not a separate panel.
- **`actions.rs` — quick actions:**
  - **Keep awake (caffeinate):** ✅ reliable. `AppState` holds an
    `Option<std::process::Child>`; toggle on = spawn `caffeinate -dimsu`, off = kill it.
  - **Flush DNS:** ✅ ships, but requires root, so it runs
    `osascript -e 'do shell script "dscacheutil -flushcache; killall -HUP mDNSResponder"
    with administrator privileges'` — this pops the native macOS password dialog. Labeled
    so it is no surprise.
  - **Do Not Disturb:** **deferred.** No stable public API on modern macOS Focus;
    shipping it now means a fragile, version-specific hack. Revisit only if wanted.
- New `InspectorCommand`s: `ToggleCaffeinate { on: bool }`, `FlushDns`. Kill stays
  `Kill { pid }` (also used by the ports panel — killing a port = killing its owner PID).

### 3.4 `agent` binary

- `--serve` (default): sample every 1s via `core`, hold latest `Snapshot`, serve
  `GET /snapshot` → JSON on `127.0.0.1:9909` via `tiny_http`. `GET /healthz` → `ok`.
- `--once`: print one `Snapshot` as JSON to stdout and exit (handy for scripts/cron).
- `push.rs`: a `Sink` trait with a `NoopSink` default. The real `HttpSink` (hub URL +
  token, `ureq`) is **not implemented here** — it waits on the Beszel spike outcome.

---

## 4. Dependencies (net lighter than today)

| Crate            | core | agent | menubar | Change      |
|------------------|------|-------|---------|-------------|
| `sysinfo`        | ✓    |       | (via core) | keep     |
| `serde`/`serde_json` | ✓ | ✓   | ✓       | keep        |
| `tiny_http`      |      | ✓     |         | **add**     |
| `tao`,`wry`,`tray-icon` |   |   | ✓       | keep (macOS UI) |
| `tiktoken-rs`    |      |       |         | **remove**  |
| `keyring`,`keyring-core` | | |        | **remove**  |
| `reqwest`        |      |       |         | **remove**  |
| `ureq`           | — deferred with hub push — |||      |

---

## 5. Testing

Unit tests live in `core` and cover the **pure** logic (no GUI, no live system):

- `lsof` LISTEN line → `PortRow` (incl. bind normalization, IPv6, malformed lines).
- `lsof` ESTABLISHED lines → per-process `ConnGroup` counts.
- `ss -tlnp` line → `PortRow` (Linux parser, table-driven fixtures).
- `format_bytes` / `format_rate` boundaries (B↔KB↔GB, zero).
- EWMA decay math (convergence, dead-PID pruning).

GUI/IPC and the agent are verified by running, not unit tests:
- `cargo run -p menubar` — tray + inspector behavior, kill, quick actions.
- `cargo run -p agent` then `curl -s localhost:9909/snapshot | jq` — agent output shape.

---

## 6. Migration / sequencing within the PR

1. Convert to a workspace; move existing code into `crates/menubar` unchanged (compiles).
2. Extract `core` (snapshot/ai/util) out of menubar; menubar depends on it.
3. Delete `services.rs` + AI/token deps; strip inspector/app of token+provider.
4. Add `net.rs` (ports/connections/identity) + disk volumes + uptime + EWMA to `core`.
5. Add `actions.rs` + condensed tray + reorganized inspector to `menubar`.
6. Add `agent` crate (serve + once; push stub).
7. macOS verified end-to-end; Linux compiles with sysinfo-only collection.

Each step keeps the tree compiling and is a natural commit boundary for the plan.

---

## 7. Out of scope (named so it is not silently assumed done)

- The hub (scraper/store/dashboard) — separate sub-project, gated on the Beszel spike.
- Agent → hub push wire format and auth — stubbed only.
- Linux ports/connections/GPU collection — fast-follow after this PR.
- Multi-tenant isolation / RBAC for client servers — defer until real; hosts will carry
  a `group` tag so it is ready, but no tenancy logic now.
- Menu bar as a pure HTTP client of the agent — `core` is shaped for it; not built now.

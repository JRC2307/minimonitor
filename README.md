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

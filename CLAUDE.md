# MiniMonitor — Obsidian bridge

The vault's `projects/tools/minimonitor/` folder holds this project's `hub.md` + `tasks.md`
(vault-native) and a `notes/` symlink into this repo's `docs/notes/`. Edits to `notes/` here
are edits in the vault and vice-versa. For code work, launch Claude from this repo.

## Symlinked into the vault
| Vault | → repo |
|-------|--------|
| `notes/` | `docs/notes/` |

## Quick map
- `crates/core` — cross-platform collection lib (`minimonitor-core`)
- `crates/agent` — headless server, `GET /snapshot` on `127.0.0.1:9909`
- `crates/menubar` — macOS tray + wry inspector (links core)
- `crates/fleet` — `fleet` CLI/server: caguastore/fleet control-center web UI
  (`store.rs`, `serve/templates`, `serve/routes.rs`)
- `docs/superpowers/specs|plans/` — design, plan, and the Beszel build-vs-buy spike runbook
- Install/reload the login LaunchAgent: `./scripts/install.sh`

## What this is / status
Rust workspace: a cross-platform system-monitoring core lib + a macOS menu-bar
tray app (local, per-Mac via LaunchAgent) + a headless agent that serves JSON
snapshots + `fleet`, the control-center web app behind caguastore
(https://caguaserver.tail82f3c6.ts.net). **Live**: `fleet-serve` +
`minimonitor-agent` run as systemd/local services on caguaserver; menubar/agent
also run locally on Macs. Actively developed (git log 2026-07-11/12, mostly
caguastore tile additions).

## How to run locally
```bash
cargo run -p minimonitor          # menu-bar app (macOS)
cargo run -p minimonitor-agent    # headless agent, GET /snapshot on :9909
cargo run -p minimonitor-agent -- --once   # one JSON snapshot to stdout
cargo run -p fleet -- serve       # fleet web UI locally (binds per [serve].bind in fleet.toml)
cargo test --workspace
```
Config: `fleet.toml` ([serve] bind/store_path); catalog override template at
`crates/fleet/store.example.toml` (copy to `~/.config/fleet/store.toml`).

## Deploy
caguaserver, behind Tailscale HTTPS serve:
- rsync repo → `caguaserver:src/minimonitor/`
- build there: `cargo build --release -p fleet -p minimonitor-agent`
- restart services: `fleet-serve` (systemd/local unit, caguastore UI) and
  `minimonitor-agent`; copy binaries to `~/.local/bin/` per the global
  caguastore-tile recipe.
- `fleet serve` port: `:8099` (Tailscale-bound; also fronted via `tailscale serve`
  as https://caguaserver.tail82f3c6.ts.net).
- Separate observability stack (`deploy/docker-compose.yml`: Beszel :8090,
  Uptime-Kuma :3001, ntfy :8082, cloudflared) is docker-based, `deploy/README.md`
  has the full runbook.

## Command Center
project_id: 15
tasks: GET http://caguaserver.tail82f3c6.ts.net:8787/api/tasks?project_id=15

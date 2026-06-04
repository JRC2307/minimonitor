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
- `docs/superpowers/specs|plans/` — design, plan, and the Beszel build-vs-buy spike runbook
- Install/reload the login LaunchAgent: `./scripts/install.sh`

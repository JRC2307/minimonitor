# deploy/ — what runs where (since 2026-10-19)

Uptime-Kuma, Beszel and ntfy are gone (never used for real; Kuma's alerts went
to a dead ntfy.sh topic). Their compose files live in git history; the Kuma data
is archived at `caguaserver:~/archive/retired-2026-10-19/observability.tgz`.

## caguaserver

| Unit | What |
|---|---|
| `fleet-serve.service` | caguastore / control-center UI, `127.0.0.1:8099`, fronted by `tailscale serve` (https://caguaserver.triceratops-adelie.ts.net) |
| `minimonitor-agent.service` | host snapshot JSON on the tailnet IP `:9909` |
| `fleet-sync` / `fleet-collect` / `fleet-probe` timers | tailnet registry, host snapshots, MTR path health |
| `fleet-watch.timer` (every 2 min) | **the fleet's alarm** — see below |

Config: `~/.config/fleet/{fleet.toml,store.toml,service-labels.toml}`.
Build on the server with `~/.cargo/bin/cargo build --release -p fleet -p minimonitor-agent`,
copy binaries to `~/.local/bin/`, restart the unit.

## fleet-watch

`watch/fleet-watch.py` → `caguaserver:~/.local/bin/fleet-watch.py`, units in
`watch/` → `/etc/systemd/system/`. Checks Command Center, cuentas, hermeshub,
cagua-push, caguastore, Soft Serve, mini SSH, failed systemd units, disk on
`/` and `/data`, the weekly backup's age, and the mini's WhatsApp watchdog
heartbeat. A check alerts after failing 2 runs in a row (~4 min), reminds
every 6 h while still broken, and sends a recovery notice. Alerts go to
cagua-push `/api/notify` (tag `fleet`); if cagua-push is the thing that is
down, they go out through the mini's `wa-notify`.

    python3 ~/.local/bin/fleet-watch.py --status   # every check, no alerts

Add a service: one line in `HTTP` or `TCP` at the top of the script.

The mini side (`net.caguafleet.wa-watchdog`, both WhatsApp bridges +
cliproxyapi) lives in the dotfiles repo, `machines/mini/wa-watchdog.py`.

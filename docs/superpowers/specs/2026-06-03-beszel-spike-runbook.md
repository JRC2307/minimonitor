# Spike: evaluate Beszel + Uptime-Kuma before building a custom hub

**Date:** 2026-06-03
**Status:** Runbook — run on the Mac mini, ~1hr, decide build-vs-buy on evidence
**Owner:** caguabot
**Gates:** whether MiniMonitor's agent→hub *push* + a custom hub get built at all.

---

## Why this spike

The fleet goal (mini → NAS → Linux inference box → Hetzners → eventually client
servers, "see they're live by IP", one lightweight hub) is ~80% covered by existing
FOSS. Before spending weeks on a custom hub, run the two closest tools against your
**real boxes** and judge what's actually missing.

- **Beszel** — lightweight hub + small agents; Linux/macOS/Docker; push-based; history;
  alerts. Closest match to "lightweight hub + agents".
- **Uptime-Kuma** — lightweight "is-it-up" (HTTP/TCP/ping) for *any* IP, including
  servers you do not control. Nails the agentless liveness tier.

The thing they will **not** do: your AI-workload detection, per-port→process view, the
macOS menu bar, and Command Center integration. The decision is whether that gap is
worth a custom build.

---

## Setup (Docker on the mini)

> Prereq: Docker Desktop / colima running on the mini. Keep both behind the tailnet,
> not the public internet.

### Beszel hub + local agent
```bash
mkdir -p ~/Desktop/1/tools/_spike/beszel && cd ~/Desktop/1/tools/_spike/beszel
# Hub
docker run -d --name beszel-hub -p 8090:8090 \
  -v ./beszel_data:/beszel_data henrygd/beszel:latest
# Open http://localhost:8090  (or http://<mini>.tailnet:8090), create the admin user,
# then "Add system" — it shows the exact `beszel-agent` docker/run command + key.
# Run that agent command on the mini; later run it on the NAS / a Hetzner too.
```

### Uptime-Kuma (agentless liveness)
```bash
docker run -d --name uptime-kuma -p 3001:3001 \
  -v uptime-kuma:/app/data louislam/uptime-kuma:1
# Open http://localhost:3001 — add monitors: ping the mini, a Hetzner IP, the NAS web
# UI (HTTP), a router (TCP). This is the "see they're live by IP" tier.
```

---

## What to actually test (use real boxes, not localhost only)

1. Add the **mini** as a Beszel agent → confirm CPU/RAM/disk/net/temp show + history.
2. Add **one remote box** you control (a Hetzner or the NAS) as a Beszel agent over the
   tailnet → confirm push works through NAT without inbound ports.
3. Add **one box you do NOT control** (or a public URL) to Uptime-Kuma → confirm
   up/down + latency + alert fires when it is down.
4. Trigger an alert (stop a container / block a port) → confirm notification path
   (ntfy / Pushover / email) actually reaches your phone.

---

## Decision checklist

Score each. If Beszel+Kuma cover the **must-haves**, adopt them and **drop** the custom
hub + agent-push from MiniMonitor's roadmap (MiniMonitor stays the rich macOS menu-bar
tool + a `core` lib). If they miss must-haves, build the custom hub.

| Need | Beszel/Kuma? | Must-have? |
|------|--------------|------------|
| Liveness of arbitrary IPs (incl. client servers) | Kuma ✓ | yes |
| Rich host metrics (CPU/RAM/disk/net) on owned boxes | Beszel ✓ | yes |
| History / trends | Beszel ✓ | yes |
| Down / disk-full / cpu-pegged alerts to phone | both | yes |
| Push from behind NAT (no inbound) | Beszel ✓ | yes |
| Cross-platform agents (macOS + Linux) | Beszel ✓ | yes |
| **Per-port → process view** | ✗ | ? |
| **AI-workload detection (Ollama/Cursor/Claude…)** | ✗ | ? |
| **macOS menu-bar glance** | ✗ (MiniMonitor keeps this) | ? |
| **Command Center integration** | ✗ | ? |
| Lightweight to run/maintain | ✓✓ | yes |

**Outcome to record (one line, back in the refactor spec / hub.md):**
- `ADOPT` Beszel+Kuma → MiniMonitor = menu-bar + `core` only; no custom hub. **or**
- `BUILD` custom hub → because: `<specific gaps that mattered>`.

---

## Teardown
```bash
docker rm -f beszel-hub uptime-kuma
# keep ~/Desktop/1/tools/_spike/beszel/beszel_data only if you decide to adopt
```

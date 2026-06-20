# Fleet Architecture — North Star

**Date:** 2026-06-20
**Status:** Architecture / strategy doc — the picture to design against before committing to any one plane.
**Owner:** caguabot
**Supersedes scope of:** `2026-06-03-beszel-spike-runbook.md` (that spike is now Phase 1 of this larger plan).

---

## 1. What this is

caguabot is scaling toward a **general-purpose internal compute fabric**: heterogeneous
nodes — local hardware in town, rented VPS/GPU (Hetzner, cloud providers), and external
inference APIs — managed as **one fleet**. Workloads are mixed and not special-purpose:

- client apps & services hosting,
- caguabot's own batch/automation jobs (scrapers, trading bots, builds, experiments),
- dev/test/environment machines and ephemeral worker fleets,
- inference that can land on a **local GPU** or an **external provider** interchangeably,
- local compute **matched with rented compute** — burst out when local is saturated.

The original ask ("a real monitor for all my devices") is **one plane** of this. This doc
exists so the monitor — and everything after it — is built on a spine that survives as the
fabric grows, instead of being redone once orchestration and provisioning land.

### Guiding constraints
- **Still one person.** Every choice optimizes for low operational overhead. Boring,
  battle-tested, single-binary > powerful-but-heavy.
- **Buy the 80%, build the differentiated 20%.** Assemble FOSS where it fits; build custom
  only where nothing off-the-shelf gives the insight (the per-hop path prober).
- **Incremental & non-regret.** Commit now only to choices that are correct regardless of
  later decisions. Defer the big commitments (orchestrator) until the pain is real.

---

## 2. The six planes

Fleet management is layers, bottom-up. Each constrains the ones above it. **Everything
above plane 2 reads from plane 2.**

| # | Plane | Choice | Rationale |
|---|-------|--------|-----------|
| 1 | **Network fabric** | Tailscale + disciplined tags | Already in use. Makes local + rented + cloud look like one flat network. Tags become the inventory signal. |
| 2 | **Inventory / source of truth** | Fleet registry (git-tracked, SQLite-backed, fed by Tailscale API) | The shared spine. Monitor, orchestrator, provisioner all read *one* canonical "what exists." |
| 3 | **Provisioning / config** | cloud-init + thin Ansible/Nix baseline | "Make-a-node-fleet-ready" in one command: installs Tailscale + monitoring agent + (later) Nomad client, registers the node. |
| 4 | **Orchestration / scheduling** | **Nomad** (deferred) | Heterogeneous + mixed workloads (containers *and* raw batch *and* dev VMs), single binary, Tailscale-native, bursts to rented by joining a node. k8s is too heavy for a solo fleet of this shape. |
| 5 | **Observability / monitoring** | Beszel + Uptime-Kuma + custom MTR prober + Homepage | The monitor. Reads its device list from the registry (plane 2), not its own list. |
| 6 | **Access / secrets / cost** | Tailscale SSH + secrets approach (TBD) + capex-vs-opex ledger | Who can reach what; secret distribution; the accounting that says *when* to buy local vs rent. |

### Why Tailscale tags are load-bearing
Tags are the cheap, always-current signal the registry derives from. Discipline now pays
off everywhere later. Proposed tag schema (illustrative — finalized in the Phase 0 spec):

- `role:` — `host`, `worker`, `dev`, `inference`, `nas`, `router`, `hub`
- `owner:` — `self`, `client-<name>`
- `site:` — `local`, `rented`, `cloud-<provider>`
- `gpu:` — `none`, `<model>` (for inference scheduling later)

The orchestrator (Nomad) and the monitor both consume these. Get them right once.

---

## 3. The inventory registry (the non-regret spine)

The single most important decision in this whole plan is that there is **one source of
truth for "what machines exist,"** and it is explicit, not "in caguabot's head + Tailscale."

- **Backing store:** SQLite (queryable, transactional) with a git-tracked YAML/JSON export
  for human-readable diffs and review.
- **Primary feed:** the **Tailscale API** (per account / tailnet) — enumerate devices,
  pull tags, online state, addresses, OS. This auto-discovers the fleet; no manual list.
- **Multi-account:** caguabot runs devices across more than one Tailscale account/tailnet.
  The registry merges them into one view (one of the things no single off-the-shelf tool does).
- **Enrichment:** registry rows carry derived fields — role, owner/client, site
  (local|rented|cloud), capacity, GPU, and *which monitoring tier applies* (agent vs agentless).
- **Consumers:** the monitor (which targets to probe, how), later the provisioner (what
  baseline a node needs), later the orchestrator (what capacity exists where).

**Why this is non-regret:** whatever orchestrator or dashboard is chosen later, "the
canonical list of nodes and their attributes" is needed and identical. Building the monitor
on top of it (instead of a private device list) is what keeps Phase 1 from being thrown away.

---

## 4. Observability plane — design (Phase 1 detail)

Two collection problems, one presentation problem. None of the off-the-shelf pieces is
custom; the one custom piece is the highest-insight, lowest-overlap part of the list.

### Collection
- **Agent tier (owned boxes):** **Beszel** — hub + small agents. Rich host metrics
  (CPU/RAM/disk/temp/net), history, push-based through NAT (no inbound ports), cross-platform
  (macOS + Linux). For: mini, NAS, inference box, Hetzners.
- **Agentless tier (everything else):** **Uptime-Kuma** — HTTP/TCP/ping liveness for client
  servers (IP/URL only), tailnet-only services, and public Cloudflare-fronted sites. The
  "see they're live by IP" tier, including boxes caguabot does not control.

### The custom piece — per-hop path & latency prober
Neither Beszel nor Kuma answers *where* a connection degrades (own ISP vs Tailscale relay vs
Cloudflare vs the host). This is one of caguabot's top-three insight priorities and is the
natural extension of minimonitor's Rust `core`/`agent`:

- Scheduled MTR/traceroute to key targets (client sites, critical tailnet services, gateways).
- Store per-hop latency + packet loss over time.
- Surface "the path to X degraded at hop N" — the *why* behind a slowdown, not just up/down.

This keeps minimonitor focused on what only it can do, and is the differentiated 20%.

### Presentation — single pane of glass
**Homepage (gethomepage)** — lightweight dashboard with native widgets for Uptime-Kuma,
Beszel, Cloudflare, and Tailscale. One screen, every device/service green/red, per-client
rollups. (Grafana/Prometheus is the *graduation* path if/when deep time-series across the
fleet is wanted — deferred; it's operational weight a fleet this size doesn't need yet.)

### Glue
- **Tailscale discovery → registry → auto-enroll** into Kuma/Beszel. Fleet stays current
  with zero manual adds.
- **Cloudflare puller** — SSL-expiry countdowns, zone health, analytics into the single pane.

### Alerting & the "who watches the watcher" gap
- **Hub location: the Mac mini** (caguabot's choice — cheapest/simplest, already tailnet-connected).
- Alerts → phone via **ntfy / Pushover** (instant down + context).
- **Mitigation for mini-only:** a **$0 external dead-man's-switch** (healthchecks.io / Better
  Stack free) the mini pings every minute. If the mini or its ISP dies — and the on-mini hub
  with it — *that* external service alerts the phone. Closes the blind spot with no second box.

---

## 5. Build order

Each phase = its own spec → plan → PR. Do **not** build as one platform.

| Phase | Scope | When | Value |
|-------|-------|------|-------|
| **0** | Fabric tags + inventory registry | **now** | The spine. Non-regret. |
| **1** | Observability on the registry (Beszel + Kuma + MTR prober + Homepage + alerts) | **now** | The original ask. Daily value; forces the registry to be real. |
| **2** | Provisioning baseline ("make-a-node-fleet-ready" in one command) | later | "Rent 5 Hetzners" / "add a local box" → 10 min, not a day. |
| **3** | Nomad orchestration + burst-to-rented | later | Realizes "match local with rented." Commit only when manual placement hurts. |
| **4** | Cost/access polish (capex-vs-opex ledger, secrets, SSH policy) | later | Decide *when* to buy local vs rent. |

**Phase 0 + 1 bundle into the first buildable spec.** It *is* the monitor caguabot came in
asking for — built on a registry spine so it survives the later planes.

---

## 6. Deferred decisions (and why deferring is correct)

- **Orchestrator (Nomad vs k3s vs Swarm).** Recommendation is Nomad, but the *commitment* is
  deferred to Phase 3. Nothing in Phase 0–1 depends on it; the registry + tags feed whatever
  wins. Choosing now would be premature.
- **Grafana/Prometheus.** Deferred graduation path for deep time-series. Homepage covers the
  single-pane need today at a fraction of the ops cost.
- **Secrets management.** Real decision (Plane 6), deferred to Phase 4. Until then, existing
  per-project `.env` discipline holds.
- **Multi-tenant metering/billing.** Explicitly out of scope — caguabot is *not* renting out
  raw compute to others (confirmed). No multi-tenant isolation/billing plane needed.

---

## 7. Open questions (resolve when their phase arrives)

- Exact Tailscale tag schema + how many accounts/tailnets the registry must merge (Phase 0).
- Registry: standalone service vs extension of minimonitor-agent vs a thin new tool (Phase 0).
- MTR prober target list + cadence + storage format (Phase 1).
- Cloudflare: API scope (read-only analytics/SSL) and whether CF Tunnel exposes the dashboard
  to the phone off-tailnet (Phase 1).
- Local-vs-rented economics model — what counts as "saturated," burst triggers (Phase 3).

---

## 8. One-line summary

Build the **fabric tags + inventory registry** (non-regret spine) and the **observability
plane on top of it** now; defer the orchestrator (Nomad) and provisioning until the registry
is real and manual placement actually hurts. Buy the 80% (Beszel + Kuma + Homepage), build
the differentiated 20% (the Rust per-hop path prober + the multi-account registry glue).

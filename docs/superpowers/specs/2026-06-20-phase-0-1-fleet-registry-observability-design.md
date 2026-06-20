# Fleet Phase 0+1 Design Spec — Inventory Registry + Observability Stack

**Date:** 2026-06-20
**Status:** Spec (implementation-grade)
**Owner:** caguabot
**References:** `2026-06-20-fleet-architecture-north-star.md` (the 6-plane fleet architecture; this spec implements **plane 2 — inventory registry** and **plane 5 — observability**, and nothing else)

---

## 1. Scope

### In scope (Phase 0+1 only)

This spec covers exactly two of the six planes from the north-star doc:

- **Plane 2 — Inventory registry ("the non-regret spine"):** a single Rust CLI binary `fleet` that pulls every configured Tailscale tailnet, merges/dedupes the devices into one fleet view, stores it in SQLite, and exports a git-tracked snapshot. No daemon, no web service — a short-lived CLI run by cron/LaunchAgent on the Mac mini.
- **Plane 5 — Observability:** a Docker stack of battle-tested FOSS (Beszel, Uptime-Kuma, Homepage, ntfy) on the mini behind the tailnet, plus the one custom-built insight piece — the per-hop MTR path prober (`fleet probe`) — and the external dead-man's-switch (`fleet heartbeat`).

The build philosophy is fixed: **buy the 80% (FOSS), build the differentiated 20% (the MTR prober + the cross-account registry merge).**

### Explicitly out of scope

- **Plane 3 — Provisioning** (Terraform/cloud-init/box bring-up): not touched.
- **Plane 4 — Orchestration / Nomad**: deferred per north-star; not touched.
- **Plane 6 — full secrets management (sops/age/Vault):** Phase 0+1 uses the minimal split-store approach in §7; the broader secrets decision is a later plane.
- **CFDI/accounting, cost accounting (plane: cost):** not here.
- Any service exposed to the public internet. Everything is tailnet-only.
- **Cloudflare analytics (request/threat counts):** cut from Phase 0+1 (see §10, resolved risk R-9) — cf-sync is SSL-expiry + zone-health only.
- **Per-monitor Kuma state folded into the export:** cut from Phase 0+1 (resolved risk R-10); the single-pane up/down is driven by the registry's own derived `online`.
- **IPv6 path probing, ECMP multipath (paris/dublin):** v4-only, Classic strategy for Phase 1.

### Fleet reality this targets

2–3 Tailscale accounts/tailnets that must be **merged day-one**, 15–40 heterogeneous devices total (owned mini/workers/NAS, rented GPU boxes, client servers, mobile). Solo operator — every decision optimizes for **low operational overhead**.

---

## 2. Architecture overview

```
                       ┌──────────────────────── Mac mini (tailnet: js-mac-mini.tail82f3c6.ts.net) ─────────────────────────┐
                       │                                                                                                     │
  Tailscale API   ┐    │   NATIVE HOST BINARY  (LaunchAgent / cron, runs as the logged-in user)                             │
  (2-3 accounts)  ├───▶│   ┌────────────────────────────────────────────────────────────┐                                  │
  Cloudflare API  ┘    │   │  fleet  (crates/fleet, single static binary, reuses core)    │                                 │
                       │   │   sync · enroll · cf-sync · export · probe · heartbeat ·     │                                  │
                       │   │   list · show · ssh                                          │                                  │
                       │   │     state ─▶ SQLite (~/.local/state/fleet/fleet.db)          │                                  │
                       │   │     export ─▶ fleet.yaml (git) + JSON into Homepage public/  │                                  │
                       │   └───┬───────────────┬───────────────┬───────────────┬─────────┘                                  │
                       │       │ enroll        │ enroll        │ alerts        │ export files                                │
                       │       ▼ (Beszel REST) ▼ (Kuma sio)    ▼ (ntfy POST)   ▼                                            │
                       │   DOCKER STACK (docker-compose, all bound to the mini's 100.x tailnet IP only)                     │
                       │   ┌─────────┐  ┌──────────────┐  ┌──────────┐  ┌───────────────────────┐                          │
                       │   │ Beszel  │  │ Uptime-Kuma  │  │  ntfy    │  │ Homepage (single pane)│                          │
                       │   │ :8090   │  │ :3001        │  │ :8082    │  │ :3000  reads exports  │                          │
                       │   └────▲────┘  └──────────────┘  └────▲─────┘  └───────────────────────┘                          │
                       │        │ outbound WS                  │ push                                                       │
                       └────────┼──────────────────────────────┼─────────────────────────────────────────────────────────┘
                  agent boxes ──┘ (push through NAT)            └──▶ phone (ntfy app on tailnet)
                                                                        ▲
  healthchecks.io (hosted SaaS, OFF-mini) ◀── fleet heartbeat (1/min) ──┘  alerts phone if the mini/ISP dies
```

**Key boundaries:**

- **`fleet` runs natively on the host, never in Docker.** The MTR prober must see the Mac's *real* network path; a container on macOS traces the Docker-Desktop Linux VM's path instead (resolved risk R-1). All of `fleet` stays native so it shares one binary + one Keychain access path.
- **The FOSS services run in Docker.** Their network vantage is irrelevant to them.
- **State lives in SQLite + a git-tracked YAML export.** No database server, no web backend in `fleet`.
- **Tailnet is the perimeter.** Tailscale ACLs gate access; container ports bind to the mini's `100.x` IP only (defense in depth), and an install-time preflight hard-fails on a wildcard bind (resolved risk R-5).

**Tag schema** (fixed by north-star §2, four facets parsed from Tailscale's flat `tag:<facet>-<value>` strings): `role` (host|worker|dev|inference|nas|router|hub), `owner` (self|client-`<name>`), `site` (local|rented|cloud-`<provider>`), `gpu` (none|`<model>`). Attributes Tailscale can't hold live in a git-tracked `fleet-overrides.yaml`.

---

## 3. The `fleet` CLI

### 3.1 Crate layout in the workspace

**New member:** `crates/fleet`, package + single binary both named `fleet`, reusing `crates/core` by path dep. This is the workspace's first async crate; tokio/reqwest enter at the fleet level and are hoisted into `workspace.dependencies` for future reuse — **`core` is never made async** (it stays sync; `fleet` calls its tailscale-shelling helpers directly or via `spawn_blocking`).

**Root `Cargo.toml` additions** (preserving the existing `edition=2024`, `resolver=2`, version `0.2.0`, and the centralized-deps pattern):

```toml
[workspace]
resolver = "2"
members = ["crates/core", "crates/agent", "crates/menubar", "crates/fleet"]

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }     # existing
serde_json = "1"                                      # existing
sysinfo = "0.37"                                      # existing
# --- fleet-introduced, hoisted for reuse ---
clap = { version = "4.6", features = ["derive"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
rusqlite = { version = "0.38", features = ["bundled"] }
rusqlite_migration = "2.5"
figment = { version = "0.10", features = ["toml", "env"] }
serde_yaml_ng = "0.9"
chrono = { version = "0.4", features = ["serde"] }
anyhow = "1"
thiserror = "2"
```

**Dependency justifications (high confidence):**

- `rusqlite 0.38` + `bundled` + `rusqlite_migration 2.5`, **not sqlx** — compiles SQLite into the binary (no system libsqlite, no compile-time `DATABASE_URL`); sync DB is right for a short-lived CLI; migrations are `&str` tracked in `PRAGMA user_version`.
- `reqwest 0.12` with `default-features=false` + `rustls-tls` — no OpenSSL/native-tls system dep; combined with bundled rusqlite the binary has **no C system deps beyond libc** ("boring single static binary").
- `serde_yaml_ng`, **not `serde_yaml`** — `serde_yaml` is RUSTSEC-flagged unmaintained; `cargo-audit` would fail it. `serde_yaml_ng` is the maintained drop-in.
- `figment` — layered `fleet.toml` + `FLEET_*` env (secrets via env, never in the git TOML).
- `chrono` with `serde` — Tailscale `lastSeen` is RFC3339 with **non-UTC offsets**; must parse offset-aware then normalize to UTC or get hours of skew.
- `trippy-core` (probe; pinned `=0.13.x`, see §5) and `rust_socketio` (Kuma; see §3.7) are the two explicitly-unstable deps, each isolated behind a thin adapter.
- `wiremock 0.6` (dev) — async-native `MockServer`, matches reqwest+tokio, parallel-safe; not mockito.

**`crates/fleet/Cargo.toml`:**

```toml
[package]
name = "fleet"
version.workspace = true
edition.workspace = true

[[bin]]
name = "fleet"
path = "src/main.rs"

[dependencies]
minimonitor-core = { path = "../core" }
serde = { workspace = true }
serde_json = { workspace = true }
serde_yaml_ng = { workspace = true }
clap = { workspace = true }
tokio = { workspace = true }
reqwest = { workspace = true }
rusqlite = { workspace = true }
rusqlite_migration = { workspace = true }
figment = { workspace = true }
chrono = { workspace = true }
anyhow = { workspace = true }
thiserror = { workspace = true }
async-trait = "0.1"
rust_socketio = { version = "0.6", features = ["async"] }
trippy-core = "=0.13.0"          # explicitly-unstable API; exact pin, see §5 / R-7
ipnet = "2"                       # CGNAT-range / bind-address validation (R-5)

[dev-dependencies]
wiremock = "0.6"
tokio = { workspace = true }
tempfile = "3"
```

**Module layout** (`crates/fleet/src/`) — every unstable external surface behind a thin mockable boundary; all *logic* pure-testable:

```
main.rs            #[tokio::main]; parse Cli; load config; open db; dispatch
cli.rs             clap Parser/Subcommand (the 9 verbs)
config.rs          figment load -> typed Config; fleet.toml + FLEET_* env; secret resolver (R-8)
secrets.rs         resolve order: FLEET_* env -> Keychain -> hard error (R-8); redaction helper (R-6)
model.rs           Node, Tier, Tags, ProbeRun, ProbeHop, CfZone; fleet_id validation (R-3)
db/mod.rs          rusqlite open (WAL, FKs on) + migrations::to_latest
db/nodes.rs        upsert_node, list, get, sweep (epoch-scoped, R-4)
db/probe.rs        insert probe_run + probe_hop; retention sweep
db/cf.rs           upsert cf_zone
overrides.rs       load + validate (R-cross-owner) + apply fleet-overrides.yaml
tailscale.rs       OAuth + /devices client (base_url injectable for fixtures)
merge.rs           PURE merge+dedupe across tailnets (no I/O)
beszel.rs          PocketBase client; parameterized filters (R-2)
kuma/mod.rs        KumaClient trait + reconcile() (pure) + sio impl (designed, §3.7)
cloudflare.rs      read-only CF REST (zones + cert packs only; no GraphQL)
probe.rs           trippy-core adapter (unprivileged) + aggregation + evaluate() (pure)
export.rs          build Homepage export struct -> JSON/YAML (schema-locked)
alert.rs           ntfy publish + healthchecks ping (redacted on error)
doctor.rs          preflight: bind-address + secret-resolvability checks (R-5/R-8)
commands/*.rs      one module per subcommand orchestrating the above
```

Discipline: `merge`, `overrides::apply`, tag parsing, `online` derivation, `kuma::reconcile`, `probe::evaluate`, `export::build`, the CF `min(expires_on)` fold, and the delete-guard are **pure functions over in-memory structs** — the bulk of the test weight, zero network. Network clients take an injectable base URL so wiremock fixtures stand in.

### 3.2 Node data model

```rust
// model.rs
use chrono::{DateTime, Utc};
use serde::{Serialize, Deserialize};

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct Node {
    /// Stable cross-account fleet id. For machineKey/alias merges this is the
    /// merge key; for fuzzy merges it is a MINTED synthetic id (R-11) with the
    /// fuzzy string kept only as a re-link hint. Always matches ^[A-Za-z0-9._:-]+$ (R-3).
    pub fleet_id: String,
    pub hostname: String,
    pub fqdn: String,                 // Tailscale MagicDNS FQDN of the canonical row
    pub seen_in: Vec<TailnetRef>,     // every (account, device_id) this box appears as
    pub addresses: Vec<String>,       // 100.x tailnet IPs (deduped union)
    pub os: String,                   // macOS|linux|windows|iOS|android
    pub online: bool,                 // DERIVED from last_seen freshness (§3.3)
    pub last_seen: DateTime<Utc>,     // max across accounts, normalized UTC
    pub tags: Tags,
    pub tier: Tier,                   // agent | agentless
    pub dedupe_key_kind: DedupeKind,  // machinekey | alias | fuzzy (confidence)
    pub notes: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct TailnetRef { pub account: String, pub device_id: String }

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug, Default)]
pub struct Tags {
    pub role: Option<String>,   // host|worker|dev|inference|nas|router|hub
    pub owner: Option<String>,  // self | client-<name>
    pub site: Option<String>,   // local | rented | cloud-<provider>
    pub gpu: Option<String>,    // none | <model>
    pub raw: Vec<String>,       // tags that didn't match a known facet
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Tier { Agent, Agentless }

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum DedupeKind { Machinekey, Alias, Fuzzy }
```

**`fleet_id` validation (R-3):** constructed only via `FleetId::new(s) -> Result<FleetId>` which enforces `^[A-Za-z0-9._:-]+$`. Hostnames are **slugified** (lowercase, non-matching chars → `-`) before they can become part of any id or a fuzzy hint, so an attacker-controlled hostname from a merged client tailnet can never carry quotes/semicolons/backticks/leading-`-` into a PocketBase filter (§3.6) or an ssh argv (§3.5).

**Tag facet parsing** (`tags.rs`, pure): split each `tag:` value on the first `-` after a known facet prefix (`role-`, `owner-`, `site-`, `gpu-`); unmatched tags → `raw`. This is a documented convention over Tailscale's value-less flat tags. Overrides win: precedence is **override > parsed tag > default**.

**Tier derivation (precedence explicit):**
1. `fleet-overrides.yaml` sets an explicit `tier` for this `fleet_id` → use it.
2. Else derive: **agent** when `owner == self` AND `os ∈ {macOS, linux}` AND `role ∈ {host, worker, nas, inference, hub}`. **agentless** otherwise (client-owned, mobile, router, or no agent-capable role).
3. Undeterminable → **agentless** (the safe zero-install tier).

### 3.3 The `online` derivation (hard constraint)

There is **no `online` boolean in the Tailscale REST API** (`clientConnectivity` is documented-but-empty in practice, GH #11122). The registry **must derive** presence from `lastSeen` freshness:

```rust
pub fn is_online(last_seen: DateTime<Utc>, max_age: std::time::Duration) -> bool {
    Utc::now().signed_duration_since(last_seen).to_std()
        .map(|age| age < max_age).unwrap_or(false)   // unparseable/future -> offline
}
```

**Threshold:** default **15 min** (`online_threshold_secs = 900`), matching Tailscale's own admin-console heuristic; avoids false-down churn for quiet agentless/ephemeral nodes. Stored as a materialized `online` column **and** recomputed at query time (`fleet list --online` never trusts the stale flag). `lastSeen` carries non-UTC offsets (e.g. `-05:00`) — parse with `parse_from_rfc3339` then `.with_timezone(&Utc)` before any comparison.

### 3.4 Multi-tailnet merge + dedupe

The day-one differentiator. No Tailscale id is globally stable across accounts: `id` (DeviceID) is per-tailnet; `nodeId` changes on re-registration; **`machineKey` is the robust same-physical-box signal** (stable across re-auth within one node state dir). If a box joined two tailnets without wiping `/var/lib/tailscale`, both rows share a `machineKey`; if wiped, `machineKey` differs → fall back to the alias map, then fuzzy.

**`merge.rs` — pure:** `fn merge(per_account: Vec<(String, Vec<TsDevice>)>, overrides: &Overrides, prior: &PriorIds, threshold: Duration) -> Vec<Node>`

1. **Collect & filter.** Flatten `(account, TsDevice)`. Drop `isExternal == true` (shared-in devices with empty `machineKey`/`created` that would pollute inventory). Drop `authorized == false` unless `include_unauthorized`.
2. **Compute merge key** (precedence ladder):
   ```rust
   fn merge_key(d: &TsDevice, ov: &Overrides) -> (String, DedupeKind) {
       if let Some(id) = ov.alias_for(&d.account, &d.id) { return (id, DedupeKind::Alias); }
       if !d.machine_key.is_empty() { return (format!("mk:{}", d.machine_key), DedupeKind::Machinekey); }
       (format!("fz:{}|{}", slugify(&d.hostname), d.os.to_lowercase()), DedupeKind::Fuzzy)
   }
   ```
3. **Group by merge key.** Each group = one physical box across ≥1 tailnet.
4. **Canonical row** = device with the most recent `lastSeen` (freshest hostname/fqdn/os).
5. **Fold to one `Node`:** `seen_in` = all pairs; `addresses` = sorted union; `last_seen` = max; `online` = derived; tags parsed then override-layered; tier derived.
6. **Stable id minting (R-11):** machineKey/alias kinds use the merge key directly as `fleet_id`. **Fuzzy kind mints a synthetic short id** the first time a box is seen (e.g. `n-<8hex>`), storing the `fz:...` string only as a re-link hint in `node_seen`; on later syncs a fuzzy match re-links to the existing minted id instead of forking. So a renamed fuzzy box keeps its identity (enrollment/probe history doesn't orphan). The `fz:`-keyed override form is still honored as a hint but the spec recommends promoting fuzzy boxes to explicit aliases.
7. **Apply overrides** (§3.5).

**Do NOT assume hostname is unique** across the merged fleet — two unrelated `worker` boxes in different client tailnets must not auto-merge; that is why fuzzy is last-resort, flagged via `dedupe_key_kind`, and the alias map exists.

**Cross-owner alias guard (R-overrides):** `overrides.rs` rejects (load-time error) any alias whose members span different `owner` facets unless an explicit `ack_cross_owner: true` is set, and warns when an override flips `owner` from `client-*` to `self` — preventing a typo'd alias from merging a client box and a self box into one identity and mis-deriving `tier: agent` (which would trigger a Beszel agent enroll against a box you don't own).

### 3.5 `fleet.toml` and `fleet-overrides.yaml`

`fleet.toml` is git-tracked operational config; **secrets are env/Keychain-resolved by name, never inlined**. Loaded via `Toml::file(path)` merged with `Env::prefixed("FLEET_")`.

```toml
# fleet.toml  (git-tracked; secrets are env/Keychain-resolved)
db_path           = "~/.local/state/fleet/fleet.db"
export_yaml_path  = "~/Desktop/1/tools/minimonitor/fleet.yaml"        # git-tracked snapshot (stable fields only, R-export)
export_dir        = "~/Desktop/1/tools/minimonitor/deploy/homepage/fleet"  # JSON served to Homepage
online_threshold_secs = 900
ssh_user          = "caguabot"
include_unauthorized = false
include_external     = false

[[tailnets]]
name = "personal"
oauth_client_id  = "k123..."                 # non-secret id, ok to commit
oauth_secret_env = "FLEET_TS_PERSONAL_SECRET" # resolved from env/Keychain
tailnet = "-"                                # "-" = the token's own tailnet

[[tailnets]]
name = "client-acme"
oauth_client_id  = "k456..."
oauth_secret_env = "FLEET_TS_ACME_SECRET"
tailnet = "-"

[beszel]
url = "http://js-mac-mini.tail82f3c6.ts.net:8090"
user = "caguabot@example.com"                # PocketBase `users` collection (NOT _superusers)
password_env = "FLEET_BESZEL_PASSWORD"
agent_port = 45876

[kuma]
url = "http://js-mac-mini.tail82f3c6.ts.net:3001"
user = "caguabot"
password_env = "FLEET_KUMA_PASSWORD"
ntfy_notification_id = 1                      # Kuma notification id to wire monitors to

[cloudflare]
token_env = "FLEET_CF_TOKEN"                 # read-only: Zone:Read, SSL and Certificates:Read
ssl_warn_days = 14

[ntfy]
base_url = "http://js-mac-mini.tail82f3c6.ts.net:8082"
topic = "fleet"
token_env = "FLEET_NTFY_TOKEN"

[healthchecks]
ping_key_env = "FLEET_HC_PING_KEY"           # SECRET — never logged (R-6)
slug = "mini-heartbeat"

[probe]
cycles = 10
per_hop_timeout_ms = 1500
loss_threshold_pct = 20.0                     # DESTINATION-hop only (§5)
rtt_threshold_ms   = 250.0                     # DESTINATION-hop only
retention_days = 30

[[probe.target]]
name = "isp-gateway";      addr = "192.168.1.1"; path = "underlay"
[[probe.target]]
name = "cloudflare-dns";   addr = "1.1.1.1";     path = "underlay"
[[probe.target]]
name = "client-acme-prod"; addr = "203.0.113.10"; path = "underlay"
[[probe.selector]]
match_tag = "role:host";   path = "overlay"    # registry-derived: probe every matching node
```

Note: the **percent delete-guard is a hardcoded constant** (40%), not a `fleet.toml` knob (resolved risk R-12) — one fewer knob the operator will never tune.

`fleet-overrides.yaml` — git-tracked, human-edited, reviewed in PRs:

```yaml
# fleet-overrides.yaml
aliases:                            # declare (account, device_id) pairs are the SAME box
  - fleet_id: nas-01
    # ack_cross_owner: true         # required only if members span different owners (R-overrides)
    members:
      - { account: personal,    device_id: "123456" }
      - { account: client-acme, device_id: "998877" }

nodes:                              # per-node attribute layering, keyed by fleet_id (post-merge)
  nas-01:
    tags: { role: nas, owner: self, site: local, gpu: none }
    tier: agent
    notes: "Synology DS920+, ZFS pool tank, backup target"
```

**Application order** (pure, `overrides.rs`, merge step 7): (a) aliases collapse members under one `fleet_id` *before* grouping; (b) after a `Node` folds, `nodes[fleet_id]` overwrites any present `tags` facets / `tier` / `notes`; absent fields fall through to tag-derived/defaults.

### 3.6 SQLite DDL

Single file (`db_path`), `PRAGMA foreign_keys=ON`, `journal_mode=WAL` on open, schema in `PRAGMA user_version` via `rusqlite_migration`. `M001` baseline:

```sql
-- M001 ------------------------------------------------------------------

CREATE TABLE node (
    fleet_id        TEXT PRIMARY KEY,                  -- stable; minted for fuzzy (R-11)
    hostname        TEXT NOT NULL,
    fqdn            TEXT NOT NULL DEFAULT '',
    os              TEXT NOT NULL DEFAULT '',
    addresses       TEXT NOT NULL DEFAULT '[]',        -- JSON array of tailnet IPs
    online          INTEGER NOT NULL DEFAULT 0,        -- derived at sync, recomputable
    last_seen       TEXT NOT NULL,                     -- RFC3339 UTC
    tier            TEXT NOT NULL DEFAULT 'agentless', -- agent|agentless
    role TEXT, owner TEXT, site TEXT, gpu TEXT,        -- parsed tag facets
    raw_tags        TEXT NOT NULL DEFAULT '[]',        -- JSON array, unparsed tags
    dedupe_key_kind TEXT NOT NULL DEFAULT 'fuzzy',     -- machinekey|alias|fuzzy
    notes           TEXT,
    first_seen      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
CREATE INDEX idx_node_tier   ON node(tier);
CREATE INDEX idx_node_online ON node(online);

-- Provenance + the sync-epoch column the sweep depends on (R-4).
CREATE TABLE node_seen (
    account            TEXT NOT NULL,
    device_id          TEXT NOT NULL,                  -- per-tailnet DeviceID
    node_id            TEXT NOT NULL REFERENCES node(fleet_id) ON DELETE CASCADE,
    node_key           TEXT NOT NULL DEFAULT '',       -- tailscale nodeId (diagnostic)
    machine_key        TEXT NOT NULL DEFAULT '',       -- cross-account dedupe signal
    fuzzy_hint         TEXT NOT NULL DEFAULT '',        -- fz:slug|os, for re-link (R-11)
    last_seen          TEXT NOT NULL,
    last_confirmed_run INTEGER NOT NULL,               -- sync-epoch: which run last saw it (R-4)
    PRIMARY KEY (account, device_id)
);
CREATE INDEX idx_seen_node ON node_seen(node_id);
CREATE INDEX idx_seen_mk   ON node_seen(machine_key);

-- Per-run bookkeeping: which accounts succeeded in a given sync run (R-4).
CREATE TABLE sync_run (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    ts        TEXT NOT NULL,
    accounts_ok TEXT NOT NULL DEFAULT '[]'             -- JSON array of account names that succeeded
);

-- Idempotent enroll mapping -> precise decommission.
CREATE TABLE enrollment (
    fleet_id      TEXT NOT NULL REFERENCES node(fleet_id) ON DELETE CASCADE,
    system        TEXT NOT NULL,                       -- 'kuma' | 'beszel'
    remote_id     TEXT NOT NULL,                       -- Kuma monitorID / Beszel record id
    last_enrolled TEXT NOT NULL,
    PRIMARY KEY (fleet_id, system)
);

-- MTR prober.
CREATE TABLE probe_run (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          TEXT NOT NULL,                          -- RFC3339 UTC, run start
    target_name TEXT NOT NULL,
    target_addr TEXT NOT NULL,
    path_type   TEXT NOT NULL DEFAULT 'underlay',       -- underlay|overlay
    cycles      INTEGER NOT NULL,
    breached    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_run_target ON probe_run(target_name, ts);

CREATE TABLE probe_hop (
    run_id   INTEGER NOT NULL REFERENCES probe_run(id) ON DELETE CASCADE,
    ttl      INTEGER NOT NULL,
    host     TEXT,                                       -- NULL/'???' = non-responding
    sent     INTEGER NOT NULL,
    recv     INTEGER NOT NULL,
    loss_pct REAL NOT NULL,
    last_ms  REAL, avg_ms REAL, best_ms REAL, wrst_ms REAL, stdev_ms REAL,
    severity TEXT NOT NULL DEFAULT 'ok',                 -- ok|warn|breach (computed in probe)
    PRIMARY KEY (run_id, ttl)
);

-- Cloudflare cf-sync snapshot (read-only; SSL + zone health ONLY — no analytics, R-9).
CREATE TABLE cf_zone (
    zone_id         TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    status          TEXT NOT NULL,                       -- active|pending|...
    paused          INTEGER NOT NULL DEFAULT 0,
    healthy         INTEGER NOT NULL DEFAULT 0,          -- status==active && !paused
    min_cert_expiry TEXT,                                -- earliest edge-cert expiry (RFC3339)
    synced_at       TEXT NOT NULL
);
```

`node` is intentionally denormalized (facets as columns, arrays as JSON text) — queries are trivial at 15–40 rows. `node_seen` carries the real multi-account provenance plus `last_confirmed_run` so the sweep can compute a per-account, success-scoped set difference. `probe_hop` is the only unbounded table — retention runs in its **own transaction at command start** (R-13) so a breach early-return can't skip GC.

### 3.7 Subcommands

#### `fleet sync` — pull, merge, upsert, export

Cron ~5 min. Inputs: `[[tailnets]]`, overrides, OAuth secrets. Outputs: `node`/`node_seen` rows; rewritten `fleet.yaml`; non-zero exit on hard failure (so a wrapping hc.io check catches a dead sync).

1. Open a `sync_run` row. For each `[[tailnets]]`: mint an OAuth token once — `POST https://api.tailscale.com/api/v2/oauth/token` (`client_id`, `client_secret`, `grant_type=client_credentials`) → Bearer (1h TTL, no refresh needed). Use least-privilege `devices:read`.
2. `GET /api/v2/tailnet/{tailnet}/devices?fields=default` per account. Deserialize `TsDevice` (camelCase), tag with source `account`. On `429` honor `Retry-After` with backoff.
3. **Resilience:** sync is additive/upsert per account. A failed account does **not** wipe its rows — last-known rows remain and age to offline naturally. Record each succeeded account in `sync_run.accounts_ok`.
4. Filter `isExternal` / unauthorized.
5. **Merge+dedupe** (§3.4, pure) → `Vec<Node>`. **Apply overrides.**
6. **Upsert** (transaction): `INSERT … ON CONFLICT(fleet_id) DO UPDATE` for `node`; upsert `node_seen` rows for touched `(account, device_id)` and set `last_confirmed_run = this_run`. Set `first_seen` once, bump `updated_at`. Recompute `online`.
7. **Epoch-scoped sweep (R-4):** for each account in `accounts_ok`, any `node_seen(account, *)` row with `last_confirmed_run != this_run` is gone-from-that-account → delete that provenance row. A `node` becomes **stale** only when *all* its `seen_in` rows are gone across successfully-synced accounts. Stale nodes are **marked, not auto-deleted** (ephemeral nodes vanish on logout; deletion is a deliberate `fleet sync --prune`).
8. **Rewrite `fleet.yaml`** via `serde_yaml_ng` — sorted by `fleet_id`, **excluding volatile fields** (`last_seen`, `online`, `updated_at`) so the git snapshot isn't a diff-noise generator every 5 min (R-export). The served JSON (with volatile fields) is written separately by `fleet export`.

#### `fleet list` / `show` / `ssh`

**`fleet list [--tag <facet:value>] [--tier <t>] [--online] [--json]`** — pure SQLite read. `--tag role:host` filters the facet column (covers site/owner/gpu/role; the redundant `--site` flag is dropped, R-yagni). `--online` recomputes freshness. Default = compact aligned table (hostname, tier, online ●/○, site, role, owner, relative last_seen, a `~` marker on fuzzy-merged rows for operator review). `--json` emits `Vec<Node>`.

**`fleet show <node>`** (`<node>` = fleet_id | hostname | fqdn; ambiguous hostname → list candidates, exit non-zero) — full detail: facets, every `seen_in` pair, addresses, `dedupe_key_kind`, enrollment status (joined), last probe summary.

**`fleet ssh <target> [--user U] [--ts] [--all] [-- <cmd...>]`** — resolve `<target>` (name or `tag:facet:value`). **Argv built safely (R-2):** connect to a **validated `100.x` IP** parsed from `addresses` (an `IpAddr`), *not* the API-supplied fqdn/hostname, so a crafted MagicDNS name (e.g. `-oProxyCommand=…`) can't become an ssh option. Pass `user@IP` as separate argv elements, insert `--` before the host token, `exec` the system `ssh` (or `tailscale ssh` with `--ts`) inheriting the operator's keys. `--all` fans out sequentially, reporting per-node exit.

#### `fleet enroll` — idempotent reconcile

Every `tier:agent` node → a Beszel system; every `tier:agentless` node → a Kuma monitor. Run on cron after `sync`. `--dry-run` prints the plan. Inputs: desired `node` rows, `enrollment` table, `[beszel]`/`[kuma]` config+secrets. A **hardcoded 40% delete-guard** protects both systems (R-12): if more than 40% of existing monitors would be deleted in one run (a partial-fleet blip), abort loudly without deleting.

**Agent tier → Beszel (PocketBase REST):**
1. Auth: `POST {url}/api/collections/users/auth-with-password {identity, password}` → `{token}`. Use the **`users`** collection (not `_superusers` — the `/api/beszel/*` routes reject superuser tokens). Header `Authorization: <token>` (raw, no `Bearer`, matched to the pinned-version fixture).
2. **Match on the agent's self-reported identity, do NOT create-by-fleet_id (R-2/R-source-of-truth).** Under the universal-token model agents self-register their own `systems` record on first WebSocket connect. So enroll **reads existing systems and matches by the agent's reported host (the tailnet IP) / reported name**, then only **backfills/PATCHes** drift (e.g. sets a friendly `name`, links `users`). It never blind-creates a second record keyed on `fleet_id` (which would diverge from the agent-set name and produce the duplicates reconcile exists to prevent). PocketBase filters are **parameterized**, never string-interpolated (R-2): `filter="host={:h}"` with a bound params object; `fleet_id`/host values are pre-validated (§3.2).
3. Record the matched `remote_id` in `enrollment`.
4. **Decommission:** for each `enrollment(system='beszel')` whose `fleet_id` is no longer a desired agent node → `DELETE .../systems/records/{remote_id}` (subject to the guard), drop the row.
5. **Universal-token window (R-15):** the token is a *first-registration bootstrap only*. enroll **enables it on-demand** — only when it detects a desired agent node that has no matching `systems` record yet — and leaves it disabled otherwise. It is **not** re-enabled every run (which would rotate the value every 5 min and stale every agent's baked-in env token). Once registered, an agent uses its persistent per-agent credential forever; the bootstrap token in agent compose is one-time.

**Agentless tier → Uptime-Kuma (socket.io, the load-bearing surface):**

Kuma has **no official REST API** for monitor CRUD; management is its internal **socket.io v4** API (version-coupled, breaking across releases). Decision: **run Kuma 1.23.x** (not beta-grade 2.0), **pin the container tag**, and **speak native Rust socket.io** via `rust_socketio` (async, engine.io v4) behind a `KumaClient` trait. The Python `uptime-kuma-api` sidecar is the documented fallback only if churn makes native untenable.

The socket.io dance is **designed, not hand-waved (R-1)** — the protocol is push-based, so `list()` cannot be a plain request/response:

```rust
// kuma/sio.rs — the real (unstable) impl, the ONE place the wire protocol lives.
// Connect → server PUSHES "monitorList" as a broadcast on auth; we arm a oneshot
// BEFORE connecting and resolve it from the event handler.
async fn connect_and_login(cfg: &KumaCfg) -> Result<Session> {
    let (mon_tx, mon_rx) = tokio::sync::oneshot::channel();   // armed before connect
    let client = ClientBuilder::new(&cfg.url)
        .on("monitorList", move |payload, _| {                 // out-of-band broadcast
            let _ = mon_tx_once.send(parse_monitor_list(payload));  // resolve list() future
        })
        .connect().await?;
    // login is an async ACK carrying a JWT in the callback (emit_with_ack):
    let token = client.emit_with_ack("login",
        json!({ "username": cfg.user, "password": cfg.password, "token": "" }),
        Duration::from_secs(10),
        |ack, _| { /* extract ack[0].token -> set on session */ }).await?;
    let monitors = tokio::time::timeout(Duration::from_secs(10), mon_rx).await??;
    Ok(Session { client, token, monitors })   // list() returns the captured broadcast
}
```

```rust
#[async_trait::async_trait]
pub trait KumaClient {                 // the ONLY unstable boundary; faked in tests
    async fn list(&self) -> Result<Vec<RemoteMonitor>>;   // resolves against the pushed monitorList
    async fn add(&self, m: &MonitorSpec) -> Result<i64>;  // emit_with_ack "add"
    async fn edit(&self, id: i64, m: &MonitorSpec) -> Result<()>; // needs the FULL object
    async fn delete(&self, id: i64) -> Result<()>;
}
```

`MonitorSpec` is the **full, version-pinned monitor object** (the exact field schema — `type` ∈ `ping|http|port`, `hostname`/`url`/`port`, `interval`, `maxretries`, `notificationIDList: {<ntfy_id>: true}`) captured in a **recorded payload fixture** for 1.23.x; a contract test asserts `MonitorSpec` serialization matches that fixture, so a field rename fails a test, not production (R-testability). Idempotency key = `fleet_id` used as the monitor `name`, with the resolved `monitorID` also stored in `enrollment`. Because `editMonitor` needs the full object, reconcile always sends a complete `MonitorSpec`.

**`reconcile()` is pure** (tested against a fake `KumaClient`):

```rust
pub async fn reconcile(c: &impl KumaClient, want: &[MonitorSpec], guard_pct: u8) -> Result<()> {
    let have = c.list().await?;
    let by_name: HashMap<&str,&RemoteMonitor> = have.iter().map(|m|(m.name.as_str(),m)).collect();
    let to_delete: Vec<_> = have.iter().filter(|m| !want.iter().any(|w| w.name == m.name)).collect();
    if !have.is_empty() && to_delete.len()*100 / have.len() > guard_pct as usize {
        anyhow::bail!("delete guard: {} of {} monitors would be removed; aborting",
                      to_delete.len(), have.len());
    }
    for spec in want {
        match by_name.get(spec.name.as_str()) {
            Some(rm) if drifted(rm, spec) => c.edit(rm.id, spec).await?,
            Some(_) => {}                       // in sync
            None     => { c.add(spec).await?; } // never blind-add -> no dupes
        }
    }
    for m in to_delete { c.delete(m.id).await?; }
    Ok(())
}
```

#### `fleet cf-sync` — read-only Cloudflare pull

SSL-expiry + zone-health into `cf_zone`. **REST only — no GraphQL analytics (R-9).** Inputs: `[cloudflare]` token, `ssl_warn_days`. Outputs: upserted `cf_zone`; ntfy alert when any zone's `min_cert_expiry` is within `ssl_warn_days` or a zone goes unhealthy. Lives in `fleet` (not a Homepage widget) because **Homepage has no native widget for SSL-expiry/zone-health** (only `cloudflared` tunnel health).

Every CF response is an envelope — **check `success` AND `errors`** (HTTP 200 can carry `success:false`):
1. Preflight `GET /user/tokens/verify`.
2. Zones: paginate `GET /zones?per_page=50&page=N` → `{id, name, status, paused}`; `healthy := status=="active" && !paused`.
3. **SSL (load-bearing):** per zone `GET /zones/{id}/ssl/certificate_packs?status=all&per_page=50` — **`status=all` is REQUIRED** or expired/pending packs are hidden. Expiry is nested: `min(pack.certificates[].expires_on)` across all packs/certs (a pack can hold RSA+ECDSA). Store as `min_cert_expiry`.
4. Upsert; evaluate thresholds; ntfy on breach.

**Token scope (minimal, read-only):** Zone:Read, SSL and Certificates:Read, "All zones from an account." No Edit, no Analytics.

#### `fleet export` — the Homepage single-pane JSON

Writes `fleet.json` / `path-health.json` / `cf.json` into `export_dir` (the host side of the Homepage `public/` bind mount, §4) so Homepage's backend reaches them at `http://localhost:3000/fleet/*.json`. Schema is a **frozen, fixtures-tested contract** (Homepage `customapi` dotted field-paths are brittle to renames; a schema-lock test asserts the depended-on keys never silently rename):

```json
{
  "generated_at": "2026-06-20T18:00:00Z",
  "nodes": [
    { "id": "nas-01", "hostname": "nas-01", "tier": "agent",
      "online": 1, "site": "local", "role": "nas", "owner": "self",
      "last_seen": "2026-06-20T17:58:11Z" }
  ]
}
```

`online`/`healthy`/`breached`/`severity` are emitted so Homepage `remap`/`color: adaptive` work directly. **Single-pane up/down is driven by the registry's own derived `online` (R-10)** — `fleet export` does **not** read Kuma socket.io for per-node status (that would re-expose the most fragile surface for dashboard cosmetics); the native coarse `uptimekuma` rollup widget covers aggregate Kuma health. `path-health.json` carries the latest probe run's destination-hop summary with precomputed `severity`. `cf.json` carries `{zones: [{name, status, healthy, ssl_days_left}]}`.

#### `fleet probe` — the custom MTR per-hop path prober

See §5 (the prober is the differentiated build and gets its own section).

#### `fleet heartbeat` — external dead-man's-switch

See §6.

---

## 4. Observability stack (Docker on the mini)

Four services, one compose file in `tools/minimonitor/deploy/` (git-tracked; `.env` git-ignored). All pinned, all bound to the mini's `100.x` tailnet IP only. **Kuma 1.23.x, not 2.0** (boring-stack choice; gates the enroll client lib).

```yaml
# tools/minimonitor/deploy/docker-compose.yml
name: fleet-observability
services:
  beszel:
    image: henrygd/beszel:0.9.1               # PIN; re-record fixtures on bump
    restart: unless-stopped
    ports: ["${MINI_TS_IP}:8090:8090"]        # tailnet IP only, never 0.0.0.0 (templated, R-5)
    volumes: ["./beszel_data:/beszel_data"]
    healthcheck:
      test: ["CMD","wget","-qO-","http://localhost:8090/api/health"]
      interval: 30s
      timeout: 5s
      retries: 3

  uptime-kuma:
    image: louislam/uptime-kuma:1.23.16       # PIN to 1.23.x (NOT 2.0)
    restart: unless-stopped
    cap_add: ["NET_RAW"]                       # required: Kuma's ICMP 'ping' monitor needs it
    ports: ["${MINI_TS_IP}:3001:3001"]
    volumes: ["./kuma_data:/app/data"]

  homepage:
    image: ghcr.io/gethomepage/homepage:v0.10.9  # PIN; customapi behavior shifts across releases
    restart: unless-stopped
    ports: ["${MINI_TS_IP}:3000:3000"]
    volumes:
      - ./homepage/config:/app/config
      - ./homepage/fleet:/app/public/fleet:ro    # fleet export served as static files (§3.7 export)
    environment:
      HOMEPAGE_ALLOWED_HOSTS: "js-mac-mini.tail82f3c6.ts.net:3000"
      HOMEPAGE_VAR_BESZEL_USER: ${BESZEL_HOMEPAGE_USER}
      HOMEPAGE_VAR_BESZEL_PASS: ${BESZEL_HOMEPAGE_PASS}
      HOMEPAGE_VAR_CF_ACCOUNT: ${CF_ACCOUNT_ID}
      HOMEPAGE_VAR_CF_TUNNEL: ${CF_TUNNEL_ID}
      HOMEPAGE_VAR_CF_TUNNEL_TOKEN: ${CF_TUNNEL_TOKEN}
    env_file: [".env"]

  ntfy:
    image: binwiederhier/ntfy:v2.11.0          # PIN
    restart: unless-stopped
    command: serve
    ports: ["${MINI_TS_IP}:8082:80"]           # tailnet-only; phone must be on tailnet (§6)
    environment:
      NTFY_BASE_URL: "http://js-mac-mini.tail82f3c6.ts.net:8082"
      NTFY_AUTH_FILE: /var/lib/ntfy/user.db
      NTFY_AUTH_DEFAULT_ACCESS: deny-all        # private: nothing readable without a token
    volumes: ["./ntfy:/var/lib/ntfy"]
```

**Ports** (all on `${MINI_TS_IP}`): Beszel `8090`, Kuma `3001`, Homepage `3000`, ntfy `8082→80`. Beszel **agents** (on owned boxes, §4.2) listen `45876` and connect *outbound* — no inbound port on agent boxes.

**`${MINI_TS_IP}` is templated from `tailscale ip -4` at install time and install hard-fails on empty (R-5)** — never defaulting to a `0.0.0.0` wildcard. The `fleet doctor` preflight (run by `install.sh` before `compose up`) parses the compose file and fails if any published port resolves to `0.0.0.0` or a non-CGNAT (`100.64.0.0/10`) address.

### 4.1 Homepage single-pane config

Two hard constraints shape this: (1) native widgets are coarse — `uptimekuma` scrapes one status-page slug (aggregate only), `tailscale` is per-device-single-tailnet (useless for a merged fleet), and there is **no native Cloudflare SSL/zone widget**; so the **fleet-wide views ride on `customapi` panels reading the export**. (2) Homepage's *backend* makes the call, so the URL must be container-reachable — solved by serving the export as static files under `public/` (verified against the pinned v0.10.9; **if that image does not statically serve `/app/public`, the fallback is a tiny caddy sidecar** — R-14).

```yaml
# deploy/homepage/config/services.yaml
- Fleet:
    - Fleet Nodes:                       # CUSTOM: merged tailnet-wide node list (registry export)
        icon: mdi-server-network
        widget:
          type: customapi
          url: http://localhost:3000/fleet/fleet.json
          refreshInterval: 60000
          display: dynamic-list
          mappings:
            items: nodes
            name: hostname
            label: site
            limit: 60
            additionalField:
              field: online              # registry-derived (NOT Kuma, NOT TS API)
              color: adaptive
              remap:
                - { value: 1, to: up }
                - { value: 0, to: down }
                - { any: true, to: "?" }

    - Path Health:                       # CUSTOM: per-hop MTR panel (the built 20%)
        icon: mdi-transit-connection-variant
        widget:
          type: customapi
          url: http://localhost:3000/fleet/path-health.json
          refreshInterval: 300000        # matches probe cadence
          display: dynamic-list
          mappings:
            items: hops
            name: host
            label: hop
            additionalField:
              field: severity            # ok|warn|breach precomputed in fleet probe
              color: adaptive
              remap:
                - { value: ok,     to: OK }
                - { value: warn,   to: WARN }
                - { value: breach, to: BREACH }

    - Agentless (Uptime-Kuma):           # NATIVE coarse rollup
        widget: { type: uptimekuma, url: http://uptime-kuma:3001, slug: fleet }

    - Agent tier (Beszel):               # NATIVE all-systems overview
        widget:
          type: beszel
          url: http://beszel:8090
          username: "{{HOMEPAGE_VAR_BESZEL_USER}}"
          password: "{{HOMEPAGE_VAR_BESZEL_PASS}}"
          version: 2

    - SSL & Zones (Cloudflare):          # CUSTOM: SSL-expiry + zone health from cf-sync export
        widget:
          type: customapi
          url: http://localhost:3000/fleet/cf.json
          refreshInterval: 600000
          display: dynamic-list
          mappings:
            items: zones
            name: name
            label: status
            additionalField: { field: ssl_days_left, color: adaptive }
```

Secrets in `services.yaml` are `{{HOMEPAGE_VAR_*}}` references only (the file is git-tracked); substitution is from `.env` at container start. Rotation = edit `.env` + `docker compose up -d homepage`.

### 4.2 Beszel agent rollout (agent tier, push-through-NAT)

Enrollment model: **universal-token / WebSocket (push-through-NAT), NOT the SSH-key model** (the SSH model needs the hub to connect inbound to each agent — fails behind NAT). **Do NOT shell out to `install-agent.sh`** (it demands an SSH key even with a universal token); run the Docker agent directly:

```yaml
# per owned box (NOT the mini hub): beszel-agent
services:
  beszel-agent:
    image: henrygd/beszel-agent:0.9.1     # PIN to match the hub
    restart: unless-stopped
    network_mode: host                     # host metrics + outbound WS
    volumes: ["/var/run/docker.sock:/var/run/docker.sock:ro"]
    environment:
      LISTEN: 45876
      HUB_URL: http://js-mac-mini.tail82f3c6.ts.net:8090
      TOKEN: ${BESZEL_BOOTSTRAP_TOKEN}     # ONE-TIME bootstrap, not a live credential (R-15)
```

The agent connects outbound, self-registers a `systems` record on first connect, and thereafter uses its **persistent per-agent credential**. `fleet enroll` enables the universal token **only on-demand** when a not-yet-registered desired agent node is detected (R-15), so the token doesn't rotate every cycle and stale every agent's baked-in env. Auth for enroll's hub calls uses the PocketBase `users` collection (not `_superusers`); the Homepage Beszel widget separately needs a *superuser* account (a different credential).

---

## 5. The MTR path prober (`fleet probe` — the built 20%)

The one custom insight piece: scheduled per-hop latency + loss to registry-derived + pinned targets, stored in SQLite, alerting on per-hop threshold breach. The FOSS stack answers *whether* a target is down; this answers *where* a path degrades. One-shot, LaunchAgent-driven, **native on the host** (Docker on macOS would trace the VM's path — §2).

**Implementation: embed `trippy-core` (`=0.13.0` exact pin).** Not shell-out-to-mtr (Homebrew `mtr` isn't setuid → needs `sudo` per run; the setuid workaround resets on `brew upgrade` — a non-starter for unattended cron). Not `ftr` (single-probe RTT, no per-hop loss aggregation). Not hand-rolled sockets. trippy is pure-Rust and aggregates mtr-style per-hop loss% + RTT stats (min/avg/max/stddev/jitter).

**Unprivileged operation is explicit, not assumed (R-6/feasibility):** trippy's unprivileged mode is **opt-in** and incompatible with ECMP multipath. The adapter sets it explicitly:

```rust
// probe.rs — the ONE file the (unstable, =0.13.0-pinned) trippy API touches (R-7)
let tracer = trippy_core::Builder::new(target_addr)
    .privilege_mode(trippy_core::PrivilegeMode::Unprivileged)  // SOCK_DGRAM/IPPROTO_ICMP, no root
    .multipath_strategy(trippy_core::MultipathStrategy::Classic) // Unprivileged is incompatible with paris/dublin
    .protocol(trippy_core::Protocol::Icmp)
    .max_rounds(Some(cfg.cycles))
    .build()?;
```

A **startup self-check** opens the unprivileged dgram-ICMP socket and **fails loudly** if it can't, rather than silently producing empty traces (R-6). v4-only for Phase 1 (a second socket + dual-stack target logic is deferred).

**Targets** — two explicit path classes (a `100.x` Tailscale IP traces the WireGuard/DERP **overlay**, a public IP traces the internet **underlay**): pinned `[[probe.target]]` + registry-derived `[[probe.selector]]`, each tagged `path` and stored on `probe_run` so Homepage shows both planes distinctly.

**Per run, per target:** run `cycles` ICMP rounds → aggregate `Vec<HopStat>` → write one `probe_run` + N `probe_hop` rows (SQLite is sync — fine, done via `spawn_blocking`) → evaluate alert policy → ntfy on breach. **Retention runs in its own transaction at command start (R-13)** so a breach early-return never skips GC.

**Alert policy — the #1 false-positive trap, handled (resolved risk R-probe):** a *middle* hop at 100% loss with downstream hops responding is **NORMAL** (routers deprioritize ICMP TTL-exceeded). Therefore:

```rust
fn evaluate(hops: &[HopStat], loss_pct: f64, rtt_ms: f64) -> Option<Alert> {
    let dest = hops.iter().rev().find(|h| h.host.is_some())?;   // last RESPONDING hop = the target
    (dest.loss_pct > loss_pct || dest.avg_ms > rtt_ms).then(|| Alert::breach(dest))
}
```

Loss/RTT alerts fire **only on the destination hop**, never intermediates. Non-responding intermediates are stored as 100% loss (informational) but never alerted. Each hop gets a `severity` (`ok`/`warn` at 0.7× threshold/`breach`) written into `path-health.json` so Homepage just remaps a string — thresholding stays server-side. On breach, `fleet probe` POSTs to ntfy with target/hop/loss%/RTT at priority 4.

---

## 6. Alerting + dead-man's-switch

**Three publishers, one private topic (`fleet`), self-hosted ntfy on the tailnet** (ntfy.sh public topics are world-guessable). Most alerts never touch `fleet` — Beszel and Kuma publish natively:

| Source | Config | Mechanism |
|---|---|---|
| **Beszel** (CPU/down/temp) | Beszel UI (Shoutrrr) | `ntfy://:<token>@js-mac-mini.tail82f3c6.ts.net:8082/fleet` — no glue code |
| **Uptime-Kuma** (agentless up/down) | Kuma UI → Notifications → ntfy | server URL `…:8082`, topic `fleet`, access token — no glue code |
| **`fleet` CLI** (probe breach, sync/enroll/cf failures) | native binary, JSON POST to ntfy root | token from Keychain |

```rust
// alert.rs — fleet's only direct publishing
let token = secrets::resolve("FLEET_NTFY_TOKEN", "fleet-ntfy-token")?;
reqwest::Client::new().post("http://js-mac-mini.tail82f3c6.ts.net:8082/")
    .bearer_auth(token)
    .json(&serde_json::json!({
        "topic": "fleet", "title": "probe breach",
        "message": "path to client-acme-prod: dest hop loss 30% (>20%), avg 410ms",
        "priority": 4, "tags": ["warning"],
        "click": "http://js-mac-mini.tail82f3c6.ts.net:3000/"
    }))
    .send().await.map_err(redact)?    // R-6: never surface tokenized URL/headers
    .error_for_status().map_err(redact)?;
```

**Priority discipline:** 5 (bypasses DND) reserved for true outages (dead-man's-switch); 4 for probe breaches and enroll/sync failures; lower for info — so probe noise doesn't train the operator to ignore push. **Delivery caveat:** a tailnet-only ntfy means the **phone must be on the tailnet** to receive push (already true, for reaching the dashboard); off-tailnet relay (ntfy.sh upstream / UnifiedPush) is deferred.

**Dead-man's-switch — the mini-only blind spot.** The whole hub lives on the mini; if the mini/ISP dies, on-mini ntfy dies with it. So an **external** watchdog (the hosted SaaS `hc-ping.com`, **never self-hosted on the mini**) must watch it:

```rust
async fn heartbeat(ping_key: &str, slug: &str) -> anyhow::Result<()> {
    let url = format!("https://hc-ping.com/{ping_key}/{slug}?create=1"); // self-provisions
    let r = reqwest::Client::new().get(&url)
        .timeout(std::time::Duration::from_secs(10)).send().await;
    // R-6: ping_key is a path-credential — log only status + slug, NEVER the URL.
    match r { Ok(resp) => { resp.error_for_status().map_err(|e| redact_ping(e, slug))?; Ok(()) }
              Err(e)   => Err(redact_ping(e, slug)) }
}
```

`fleet heartbeat` runs **every minute** (`* * * * *`); the slug-ping form auto-provisions the check. hc.io's notification channel is pre-pointed at the phone (off-mini, out of `fleet`'s scope). **Phase 1 ships only the mini-liveness heartbeat** (the actual blind-spot closer); per-job sync/probe hc.io checks are a documented later add-on (R-yagni). Free tier = 20 checks; 1/min is well within limits. **The dead-man's-switch must not depend on Keychain** — its `ping_key` is resolvable from env so it still pages even if Keychain is locked (R-8).

---

## 7. Secrets handling

**Split by consumer (containers can't read macOS Keychain — do not bridge them):**

| Consumer | Store | Holds |
|---|---|---|
| Native `fleet` CLI | macOS Keychain via `security`, OR `FLEET_*` env | Tailscale OAuth secrets, Beszel password, CF token, ntfy token, hc.io ping-key |
| Docker stack | git-ignored `.env` (chmod 600), via compose `env_file`/`environment` | ntfy token, Beszel-for-Homepage superuser, CF tunnel ids |

**Resolution order (R-8), deterministic and loud:** `FLEET_*` env **first**, then Keychain, then **hard error**. A missing secret is a LOUD failure (non-zero exit) that still pages via the hc.io dead-man's-switch (whose ping_key is env-resolvable, not Keychain-dependent).

```rust
// secrets.rs
pub fn resolve(env_var: &str, keychain_service: &str) -> anyhow::Result<String> {
    if let Ok(v) = std::env::var(env_var) { if !v.is_empty() { return Ok(v); } }
    let out = std::process::Command::new("security")
        .args(["find-generic-password","-s",keychain_service,"-a","fleet","-w"]).output()?;
    anyhow::ensure!(out.status.success(), "secret unresolved: {env_var} / keychain:{keychain_service}");
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}
```

**Keychain headless caveat (R-8):** `security` reads the login keychain, unlocked only while the operator is logged in. The mini runs LaunchAgents as the logged-in user → reads succeed. If ever run truly headless, set `FLEET_*` env instead (the resolver already prefers env). **`fleet doctor` includes a secret-resolvability preflight** so a misconfigured secret fails at install, not at 3am.

**Redaction (R-6):** a redaction helper strips Authorization headers and tokenized URLs (CF token, ntfy token, **hc.io ping-key in the URL path**) from every error before it reaches logs/stderr/anyhow chains. Unit-tested: `heartbeat` error `Display` must exclude the ping_key.

**`.gitignore` (shipped as task 1, R-5):** the current repo `.gitignore` is only `/target` + `.DS_Store`. Add `deploy/.env`, `deploy/*_data/`, `deploy/ntfy/`, `deploy/homepage/fleet/*.json`. A committed `gitleaks`/ripgrep CI check scans tracked files for `tk_`, `Bearer`, `client_secret`. The `fleet.yaml` export is schema-frozen so no secret-bearing field can appear; `fleet.toml` is git-tracked but carries only non-secret ids/endpoints (the repo must stay private; sensitive ids can move behind env later).

**Rotation runbook** (tokens live in multiple places): ntfy token → Keychain + `.env` + Beszel UI + Kuma UI; Tailscale OAuth → rotate client secret in Keychain only (OAuth clients don't expire like 90-day PATs); CF token → Keychain + `.env`; Beszel-for-Homepage superuser → `.env` only. **sops/age explicitly NOT adopted** (net-negative key-management for a solo op; deferred to the plane-6 secrets decision).

---

## 8. Testing strategy

**TDD throughout; external APIs tested against RECORDED FIXTURES, never live calls.** The architecture deliberately makes the unstable external surfaces thin and isolated, and the *decisions* pure and exhaustively tested.

**Pure-function core (the bulk of the weight, zero I/O):** `merge` (all cases below), override application + cross-owner guard, tag parsing, `online` derivation, the epoch-scoped sweep set-difference, `kuma::reconcile` + delete-guard boundaries, `probe::evaluate`, `export::build` + schema-lock, the CF envelope `success:false` path + nested `min(expires_on)`, `fleet_id` validation/slugify, the secret resolver precedence.

**HTTP surfaces (Tailscale, Beszel REST, Cloudflare, ntfy, healthchecks):** `wiremock` `MockServer`, fixtures under `crates/fleet/tests/fixtures/*.json`, reqwest base URL injected. Recorded once against pinned versions; re-record on deliberate upgrade.

**The Kuma socket.io surface (the most fragile, can mass-delete — must not be untested, R-testability):** socket.io can't be HTTP-VCR'd, so:
- `reconcile()` logic tested against an in-memory fake `KumaClient`.
- A **`MonitorSpec` serialization contract test** asserts the full monitor object matches a recorded 1.23.x payload fixture — a field rename fails a test, not production.
- At least one **non-ignored** transport test: record raw engine.io/socket.io frames (login ack, `monitorList` broadcast, add/edit/delete acks) for pinned 1.23.x and replay against a local socket.io mock, **OR** run a live test in CI against an ephemeral Kuma 1.23.16 container.
- Delete-guard boundary tests: empty `have`, empty `want`, exactly `guard_pct`.

**Probe:** the trippy adapter is thin; `evaluate()` is pure — unit-test destination-hop-only policy, the intermediate-100%-loss-is-not-an-alert case, the fully-unreachable (all `???`) case, and that a breach run still performs retention (R-13). Never invoke a live trace.

**Security tests (R-2/R-3):** a hostname containing quotes/semicolons/backticks/leading-`-` must be slugified/rejected; the PocketBase filter call must **bind** (params object) not concatenate; `fleet ssh` argv for a `-`-prefixed fqdn must connect to a validated IP with a `--` separator, never pass the crafted name as an option.

**Version-pinning discipline (standing requirement, documented re-record triggers):** Kuma 1.23.16, Beszel 0.9.1 hub+agent, `trippy-core =0.13.0`, Homepage v0.10.9, ntfy v2.11.0, and the CF account plan (determines cert-pack shape).

---

## 9. Build order within Phase 0+1

Each numbered step is **one commit**, TDD (test first). Steps are ordered so each builds on a tested foundation; the registry spine lands before anything depends on it.

1. **Repo hygiene + scaffold.** Ship the `.gitignore` additions (`deploy/.env`, `deploy/*_data/`, `deploy/ntfy/`, `deploy/homepage/fleet/*.json`) and the gitleaks CI check. Add `crates/fleet` to the workspace, hoist deps, empty binary that parses `--version`.
2. **Config + secrets + doctor.** `config.rs` (figment), `secrets.rs` (env→Keychain→error resolver + redaction), `doctor.rs` (bind-address CGNAT check, secret-resolvability check). Tests: resolver precedence, missing-secret loud error, redaction excludes ping_key.
3. **Model + DDL + migrations.** `model.rs` (with `FleetId` validation/slugify), `db/mod.rs` migrations (M001), `db/nodes.rs` upsert/list/get. Tests: `fleet_id` rejects injection chars; migration applies; round-trip upsert.
4. **Tailscale client + merge (pure).** `tailscale.rs` (OAuth + devices, base_url injectable), `merge.rs`. Fixture-backed tests: same machineKey across two accounts (clean merge), wiped-state via alias, two colliding-hostname boxes must NOT merge, external/unauthorized filtering, offset-bearing `lastSeen` normalization, fuzzy synthetic-id minting + re-link on rename.
5. **`fleet sync`.** Wire pull→merge→overrides→upsert→epoch-scoped sweep→`fleet.yaml`. Tests: additive on account failure, epoch sweep set-difference, stale-not-deleted, volatile fields excluded from YAML.
6. **`fleet list` / `show` / `ssh`.** Pure reads + safe ssh argv. Tests: `--tag`/`--online` recompute; ssh argv for `-`-prefixed fqdn connects to validated IP with `--`.
7. **`fleet export` + schema lock.** Build `fleet.json`/`cf.json`/`path-health.json`; freeze schema test. (cf/path files empty-but-valid until their commands land.)
8. **`fleet cf-sync`.** REST zones + cert-packs (`status=all`), nested `min(expires_on)`, SSL-warn ntfy. Fixture tests: envelope `success:false`, nested min, threshold alert.
9. **`fleet probe`.** trippy adapter (unprivileged/Classic + startup self-check), aggregation, `evaluate()` (destination-hop-only), severity, retention-at-start, breach ntfy, `path-health.json`. Tests per §8 probe.
10. **`fleet heartbeat`.** hc.io slug-ping `?create=1`, env-resolvable ping_key, redacted errors. Tests: URL built with `?create=1`, non-2xx → non-zero, ping_key never in error output.
11. **Beszel enroll.** PocketBase `users` auth (raw token), parameterized filter match-on-self-reported-identity, backfill/PATCH, decommission under 40% guard, on-demand universal-token enable. Fixture tests: idempotent (no dup on re-run), guard aborts mass-delete.
12. **Kuma enroll (socket.io).** `kuma/sio.rs` connect/login-ack/await-`monitorList`/emit-with-ack; `reconcile()`; `MonitorSpec` contract fixture; delete-guard boundaries; one non-ignored transport/replay test.
13. **Docker stack + Homepage config.** `deploy/docker-compose.yml` (pinned, `${MINI_TS_IP}` templated), `services.yaml` (customapi + native), `.env.example`. Verify Homepage v0.10.9 serves `public/fleet/*.json` (else add caddy sidecar).
14. **Beszel agent rollout doc + compose.** Per-box agent compose (one-time bootstrap token), push-through-NAT verification.
15. **Install + scheduling.** Extend `scripts/install.sh`: `fleet doctor` → template `${MINI_TS_IP}` (hard-fail empty) → `docker compose up -d` → install LaunchAgents. Schedule: `heartbeat` 60s, `sync`/`enroll`/`probe` 300s (offset), `cf-sync` 900s, `export` chained after sync/probe/cf-sync. Boot order: stack up → sync → enroll → probe/cf-sync → export.

---

## 10. Resolved risks + residual open questions

### Resolved (every must-fix and should-fix from the adversarial review, fixed in the design above)

- **R-1 (must) Kuma socket.io was hand-waved.** Designed the push-based dance explicitly (§3.7): a oneshot armed *before* connect resolves `list()` against the pushed `monitorList` broadcast; login is `emit_with_ack` with the JWT from the callback; `MonitorSpec` is the full version-pinned object captured in a contract fixture. Native socket.io is the choice; the Python sidecar is the named fallback.
- **R-2 (must) Beszel enroll create-by-fleet_id raced the agent's self-registration.** Resolved: agents self-register; enroll **matches on the agent's self-reported identity and only backfills/PATCHes**, never blind-creating by `fleet_id`. PocketBase filters are **parameterized**, not interpolated.
- **R-3 / R-2 (must) injection via attacker-controlled hostnames** (PocketBase filter + ssh argv). Resolved: `FleetId` validation `^[A-Za-z0-9._:-]+$` + hostname slugify; ssh connects to a parsed `IpAddr` with `user@IP` as separate argv and a `--` separator; parameterized PocketBase filters.
- **R-4 (must) the multi-tailnet sweep had no algorithm.** Resolved: added `node_seen.last_confirmed_run` + a `sync_run.accounts_ok` epoch; sweep = per-succeeded-account set-difference of unconfirmed rows; a node goes stale only when all `seen_in` are gone across succeeded accounts; stale is marked, never auto-deleted.
- **R-5 (must) `.gitignore` didn't match the real repo; never-public not enforced.** Resolved: `.gitignore` additions + gitleaks shipped as build step 1; `${MINI_TS_IP}` templated from `tailscale ip -4` with install hard-fail on empty; `fleet doctor` rejects `0.0.0.0`/non-CGNAT binds before `compose up`.
- **R-6 (should) trippy unprivileged was overstated; credential-in-URL leakage.** Resolved: `PrivilegeMode::Unprivileged` + `MultipathStrategy::Classic` set explicitly, v4-only, with a loud startup socket self-check; redaction helper strips the hc.io ping-key and all tokens/headers from errors (unit-tested).
- **R-7 (should) trippy-core API is explicitly unstable.** Resolved: pinned `=0.13.0` exact, isolated behind the single `probe.rs` adapter, upgrades gated behind a compile+test checkpoint; vendoring is the documented fallback.
- **R-8 (should) Keychain failure = silent observability regression.** Resolved: deterministic resolver (env→Keychain→hard error), missing-secret is a loud non-zero exit, the dead-man's-switch ping_key is env-resolvable so it pages even when Keychain is locked; `fleet doctor` preflights resolvability.
- **R-9 (should, yagni) Cloudflare analytics was scope creep.** Resolved: cut. cf-sync is REST-only (zones + cert-packs); no GraphQL, no Account Analytics scope, no `requests_7d`/`threats_7d`.
- **R-10 (should, yagni) per-monitor Kuma fold re-exposed the fragile surface for cosmetics.** Resolved: single-pane up/down is driven by the registry's own derived `online`; `export` does not read Kuma socket.io. Native coarse rollup covers aggregate Kuma health.
- **R-11 (nice, feasibility) fuzzy fleet_id was unstable across rename.** Resolved: fuzzy boxes mint a synthetic stable id on first sight; the `fz:` string is a re-link hint only, so a rename re-links rather than forking; promotion to an explicit alias is recommended.
- **R-12 (nice) export had two serializers churning git.** Resolved: `fleet.yaml` excludes volatile fields (`last_seen`/`online`/`updated_at`); served JSON carries them. Delete-guard is a hardcoded 40% constant, not a `fleet.toml` knob.
- **R-13 (nice) retention sweep could be skipped on breach early-return.** Resolved: retention runs in its own transaction at command start.
- **R-14 (nice) Homepage static-serve claim was version-specific.** Resolved: verify v0.10.9 serves `/app/public/fleet/*.json`; documented caddy sidecar fallback if not.
- **R-15 (should, security) universal-token re-enabled every run staled agents + widened registration.** Resolved: token is a one-time bootstrap enabled **on-demand only** when a not-yet-registered desired agent node exists; registered agents use a persistent per-agent credential; enroll tests the "no new nodes → do not enable" branch.
- **Cross-owner alias + probe false-positives** also resolved inline (§3.4 owner guard; §5 destination-hop-only policy).
- **Misc yagni trims:** dropped `--site` flag (use `--tag site:`); per-job hc.io checks deferred to a documented add-on; `first_seen` retained (consumed by `fleet show` provenance) — every other model field/flag is read by at least one of the 9 verbs.

### Residual open questions (resolve during execution / confirm with operator)

1. **machineKey cleanliness** — do caguabot's dual-tailnet boxes share a state dir (clean merge) or were re-joined fresh (need alias entries)? Resolve by running `fleet sync` once and inspecting the fuzzy-flagged count; drives how much `fleet-overrides.yaml` aliasing is needed day-one.
2. **Beszel agent self-registered `name`** — confirm the exact value the pinned `henrygd/beszel-agent:0.9.1` sets on self-registration against the live hub, to lock enroll's match key.
3. **Kuma 1.23.x socket.io frame shapes** — record the exact login-ack/`monitorList`/add-edit-delete frames for 1.23.16 to back the contract + replay tests.
4. **Homepage `public/` static-serve** — confirm v0.10.9 serves the export files in-container (else add the caddy sidecar).
5. **Phone-on-tailnet** for ntfy push delivery — confirm, else add an upstream-relay path.
6. **Operator-tunable starting points** — probe cadence/cycles (10/5min), thresholds (20% / 250ms dest-hop), hc.io timezone (`America/Mexico_City` assumed), and the mini's `100.x` IP (filled at install from `tailscale ip -4`).
7. **Simultaneous Beszel registration at 15–40 nodes** — if shared-bootstrap-token registration causes `code=1000` disconnects, fall back to per-host fingerprint tokens (deferred unless observed).
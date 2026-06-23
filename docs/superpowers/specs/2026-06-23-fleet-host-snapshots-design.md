# Fleet Host Snapshots — Design Spec

| | |
|---|---|
| **Date** | 2026-06-23 |
| **Status** | Spec |
| **Owner** | caguabot |
| **Branch** | `fleet-phase-0-1` (worktree: `/Users/caguabot/Desktop/1/tools/minimonitor-wt-fleet`) |
| **References** | Phase 0+1 spec (`docs/superpowers/specs/` — the `fleet` crate: registry/sync, SQLite db, axum+askama `serve`, config, doctor); north-star doc (the per-port→process + AI-workload differentiator Beszel/Uptime-Kuma lack) |

This spec is the **final, complete** design for the fleet-wide host-snapshots feature. It integrates the COLLECT/STORAGE and AGENT/UI drafts into one document and resolves every must-fix and should-fix from review **inline** — the design below is the corrected design, not a list of corrections.

---

## 1. Scope

### In scope

1. **Core unblock** — add `serde::Deserialize` to `MonitorSnapshot` and every type reachable from it, so the fleet crate can deserialize the agent's `/snapshot` payload directly (no mirror struct).
2. **Agent change** — `minimonitor-agent` binds a tailnet-reachable address (was hardcoded `127.0.0.1:9909`), with a fail-closed self-guard, an optional shared bearer token, and `/healthz` unchanged.
3. **`fleet collect` verb** — hourly pull loop: iterate `tier:agent` registry nodes, `GET /snapshot` over the tailnet with bounded concurrency + per-host timeout, scrub secrets, store. Resilient: an unreachable/slow agent is skipped + recorded, never fails the run.
4. **Storage** — HYBRID SQLite shape (full-fidelity blob parent + extracted indexed `host_port` / `host_workload` child tables + a `host_collect_status` ledger), migration `M003`, retention with a latest-guard, read-derived staleness.
5. **`fleet serve` UI** — a host section on `/node/{id}`; new fleet-wide `/ports` and `/workloads` pages; matching `/api/ports` + `/api/workloads` JSON with schema-lock tests.
6. **Doctor** — validate the agent bind (static + active) and token resolvability.

### Out of scope (explicitly)

- **NOT duplicating Beszel/Kuma metrics.** We do not build time-series CPU/mem/net graphs, alerting, or per-second history. Host snapshots are an *hourly inventory of what's running* — listening-port→process map, AI-workload detection, top processes, disks, and a single current cpu/mem/net rollup. The differentiated value is the **fleet-wide port→process and AI-workload view**, which Beszel and Uptime-Kuma do not provide. Liveness/uptime stays in the existing probe/sync + Kuma integration.
- **`host_process` table / fleet-wide process page** — deferred (§5.1). The full process list lives in the blob; `/node` renders top processes from it.
- **Push from agents** — the agent's no-op `Sink` push stub (`crates/agent/src/push.rs`) stays deferred; the tailnet removes the NAT reason to push (§2).
- **Linux collection paths in core** — on Linux the snapshot deserializes but `ports`/`gpu`/`ai` come back empty/null (core shells `lsof`/`ioreg`/`route`, macOS-only). Storage tolerates this; making `/ports` meaningful on Linux is a separate core effort.
- **HTMX filters on the fleet pages** (`?node=`, `?proto=`) — full-page static tables first; filters are a cheap additive follow-on.

---

## 2. Architecture

### 2.1 Pull over the tailnet (not push)

The hub **pulls**: `fleet collect` iterates the registry (which already stores every node's tailnet `100.x` address) and does `GET http://<node-tailnet-ip>:9909/snapshot`. Rationale vs. push:

- The agent stays a **read-only server** — no inbound endpoint on the hub to secure, no write auth, no replay/queueing concerns.
- The tailnet removes the only real reason to push (NAT traversal): every owned box is directly addressable over WireGuard.
- The agent already serves exactly the payload we want (`GET /snapshot → MonitorSnapshot` JSON), so pull is *zero new agent surface* beyond the bind change.

Residual risk acknowledged (§7): the bearer authenticates the *client to the agent*; nothing authenticates the *responding host to the hub*. Integrity rests on the tailnet (WireGuard + ACLs). Acceptable for a single-owner tailnet; do not drive automated actions off a single uncorroborated snapshot.

### 2.2 Two-tier cadence

| Tier | What | Cadence | Mechanism |
|------|------|---------|-----------|
| Liveness / path | `fleet sync`, `fleet probe` | ~5 min | existing LaunchAgent/cron |
| Host snapshot | `fleet collect` | **hourly** | new LaunchAgent/cron |

Host snapshots are heavier (a full per-host HTTP fetch + parse + fan-out write) and change slowly, so hourly is the right granularity. The staleness threshold (§5) is keyed to this cadence.

### 2.3 Reuse of core `MonitorSnapshot` + agent `/snapshot`

The fleet crate path-deps `minimonitor-core`. Once core gains `Deserialize`, the collector does `serde_json::from_slice::<MonitorSnapshot>(&body)` — **zero schema duplication**. Core becomes the shared wire schema for the agent (producer) and fleet (consumer), both in one workspace. For a single-maintainer internal tool this coupling is intended, not a leak. The agent is unchanged on the serialization side; it already produces this exact type.

---

## 3. `minimonitor-agent` change

The agent is a dependency-light sync `tiny_http` binary (deps: `minimonitor-core`, `serde_json`, `tiny_http` only — no serde/anyhow/tokio). All changes preserve that: **flags/env only, no `agent.toml`, no async runtime.**

### 3.1 Bind resolution (fail closed, never wildcard)

The bind was the hardcoded literal `let addr = "127.0.0.1:9909"` (main.rs:35). It becomes a resolved value, first match wins:

1. `--bind <addr>` CLI flag — what the LaunchAgent uses (install.sh bakes `--bind <HOST_TS_IP>:9909`).
2. `MINIMONITOR_AGENT_BIND` env (non-empty).
3. Auto: `minimonitor_core::net::network_identity(host).tailnet_ip` → `<ip>:9909` (already shells `tailscale ip -4`, net.rs:147).
4. **Fail-closed fallback:** `127.0.0.1:9909` with a stderr warning. Loopback is safe; a routable guess is not.

`install.sh` bakes a static `--bind <HOST_TS_IP>:9909` into the LaunchAgent because the agent can start before `tailscaled` is up (so runtime auto-detect would return `None`). install.sh already resolves `HOST_TS_IP` via `tailscale ip -4` and hard-fails on empty (R-5); runtime auto-detect (step 3) is only the hand-run convenience default. An IP change on re-auth needs an agent restart (documented; install.sh re-resolves on reinstall).

### 3.2 Self-guard — IPv6-aware allowlist (resolves security must-fix #2)

Before binding, the agent runs the **same** validator the doctor uses, lifted into `minimonitor_core::net` as a dependency-free helper so both sides agree byte-for-byte. **Critical:** the validator must be `IpAddr`-aware (not IPv4-only), because `tiny_http::Server::http("[::]:9909")` binds dual-stack/public on most OSes — the IPv4-only `rsplit_once(':')` approach silently *accepts* `[::]:9909` via its template fall-through arm, which is exactly the public exposure the task forbids.

```rust
// crates/core/src/net.rs  (no new deps — hand-rolled range checks)
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// RFC 6598 CGNAT `100.64.0.0/10` — the Tailnet IPv4 overlay range.
pub fn is_cgnat(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

/// Tailscale's IPv6 ULA range `fd7a:115c:a1e0::/48`.
fn is_tailscale_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
}

fn is_loopback_host(host: &str) -> bool {
    host == "127.0.0.1" || host.starts_with("127.") || host == "::1" || host == "[::1]"
}

/// Validate a bind `HOST:PORT`. ACCEPTS: loopback, IPv4 CGNAT literals,
/// Tailscale-ULA IPv6 literals, and `${VAR}`/`{{ }}` templates (install-time
/// resolved → the host must NOT parse as an IP). REJECTS everything else,
/// including any literal that parses as an IP but is outside the allowed
/// ranges (so `[::]`, `0.0.0.0`, `192.168.x`, `fe80::1` all fail-closed).
pub fn validate_tailnet_bind(bind: &str) -> Result<(), String> {
    // Split off :PORT — but bracket-strip IPv6 first so the v6 colons don't fool us.
    let (host_raw, port) = match bind.rsplit_once(':') {
        Some((h, p)) => (h, p),
        None => return Err(format!("`{bind}` has no explicit host (implicit wildcard)")),
    };
    if port.is_empty() { return Err(format!("`{bind}` has no port")); }
    let host = host_raw.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() { return Err(format!("`{bind}` has an empty host (implicit wildcard)")); }
    if is_loopback_host(host) { return Ok(()); }

    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) if is_cgnat(ip) => Ok(()),
        Ok(IpAddr::V4(ip)) => Err(format!("`{bind}` host {ip} not in CGNAT 100.64.0.0/10")),
        Ok(IpAddr::V6(ip)) if is_tailscale_v6(ip) => Ok(()),
        Ok(IpAddr::V6(ip)) => Err(format!("`{bind}` host {ip} not a Tailscale ULA (fd7a:115c:a1e0::/48)")),
        // ONLY non-IP-parseable strings are treated as install-time templates.
        Err(_) if host.contains('$') || host.contains('{') => Ok(()),
        Err(_) => Err(format!("`{bind}` host `{host}` is neither a tailnet IP nor a ${{VAR}} template")),
    }
}
```

The agent refuses to start on a rejected bind (exit 2) unless `--allow-non-tailnet` (documented local-dev escape hatch); loopback is always allowed unconditionally.

**Doctor rewire:** fleet's `doctor::check_serve_bind` (currently uses `ipnet::Ipv4Net`, doctor.rs:12/22/27) becomes a thin delegate to `core::net::validate_tailnet_bind`, deduping the CGNAT constant and keeping core lean (no `ipnet` in core). The existing `check_serve_bind` tests (doctor.rs:275–308) assert only `.is_ok()/.is_err()`, so they stay GREEN. **But doctor also has private `is_cgnat` unit tests (doctor.rs:227–242) via `use super::*`** — when the local `fn is_cgnat` is removed, those break the build. Resolution: keep `pub(crate) use minimonitor_core::net::is_cgnat;` in doctor so `super::*` still resolves the name, and the two existing tests pass unchanged.

### 3.3 Security/token decision: tailnet ACL primary, shared bearer fail-safe-default

`/snapshot` exposes `ProcessRow.command` (full joined argv — may carry secrets in flags) and the complete port map. The trust model is a solo-operator single-owner tailnet.

**Decision: tailnet ACL (WireGuard + device auth + ACL reachability) is the PRIMARY control. Add a single shared bearer token as defense-in-depth, with a fail-safe default.**

- One shared token across all agents (per-node tokens are overkill for one owner).
- Token from `MINIMONITOR_AGENT_TOKEN` (env, empty/unset = unconfigured).
- **Fail-safe default (resolves security should-fix):** if the agent binds a **non-loopback** (tailnet) address **and no token is set**, it **refuses to start** (exit non-zero) unless `--allow-untokened-tailnet` is passed — mirroring the `--allow-non-tailnet` escape-hatch pattern. Loopback-bound agents stay open (trivially safe). This makes "tailnet-exposed AND unauthenticated" a deliberate, logged choice, not a silent default. Doctor treats a locally-detected tailnet-bound untokened agent as an **ERROR**, not a WARN.
- Back-compat is preserved without an open default: already-deployed agents are `127.0.0.1`-bound (loopback → allowed, and unreachable by collect anyway until reinstalled with `--bind`).

**Routing — explicit allowlist, not catch-all (resolves security should-fix).** The agent currently routes *everything-not-`/healthz`* to the snapshot — fragile for a security boundary. Replace with a normalized allowlist:

- Strip query string + trailing slash before matching.
- `GET /healthz` → `"ok"`, **unauthenticated**.
- `GET /snapshot` → enforce bearer iff a token is configured, then serve JSON.
- Everything else (any other path, any non-GET method) → **404**.

**Constant-time token compare (resolves security must-fix #1).** A static, never-rotating shared secret compared with `==` is a byte-at-a-time timing oracle — the exact attacker the token defends against. Compare with a fold-XOR over equal-length bytes, no new dep:

```rust
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }   // length is not secret
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) { diff |= x ^ y; }
    diff == 0
}

/// Pure, unit-testable auth decision. `token == None` ⇒ always authorized.
fn authorized(headers: &[tiny_http::Header], token: Option<&str>) -> bool {
    let Some(tok) = token else { return true };
    let expected = format!("Bearer {tok}");
    headers.iter().any(|h|
        h.field.equiv("Authorization") && ct_eq(h.value.as_str().as_bytes(), expected.as_bytes()))
}
```

The token is **never** logged: the bind/serve line echoes only the address; 401 bodies are a static `"unauthorized"` string; no error/log path on either side interpolates the token value (do not rely on `redact` — it only strips `Bearer\s+\S+`, not a bare token, secrets.rs:94).

### 3.4 Doctor validates the agent bind (static + active)

```rust
// static: the bind string written into the LaunchAgent ProgramArguments (or a future [agent] bind key)
pub fn check_agent_bind(bind: &str) -> anyhow::Result<()> {
    if bind == "127.0.0.1:9909" || bind.starts_with("127.") { return Ok(()); }
    minimonitor_core::net::validate_tailnet_bind(bind).map_err(|m| anyhow::anyhow!("agent.bind {m}"))
}

// active (local box only): catch a misconfigured *running* process
pub fn check_agent_live_bind() -> anyhow::Result<()> {
    use minimonitor_core::net::{listening_ports, is_cgnat};
    for row in listening_ports().into_iter().filter(|r| r.port == 9909) {
        let b = &row.bind;
        let safe = b == "127.0.0.1" || b == "[::1]"
            || b.parse::<std::net::Ipv4Addr>().map(is_cgnat).unwrap_or(false);
        if !safe { anyhow::bail!("agent live bind on :9909 is `{b}` — wildcard/public exposure"); }
    }
    Ok(())
}
```

Wire into `run_doctor` after the serve-bind check: static agent-bind check, local active check, and a token check — if `[collect].token_env` is set, `check_secret_resolvability` it (WARN if unresolvable, returns the service *name* not value); if a local tailnet-bound agent has no token, **ERROR** (§3.3). A failing static/active bind check is an ERROR. Remote nodes get the static check only (remote wildcard inference is fuzzy — out of scope).

---

## 4. `fleet collect`

New `crates/fleet/src/commands/collect.rs`, registered as `Commands::Collect` in main.rs mirroring `Probe` (load config, derive db_path, `commands::collect::run(&cfg, &db_path).await`). `async fn run` because reqwest is async; tokio is already a workspace dep.

### 4.1 Order of operations (load-bearing, mirrors probe.rs)

1. Open DB via `db::open` (which sets `PRAGMA foreign_keys=ON` — every path MUST go through it so cascades fire).
2. **Retention sweep FIRST**, own txn (R-13): `dbhost::retention_sweep(&mut conn, cc.retention_days)` — *before any HTTP*, so an early failure never skips GC.
3. Select target nodes (§4.2).
4. Resolve the optional bearer token once: `let token = cc.token_env.as_deref().map(|e| secrets::resolve(e, e)).transpose()?;`
5. Concurrency-limited, per-host-timed pulls (§4.4).
6. Additive persist (§4.5): each `Ok` → `insert_snapshot`; each `Err` → `record_collect_failure` + redacted `eprintln!`. **Never `?`-propagate a per-host error out of the loop** — `run` returns `Ok(())` even if every agent is down.

### 4.2 Node selection (resolves feasibility should-fix: no `n.stale`)

Collect exactly `tier == Tier::Agent` nodes:

```rust
db::nodes::list_filtered(&conn, &ListFilter { tier: Some(Tier::Agent), ..Default::default() })
```

`tier:agent` is already the registry's "this box runs the agent" classification — reuse it, do **not** invent a `has-agent` opt-in tag. **`Node` has no `stale` field** (verified: model.rs surfaces no `stale`; the M002 `stale` column is reset on every upsert and never loaded by `row_to_node`). Any `nodes.iter().filter(|n| !n.stale)` from the drafts/sketches **will not compile and must not appear** — select on `tier == Agent` + a parseable v4 address ONLY. Collect derives its own freshness from `host_collect_status` (§5), a different axis from sync-staleness. A sync-stale agent node still gets a pull attempt (cheap, timeout-bounded, failure recorded — not fatal).

In-loop: skip nodes with no parseable v4 (tailnet `100.x`) address using the probe.rs selector idiom (`n.addresses.iter().filter_map(|a| a.parse::<IpAddr>().ok()).find(IpAddr::is_ipv4)`). Base URL: `format!("http://{ip}:{}", cc.agent_port)` — IP, not MagicDNS (parity with probe, no DNS dependency in the pull). Agentless boxes are never contacted.

### 4.3 Connection seam (fixture-testable) — returns `Vec<u8>`, no `bytes` dep (resolves must-fix #1)

New `crates/fleet/src/agent_client.rs`. **`bytes` is not a fleet dependency** and reqwest does not re-export it at a nameable path; returning `Vec<u8>` loses nothing (the blob is stored as TEXT) and avoids adding a crate:

```rust
pub struct AgentClient { http: reqwest::Client }
impl AgentClient {
    pub fn new(per_host_timeout: std::time::Duration) -> Self {
        Self { http: reqwest::Client::builder().timeout(per_host_timeout).build().unwrap() }
    }
    /// base_url = http://100.x:9909 in prod, wiremock server.uri() in tests.
    pub async fn fetch_snapshot(&self, base_url: &str, token: Option<&str>)
        -> anyhow::Result<(Vec<u8>, MonitorSnapshot)> {
        let url = format!("{}/snapshot", base_url.trim_end_matches('/'));
        let mut req = self.http.get(&url);
        if let Some(t) = token { req = req.bearer_auth(t); }
        let resp = req.send().await.context("snapshot request failed")?;
        if !resp.status().is_success() { anyhow::bail!("HTTP {}", resp.status()); }
        let raw = resp.bytes().await.context("reading snapshot body")?.to_vec();   // Vec<u8>, no `bytes` type named
        let snap = serde_json::from_slice::<MonitorSnapshot>(&raw).context("decoding MonitorSnapshot")?;
        Ok((raw, snap))   // raw bytes stored verbatim (after scrub) + typed value for child-row extraction
    }
}
```

### 4.4 Concurrency + per-host timeout (resolves feasibility should-fix: futures-util feature)

`futures-util` is declared `default-features = false` in fleet's own Cargo.toml — `StreamExt`/`buffer_unordered` live behind the `std`/`async-await` features and resolve today only by accidental workspace feature-unification. **Make it explicit:** change fleet's dep to `futures-util = { version = "0.3.32", features = ["std"] }`.

A **single** wall-clock bound is enough (resolves YAGNI nice-to-have): the outer `tokio::time::timeout` bounds *total* time including connect, which is the real concern for a hung/dead box. Drop reqwest's builder `.timeout()` to avoid a second error arm — keep one bound, one error path:

```rust
use futures_util::stream::{self, StreamExt};
let to = std::time::Duration::from_millis(cc.per_host_timeout_ms);
let client = AgentClient::new(to);                       // reqwest client; its own timeout is the connect fallback
let results: Vec<(String, anyhow::Result<(Vec<u8>, MonitorSnapshot)>)> =
  stream::iter(targets).map(|(id, base)| { let c = &client; let tok = token.clone(); async move {
      let r = match tokio::time::timeout(to, c.fetch_snapshot(&base, tok.as_deref())).await {
          Ok(inner) => inner,
          Err(_) => Err(anyhow::anyhow!("timeout after {to:?}")),
      };
      (id, r)
  }}).buffer_unordered(cc.concurrency).collect().await;
```

A hung agent costs at most `per_host_timeout_ms` and never blocks the other in-flight pulls. `buffer_unordered` over a lazy `stream::iter` avoids eagerly materializing futures. No `/healthz` preflight — a single bounded GET already makes a dead box cheap; a preflight just doubles round-trips. Defaults: concurrency 8, timeout 10_000 ms.

### 4.5 Resilient additive persist + command-line scrub at rest (resolves security must-fix #3)

`Ok((raw, snap))` → `dbhost::insert_snapshot(&mut conn, &fleet_id, &raw, &snap, Utc::now())?`. `Err(e)` → `dbhost::record_collect_failure(&conn, &fleet_id, &secrets::redact(e).to_string())?` then `eprintln!("collect: {fleet_id} failed, prior snapshot kept (stale): {}", secrets::redact(e))`. On failure **nothing** about the prior good snapshot changes; only `host_collect_status` moves (§5).

**Secret scrubbing happens at COLLECT time, before insert — not at display time.** Command lines may carry secrets in argv (`--password=`, `?api_key=…`, `token=…`, `https://user:pass@…`). Display-time truncation does not scrub the *stored* value, and 80 chars is plenty to leak a token. So `insert_snapshot` runs a scrub over the snapshot before persisting:

- Extend `secrets::redact_str` patterns to cover `key=`, `token=`, `password=`, `secret=`, `apikey=` (case-insensitive, value-bearing) and `scheme://user:pass@host`, in addition to the existing `Bearer\s+\S+`.
- Apply `scrub_command(&str) -> String` to **every** `ProcessRow.command` and every `AiWorkload.example_command` in the parsed `MonitorSnapshot`, then re-serialize THAT scrubbed value into `snapshot_json` and into the extracted `host_workload.example_command`. (This is the one place we deliberately *do* re-serialize rather than store raw bytes — the raw bytes contain unredacted argv. The rollup/port fields are unaffected.)
- Document: the fleet SQLite DB is **sensitive at rest** — never commit/export it with command lines; no export builder emits raw command lines off-box.

This makes the redaction robust regardless of the fixture scrub, which is enforced separately by a test (§8), not a manual step.

---

## 5. Storage

### 5.1 Shape decision: HYBRID (resolves Q1 + the is_latest contradiction)

One `host_snapshot` **parent** row per (node, collect) holds the full (scrubbed) `MonitorSnapshot` JSON blob (fidelity, future-proofing, `/node` renders from it), PLUS extracted indexed **child** tables `host_port` and `host_workload` populated at insert, so the fleet-wide `/ports` and `/workloads` aggregates are plain indexed SELECTs that **never scan or json_extract blobs** — the exact constraint the feature requires.

- Rejected blob-only: `/ports` would json_extract-scan every node's blob per page load (O(snapshots) parses, no index).
- Rejected normalized-only: loses the exact `MonitorSnapshot` (new core fields silently dropped, no debug fidelity).
- `host_process` is **deferred**: high cardinality (hundreds of procs/node/hour → table explosion), and the only consumer this phase is `/node`, which renders top processes from the blob. Add a capped `host_process` as a follow-on `M00x` if a fleet-wide process page is ever wanted.

**No `is_latest` column (resolves must-fix: spec contradiction).** Both halves and all sketches are reconciled to **recompute-at-read**: latest-per-node is derived at query time via `WHERE hs.id IN (SELECT MAX(id) FROM host_snapshot GROUP BY node_id)`. `MAX(id)` (monotonic) is **tie-free** — `MAX(collected_at)` could tie if a manual + cron collect share an rfc3339 second and double-list every port, so `MAX(id)` is used **everywhere** (read helpers, retention guard). This removes the flip-prior-row write and the "exactly one `is_latest=1` per node" crash-window invariant, and matches the serve crate's recompute-at-read freshness discipline (`is_online`). The aggregate joins on `host_snapshot.id`, **never `GROUP BY node`** — a node has many ports in its latest snapshot.

### 5.2 DDL — migration `M003`

Append to `Migrations::new(vec![M::up(M001), M::up(M002), M::up(M003)])` (db/mod.rs:103). `collected_at` is the **collector's** `Utc::now().to_rfc3339()` — NOT the payload's `captured_at`, which is a human label (`"epoch N"`, snapshot.rs:91/301) and is NOT rfc3339-comparable.

```sql
CREATE TABLE host_snapshot (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id       TEXT    NOT NULL REFERENCES node(fleet_id) ON DELETE CASCADE,
    collected_at  TEXT    NOT NULL,            -- collector Utc::now().to_rfc3339()
    hostname      TEXT    NOT NULL DEFAULT '',
    tailnet_ip    TEXT,
    boot_epoch    INTEGER NOT NULL DEFAULT 0,
    uptime_secs   INTEGER NOT NULL DEFAULT 0,
    total_cpu_percent  REAL    NOT NULL DEFAULT 0,
    used_memory_bytes  INTEGER NOT NULL DEFAULT 0,
    total_memory_bytes INTEGER NOT NULL DEFAULT 0,
    gpu_percent   REAL,                        -- NULL on Linux agents
    workload_count INTEGER NOT NULL DEFAULT 0, -- UN-truncated total (set before truncate(6) in core)
    port_count    INTEGER NOT NULL DEFAULT 0,
    snapshot_json TEXT    NOT NULL             -- SCRUBBED MonitorSnapshot JSON
);
CREATE INDEX idx_hs_node ON host_snapshot(node_id, collected_at);

CREATE TABLE host_port (
    snapshot_id INTEGER NOT NULL REFERENCES host_snapshot(id) ON DELETE CASCADE,
    node_id     TEXT    NOT NULL,
    port        INTEGER NOT NULL,
    proto       TEXT    NOT NULL,              -- always 'TCP' today (lsof -iTCP -sTCP:LISTEN)
    process     TEXT    NOT NULL,
    pid         INTEGER NOT NULL,
    bind        TEXT    NOT NULL               -- raw '*','127.0.0.1','[::1]', LAN IP
);
CREATE INDEX idx_hp_snap ON host_port(snapshot_id);
CREATE INDEX idx_hp_node ON host_port(node_id);
CREATE INDEX idx_hp_port ON host_port(port);

CREATE TABLE host_workload (
    snapshot_id        INTEGER NOT NULL REFERENCES host_snapshot(id) ON DELETE CASCADE,
    node_id            TEXT    NOT NULL,
    label              TEXT    NOT NULL,
    category           TEXT    NOT NULL,
    process_count      INTEGER NOT NULL,
    total_cpu_percent  REAL    NOT NULL,
    total_memory_bytes INTEGER NOT NULL,
    example_command    TEXT    NOT NULL          -- SCRUBBED
);
CREATE INDEX idx_hw_snap ON host_workload(snapshot_id);
CREATE INDEX idx_hw_node ON host_workload(node_id);
CREATE INDEX idx_hw_cat  ON host_workload(category);

CREATE TABLE host_collect_status (
    node_id         TEXT PRIMARY KEY REFERENCES node(fleet_id) ON DELETE CASCADE,
    last_attempt_at TEXT NOT NULL,
    last_success_at TEXT,                       -- NULL until first success; NOT touched on failure
    last_error      TEXT
);
```

FK rationale: children FK `host_snapshot(id)` (not `node`), so retention deleting a parent cascade-drops its children — they never orphan onto a live node. `node_id` is denormalized on children purely so the aggregates filter/join without a 3-way join. `host_collect_status` FKs `node` (per-node state). All cascades require going through `db::open` (`PRAGMA foreign_keys=ON`).

**`host_collect_status` earns its keep (resolves YAGNI should-fix):** it is required, not redundant with `MAX(collected_at)`, because a *failure must record an attempt with no snapshot row* — preserving the invariant "a `host_snapshot` row means a real captured snapshot" (no placeholder rows). It also surfaces `last_error`, which doctor renders (a doctor line lists nodes with a non-null `last_error`), giving it a concrete consumer rather than dead weight.

### 5.3 Retention (R-13) with latest-guard

`dbhost::retention_sweep(conn: &mut Connection, retention_days: u32) -> Result<usize>`, own txn, called FIRST in `collect`. Copies probe.rs but adds the latest-guard so a long-silent node never vanishes from `/ports`:

```sql
DELETE FROM host_snapshot
 WHERE collected_at < ?1                                       -- cutoff = (Utc::now()-days).to_rfc3339()
   AND id NOT IN (SELECT MAX(id) FROM host_snapshot GROUP BY node_id);  -- keep each node's latest
```

Children cascade. Lexicographic `collected_at < cutoff` is valid because every timestamp is `Utc::now().to_rfc3339()` (`+00:00` offset, same as `probe_run.ts`) — never mix `Z`. Default `retention_days = 14` (hourly ≈ 336 rows/node/fortnight; a *separate* knob from probe's sparser 30d).

### 5.4 Insert (one txn, mirrors probe `insert_run` atomicity)

`dbhost::insert_snapshot(conn, node_id, raw: &[u8], snap: &MonitorSnapshot, now) -> Result<i64>`:

1. Scrub command lines in a working copy of `snap` (§4.5); `snapshot_json` = serialized scrubbed snapshot.
2. INSERT `host_snapshot` rollups from `snap` (total_cpu_percent, used/total_memory_bytes, gpu_percent, identity.hostname/tailnet_ip, boot_epoch, uptime_secs, `workload_count = snap.ai_snapshot.workload_count` — the **un-truncated** count, verified set before `truncate(6)` in ai.rs:104/107 — `port_count = snap.ports.len()`).
3. `let sid = tx.last_insert_rowid();`
4. For `p in &snap.ports`: INSERT `host_port`.
5. **Workloads from `snap.ai_snapshot.top_workloads`** (pre-aggregated label/category/process_count/cpu/mem + scrubbed example_command — exactly the `/workloads` columns). It's truncated to 6, but `workload_count` on the parent is the true total, so the UI shows "showing 6 of N". Re-deriving exhaustively from `processes[].ai_label` would duplicate ai.rs grouping in fleet — rejected; if exhaustive is ever needed, add a `full_workloads()` helper in core, not fleet.
6. UPSERT `host_collect_status` (success): `last_attempt_at = last_success_at = now`, `last_error = NULL`.
7. Commit.

### 5.5 Staleness ledger

`dbhost::record_collect_failure(conn, node_id, err)`:

```sql
INSERT INTO host_collect_status(node_id, last_attempt_at, last_error)
 VALUES(?1, ?2, ?3)
 ON CONFLICT(node_id) DO UPDATE SET last_attempt_at = excluded.last_attempt_at,
                                    last_error      = excluded.last_error;
```

Touches `last_attempt_at` + `last_error` ONLY — `last_success_at` is left intact, so the prior good snapshot's freshness is preserved and the UI shows "stale (last ok 4h ago)" rather than the node vanishing.

**Staleness is derived at read, never stored.** A node is STALE when `last_success_at IS NULL OR (now - last_success_at) > stale_after_hours`. The rule lives in one place — `model::is_stale(collected_at: &str, threshold: Duration) -> bool` (next to `is_online`), unparseable ⇒ stale (fail-safe). Default `stale_after_hours = 3` (= 3 missed hourly collects; config `snapshot_stale_secs`, default 10800).

---

## 6. `fleet serve` UI

All UI reads go through `db::host` read helpers and `export::build_*` builders; nullable cells use askama `{% match Option %}`, not `.unwrap()`; askama auto-escapes process/bind/command strings.

### 6.1 `/node/{id}` host section (additive, empty-state never 500)

Extend `get_node_html` / `NodePage` with an additive `Option<HostSnapshotView>`. Absence renders an explicit empty-state ("No host snapshot collected yet. Run `fleet collect`."), never 404/500. Read helper:

```rust
pub fn latest_for_node(conn: &Connection, node_id: &str) -> anyhow::Result<Option<HostSnapshotDetail>> {
    conn.query_row(
        "SELECT collected_at, total_cpu_percent, used_memory_bytes, total_memory_bytes, gpu_percent, snapshot_json
         FROM host_snapshot WHERE node_id = ?1 ORDER BY id DESC LIMIT 1",
        [node_id], /* → HostSnapshotDetail */ ).optional().context("latest_for_node")
}
```

To avoid coupling the read path to core-Deserialize, render ports/workloads from the **child tables** (`ports_for_node`, `workloads_for_node`) and cpu/mem/gpu from the rollup columns. (The blob is available for full-fidelity process rendering once core Deserialize lands, but the page does not block on it.) All formatting (bytes→human, %→fixed) in the handler. Staleness via `model::is_stale` against `host_collect_status.last_success_at`.

### 6.2 `/ports` — fleet-wide listening ports (indexed, no blob scan)

```rust
pub fn all_ports(conn: &Connection) -> anyhow::Result<Vec<FleetPortRow>> {
    let mut stmt = conn.prepare(
        "SELECT n.hostname, n.fleet_id, hp.port, hp.proto, hp.process, hp.pid, hp.bind, hs.collected_at
         FROM host_port hp
         JOIN host_snapshot hs ON hs.id = hp.snapshot_id
         JOIN node n ON n.fleet_id = hp.node_id
         WHERE hs.id IN (SELECT MAX(id) FROM host_snapshot GROUP BY node_id)   -- tie-free latest-per-node
         ORDER BY hp.port, n.hostname")?;
    /* query_map → FleetPortRow */
}
```

Route `/ports` + `/api/ports` registered in `build_router_with`, handler clones `get_paths_html`. Template `templates/ports.html` clones `paths.html` with a `{% if rows.is_empty() %}` empty-state and per-row `stale` class. Columns: node (link), port, proto, process (`<code>`), pid, bind (`<code>`, render raw — `bind` is a raw string like `"*"`/`"[::1]"`, do not parse), collected. Meta line says "TCP only" (`proto` is always `"TCP"` today). Add `/ports` to `base.html` nav.

### 6.3 `/workloads` — AI workloads fleet-wide

```rust
pub fn all_workloads(conn: &Connection) -> anyhow::Result<Vec<FleetWorkloadRow>> {
    let mut stmt = conn.prepare(
        "SELECT n.hostname, n.fleet_id, hw.label, hw.category, hw.process_count,
                hw.total_cpu_percent, hw.total_memory_bytes, hw.example_command, hs.collected_at
         FROM host_workload hw
         JOIN host_snapshot hs ON hs.id = hw.snapshot_id
         JOIN node n ON n.fleet_id = hw.node_id
         WHERE hs.id IN (SELECT MAX(id) FROM host_snapshot GROUP BY node_id)
         ORDER BY hw.total_cpu_percent DESC")?;
    /* query_map → FleetWorkloadRow */
}
```

Route/handler/template mirror `/ports`. Columns: node, label, category, procs, cpu%, mem, example_command (already scrubbed at collect time; additionally truncate to ~80 chars on this aggregate page — full scrubbed command only on `/node`). Page shows "N workloads (showing top 6)" when the parent rollup `workload_count` exceeds rendered rows. Empty-DB and Linux-empty cases render the empty-state gracefully. Add `/workloads` to nav.

### 6.4 `/api/ports` + `/api/workloads` + schema-lock

JSON shapes come ONLY from `export::build_ports_json` / `build_workloads_json` (each with an `_at` variant taking a caller-supplied `generated_at` for deterministic tests), returning `pub Serialize + Deserialize` structs with a per-row `stale: bool` computed via `model::is_stale`. Handlers clone `get_fleet`: `ro_conn → all_ports → Json(build_ports_json(...))`.

Schema-lock golden tests mirror `schema_lock_api_fleet_key_paths`:

- `/api/ports` keys: `hostname, fleet_id, port, proto, process, pid, bind, collected_at, stale`; `port` is a number, `stale` is a boolean.
- `/api/workloads` keys: `hostname, fleet_id, label, category, process_count, total_cpu_percent, total_memory_bytes, example_command, collected_at, stale`.

A field rename breaks the golden contract.

### 6.5 AppState wiring

`AppState` gains `pub snapshot_stale_threshold: Duration`. **Every** constructor must set it (`build_router`, `run_with`, and the test helper `full_router`) or the crate won't compile. Config exposes `snapshot_stale_secs` (default 10800 = 3 missed hourly collects), wired in `run_with` from `Config`.

---

## 7. Security (summary of the resolved model)

| Control | Decision |
|---------|----------|
| Confidentiality / reachability | Tailnet ACL (WireGuard + device auth) is **primary** — same trust model as existing `fleet serve` / fleet↔Kuma. |
| Read auth | Optional shared bearer; **constant-time** compare; static 401 body; token never logged. |
| Untokened tailnet agent | **Fail-safe default**: agent refuses to start (and doctor ERRORs) if tailnet-bound with no token unless `--allow-untokened-tailnet`. Loopback stays open. |
| Bind safety | IPv6-aware allowlist validator (CGNAT v4 + Tailscale ULA + loopback + non-IP `${VAR}` templates); rejects `0.0.0.0`/`[::]`/`fe80::`/LAN; agent self-guards + doctor static & active checks. |
| Routing | Explicit allowlist (`GET /healthz`, `GET /snapshot`), path normalized, everything else 404; no method bypass. |
| Secrets at rest | Command lines scrubbed at **collect time** before insert (extended `redact_str` patterns); fleet DB documented sensitive-at-rest, never exported with raw argv. |
| Residual (named, accepted) | The bearer authenticates client→agent, not agent→hub; integrity rests on the tailnet. A poisoned/MITM'd node could feed a forged snapshot. Acceptable for a single-owner tailnet; do not drive automated actions off one uncorroborated snapshot. Out of scope: snapshot signing / mutual auth. |

---

## 8. Testing (TDD, fixture-backed, no live network)

Every external surface is fixture-backed; one commit per task; RED→GREEN.

**Migration lock (RED first, expected):** bump `migration_applies_m001_all_tables` `user_version` `2 → 3` and extend the table list with `host_snapshot, host_port, host_workload, host_collect_status`. This RED is the correct M003-applied signal.

**Recorded `MonitorSnapshot` fixture:** generate once via `cargo run -p minimonitor-agent -- --once > crates/fleet/tests/fixtures/snapshot.json`, keep the full field set + null shapes. **Enforce the scrub by test, not by hand:** a test asserts every `command` / `example_command` field in the committed fixture matches only an allowlist of synthetic strings (e.g. `/usr/bin/ollama serve`) and contains no secret-shaped substrings — plus the standard secret-scan gate over committed files. The collect-time scrub (§4.5) is the real defense; this just keeps the fixture clean.

**Deserialize round-trip (T1):** `serde_json::from_str::<MonitorSnapshot>(include_str!("fixtures/snapshot.json"))` — fails to **compile** today (no Deserialize). Note in T1 that its RED is a *compile error*: generate+scrub the fixture and add the derives in the **same commit** (the gate harness needs a compiling tree), keeping the RED/GREEN narrative in the commit message. GREEN asserts a few fields round-trip (`total_memory_bytes`, `ports[0].port`, `load_average.0`, `sort_mode == SortMode::Cpu`).

**Storage tests (temp SQLite via `db::open`):** insert persists parent + N `host_port` + M `host_workload` + rollups, and `snapshot_json` is the **scrubbed** value (assert a planted secret in a command field does NOT appear in the stored blob); cascade deletes children; `retention_sweep` removes an old snapshot but KEEPS each node's latest (latest-guard); `record_collect_failure` stamps attempt/error and LEAVES `last_success_at` intact; latest-per-node aggregate returns ALL ports of a node's newest snapshot (proves the `MAX(id)` join, not `GROUP BY node` collapse).

**Collect loop (wiremock + axum-free) — additive-on-failure proof:** stand up a wiremock `MockServer` serving the recorded fixture at `GET /snapshot` (200) and one unreachable target (closed port / 500). Seed two `tier:agent` nodes pointing at the two base URLs. Run `collect`. Assert: reachable node has `host_snapshot` + child rows + `last_success_at` set; unreachable node has NO `host_snapshot` row but DOES have `host_collect_status` with `last_error` set and `last_success_at` NULL; and `run` returned `Ok(())`. Each node has its own base URL, so the two outcomes separate naturally.

**Agent tests (tiny_http, no axum):** factor the auth decision into the pure `authorized(headers, token)` + `ct_eq` and unit-test: `token=None` ⇒ always authorized; correct bearer ⇒ true; wrong-but-same-length ⇒ false; wrong-length ⇒ false; missing ⇒ false. `validate_tailnet_bind` unit tests (in core): cgnat v4 ok, Tailscale-ULA v6 ok, loopback ok, `${VAR}` template ok; and explicit RED rejections for `0.0.0.0:9909`, `[::]:9909`, `:::9909`, `[fe80::1]:9909`, `192.168.1.5:9909`, bare-port, empty-host. `resolve_bind_with(args, env_lookup, tailnet_ip)` (injected source, no live tailscale) tests all four precedence arms + the self-guard exit decision + the untokened-tailnet refusal.

**Doctor tests:** existing `check_serve_bind` (`is_ok`/`is_err`) stay GREEN after the core delegate; the two private `is_cgnat` tests resolve via the `pub(crate) use` re-export; new `check_agent_bind` accepts tailnet/loopback and rejects wildcard.

**Serve tests (axum oneshot over seeded temp SQLite, no socket):** reuse `oneshot_get`/`html_get`/`seed_db`/`full_router`. A `seed_host_snapshot(conn, node_id, collected_at, ports, workloads)` helper inserts one parent + child rows. `/ports` and `/workloads` render seeded rows; `/node/{id}` renders the host section AND an empty-state (node with no snapshot ⇒ 200 + "No host snapshot collected yet", not 500/404); staleness rendering (snapshot at `now - 4h` shows the `stale` badge, fresh shows none); empty-DB across all new pages ⇒ 200 + `<p class="empty">`; schema-lock for both APIs.

**Gates per task:** `cargo fmt`, `clippy -D warnings`, secret-scan, `cargo audit`. Token never committed (env/Keychain only).

---

## 9. Build order (numbered; each = one commit, RED→GREEN, dependency-ordered)

> Packaging note (from scope review): tasks **B1–B2** below are the self-contained **agent-bind PR** and ideally land first as their own reviewed PR (different crate, different test style, public-exposure risk); **C1–C9** are the collect/storage/UI PR that depends on it. They are listed here in one ordered sequence for completeness; split into two PRs at the C1 boundary.

**Agent-bind PR**
1. **B1 — core `is_cgnat` + IPv6-aware `validate_tailnet_bind`** (§3.2) + unit tests; rewire `doctor::check_serve_bind` to delegate, add `pub(crate) use` re-export so doctor's `is_cgnat` tests resolve.
2. **B2 — agent bind resolution + IPv6-aware self-guard + fail-closed loopback + fail-safe untokened-tailnet refusal + allowlist routing + constant-time bearer** (§3.1, §3.3) — pure `resolve_bind_with` / `authorized` / `ct_eq`, no live tailscale or socket in tests.

**Collect / storage / UI PR**
3. **C1 — core `Deserialize` unblock** (§3 ref / T1): add `Deserialize` to `MonitorSnapshot`, `ProcessRow`, `CoreUsage`, `DiskVolume`, `SortMode`, `PortRow`, `ConnGroup`, `NetIdentity`, `AiSnapshot`, `AiWorkload`; generate+scrub fixture; round-trip test (compile-RED → GREEN in same commit).
4. **C2 — extend `secrets::redact_str` scrub patterns + `scrub_command`** (§4.5) + unit tests over planted secrets.
5. **C3 — `M003` migration + `db/host.rs` write helpers** (`insert_snapshot` with scrub, `retention_sweep` with latest-guard, `record_collect_failure`) + storage tests; migration-lock bump.
6. **C4 — config `[collect]` section** (`agent_port` 9909, `concurrency` 8, `per_host_timeout_ms` 10_000, `retention_days` 14, `stale_after_hours` 3, `token_env`) + `snapshot_stale_secs` for serve; figment `FLEET_COLLECT__*` env.
7. **C5 — `agent_client.rs` `AgentClient`** returning `(Vec<u8>, MonitorSnapshot)` (§4.3); wiremock test (200 → parsed; 500 → Err; bearer sent when token Some).
8. **C6 — `commands/collect.rs` run loop** + `Commands::Collect` (§4); set fleet's `futures-util` to `features=["std"]`; two-target additive-on-failure test.
9. **C7 — `db/host.rs` read helpers** (`latest_for_node`, `all_ports`, `all_workloads`, `ports_for_node`, `workloads_for_node`) + `model::is_stale` + `AppState.snapshot_stale_threshold` wired through all three constructors.
10. **C8 — `/node` host section, `/ports`, `/workloads` pages** + nav links + empty-state + staleness rendering (oneshot tests).
11. **C9 — `/api/ports` + `/api/workloads` export builders + schema-lock tests; doctor `check_agent_bind` + active `:9909` check + token-resolvability/untokened-tailnet ERROR** (§3.4).

**Follow-ons (named, not built):** `install.sh` agent LaunchAgent with `--bind <HOST_TS_IP>:9909` + hourly `collect` cron (vs ~5-min sync/probe); HTMX filters on `/ports`/`/workloads`; capped `host_process` table + fleet-wide process page; Linux collection paths in core; snapshot signing / mutual auth.

---

## 10. Resolved risks + residual open questions

### Resolved (decided inline above)

| # | Risk / question | Resolution |
|---|-----------------|------------|
| R1 | Reuse `MonitorSnapshot` vs mirror struct | Add `Deserialize` to core; reuse directly. Producer/consumer in one workspace — coupling intended. |
| R2 | Undeclared `bytes` dep | `fetch_snapshot` returns `Vec<u8>`; no `bytes` crate. |
| R3 | `futures-util` feature reliance | Declare `features=["std"]` explicitly. |
| R4 | `Node.stale` doesn't exist | Select on `tier==Agent` + parseable v4 only; all `!n.stale` purged. |
| R5 | `is_latest` column vs recompute | No column; `MAX(id) GROUP BY node_id` everywhere (tie-free). |
| R6 | Storage shape | HYBRID; `/ports`/`/workloads` are indexed child-table SELECTs, zero blob scan. |
| R7 | Latest-per-node tie hazard | `MAX(id)` (monotonic), not `MAX(collected_at)`. |
| R8 | doctor `is_cgnat` tests break on lift | `pub(crate) use` re-export keeps `super::*` resolving. |
| R9 | Token timing oracle | Constant-time `ct_eq`. |
| R10 | IPv6 bind bypass (`[::]`) | `IpAddr`-aware allowlist; template arm only for non-IP-parseable hosts. |
| R11 | Secrets-in-argv at rest | Scrub at collect time before insert; fixture scrub enforced by test. |
| R12 | Open-by-default token | Fail-safe: tailnet-bound + untokened agent refuses to start; doctor ERRORs. |
| R13 | Catch-all routing | Explicit allowlist, path-normalized, 404 otherwise. |
| R14 | Retention deletes only/last snapshot | Latest-guard (`id NOT IN MAX(id) per node`). |
| R15 | `host_collect_status` redundancy | Required for failure-without-row invariant; `last_error` consumed by doctor. |
| R16 | Linux agents empty | Tolerated (nullable gpu, zero port rows); empty-states render. |
| R17 | `captured_at` mis-used as sort key | Use collector `collected_at` (rfc3339); `captured_at` is a label. |

### Residual open questions (flag for review, not blocking)

1. **PR split** — recommended two PRs (agent-bind first, then collect/storage/UI) per the project's "one spec + plan + PR each" rule. Confirm the orchestrator wants them split vs one feature branch.
2. **Token scope** — one shared agent token (assumes single-owner tailnet holds). Per-node tokens deferred.
3. **`example_command` truncation length** on aggregate pages (~80 chars) — confirm acceptable vs name-only.
4. **Stale threshold default** (3h = 3 missed hourly collects) — align with whatever the cadence/cron follow-on settles.
5. **Snapshot integrity** — the pull is confidential+ACL-gated but not host-authenticated; named as residual, no mitigation this phase.
6. **`fd7a:115c:a1e0::/48` assumption** — Tailscale's current ULA prefix; if a tailnet uses a custom prefix the v6 allowlist needs adjustment (v4 CGNAT path is unaffected).

---

Spec complete. The design above is implementation-grade and grounded in the verified worktree state (`crates/core` has zero `Deserialize`; `fleet/Cargo.toml` has no `bytes` and `futures-util` is `default-features=false`; `Node` has no `stale` field; migration lock asserts `==2`; agent is hardcoded `127.0.0.1:9909` with a catch-all route; `workload_count` is set before `truncate(6)`; `captured_at` is a label not rfc3339). Every must-fix and should-fix from the feasibility, security, and scope-YAGNI reviews is resolved inline in the design (mapped in the §10 table R1–R17).
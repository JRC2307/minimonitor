# Fleet Host Snapshots — TDD Implementation Plan

Spec: `docs/superpowers/specs/2026-06-23-fleet-host-snapshots.md` (final design). This plan turns §9 (build order B1–B2, C1–C9) into bite-sized, test-first, one-commit tasks.

## Preamble

**Branch / worktree.** Continue on the existing `fleet-phase-0-1` branch in its worktree:
`/Users/caguabot/Desktop/1/tools/minimonitor-wt-fleet`. Do **not** branch again; all tasks land here. Per §9 packaging note, B1–B2 are the self-contained **agent-bind PR** (cut a PR at the C1 boundary), C1–C9 are the **collect/storage/UI PR**. Land them as two PRs off this branch (or two stacked branches if the orchestrator prefers — confirm split per §10 residual Q1), but author the commits in the B1→C9 order below.

**How to run tests.**
- Whole workspace: `cargo test --workspace` (run from the worktree root).
- Per crate (faster inner loop): `cargo test -p minimonitor-core`, `-p minimonitor-agent`, `-p minimonitor-fleet`.
- Single test: `cargo test -p minimonitor-fleet <test_name> -- --nocapture`.
- **Gates per task (all must pass before the commit):** `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, the repo secret-scan test (`cargo test -p minimonitor-fleet secret_scan`), and `cargo audit`. Never commit a token (env/Keychain only).

**Which integrations use which fixtures.**
- **`crates/fleet/tests/fixtures/snapshot.json`** — the recorded `MonitorSnapshot` (created in C1 via `cargo run -p minimonitor-agent -- --once`, then scrubbed). Consumed by: the C1 Deserialize round-trip test, the C5 `AgentClient` wiremock body, the C6 collect-loop wiremock body, and the C2/§8 fixture-scrub gate test.
- **wiremock `MockServer`** (dev-dep, `crates/fleet`) — serves `snapshot.json` at `GET /snapshot` for C5 and C6. No live network in any test.
- **temp SQLite via `db::open`** — every storage/serve test opens a tempfile DB through `db::open` so `PRAGMA foreign_keys=ON` fires and cascades work. Serve tests reuse the existing `oneshot_get`/`html_get`/`seed_db`/`full_router` helpers (axum oneshot, no socket).
- **injected sources** — agent bind/auth tests (B2) inject `resolve_bind_with(args, env_lookup, tailnet_ip)` and `authorized(headers, token)` — no live `tailscale`, no real socket.

Every task is RED→GREEN→commit. Where the RED is a *compile* error (C1), say so in the commit message and land the fixture + derive in the same commit.

---

## Agent-bind PR (B1–B2)

### B1 — core `is_cgnat` + IPv6-aware `validate_tailnet_bind`; doctor delegates

**Goal.** Move the bind-allowlist logic into `minimonitor_core::net` as a dependency-free, `IpAddr`-aware validator (§3.2), and rewire fleet's `doctor::check_serve_bind` to delegate to it — keeping `core` lean (no `ipnet`) and giving the agent and doctor one byte-for-byte-identical validator.

**Tests first** (`crates/core/src/net.rs`, `#[cfg(test)] mod`):
- `is_cgnat`: `true` for `100.64.0.1`, `100.127.255.255`; `false` for `100.63.x`, `100.128.x`, `192.168.1.5`, `10.0.0.1`.
- `validate_tailnet_bind` ACCEPTS: `127.0.0.1:9909`, `127.1.2.3:9909`, `[::1]:9909`, `100.96.1.2:9909` (CGNAT v4), `[fd7a:115c:a1e0::1]:9909` (Tailscale ULA v6), `${HOST_TS_IP}:9909` and `{{ ts_ip }}:9909` (templates — host not IP-parseable, contains `$`/`{`).
- `validate_tailnet_bind` REJECTS (assert `is_err`): `0.0.0.0:9909`, `[::]:9909`, `:::9909`, `[fe80::1]:9909`, `192.168.1.5:9909`, `100.63.0.1:9909` (just below CGNAT), `9909` (bare port / no host), `:9909` (empty host), `100.96.1.2` (no port).
- (in `crates/fleet/src/doctor.rs`) the two existing private `is_cgnat` tests at the `super::*` import must still resolve and pass; the existing `check_serve_bind` `is_ok`/`is_err` tests must stay GREEN.

**Implementation.**
- Add to `crates/core/src/net.rs` exactly the spec §3.2 block: `pub fn is_cgnat`, `fn is_tailscale_v6`, `fn is_loopback_host`, `pub fn validate_tailnet_bind`. No new core deps — hand-rolled octet/segment range checks; bracket-strip IPv6 before the `rsplit_once(':')` so v6 colons don't fool the splitter.
- In `crates/fleet/src/doctor.rs`: replace the body of `check_serve_bind` with a delegate to `minimonitor_core::net::validate_tailnet_bind(...)` (drop the local `ipnet::Ipv4Net` use). Add `pub(crate) use minimonitor_core::net::is_cgnat;` so doctor's existing `super::*` `is_cgnat` tests still resolve. Remove the now-dead local `fn is_cgnat`. Drop `ipnet` from fleet's deps only if nothing else uses it (grep first; if used elsewhere, leave it).

**Acceptance.** New core tests pass; existing doctor tests pass unchanged; `cargo clippy -D warnings` clean across `core` + `fleet`; no `ipnet` in `core`. One commit: `feat(core): IPv6-aware validate_tailnet_bind + is_cgnat; doctor delegates`.

---

### B2 — agent bind resolution, self-guard, fail-safe untokened-tailnet refusal, allowlist routing, constant-time bearer

**Goal.** Replace the agent's hardcoded `127.0.0.1:9909` and catch-all route (§3.1, §3.3) with: precedence-resolved bind, IPv6-aware self-guard (reusing B1's validator), fail-closed loopback fallback, fail-safe refusal when tailnet-bound + untokened, an explicit GET allowlist (`/healthz` unauth, `/snapshot` token-gated, else 404), and a constant-time bearer compare. Keep the agent dependency-light: **flags/env only, no `agent.toml`, no serde/tokio.**

**Tests first** (`crates/agent/src/...`, pure-function unit tests — no socket, no live tailscale):
- `ct_eq`: equal slices ⇒ `true`; same-length-different ⇒ `false`; different-length ⇒ `false`; both empty ⇒ `true`.
- `authorized(headers, token)`: `token=None` ⇒ `true` regardless of headers; correct `Bearer <tok>` ⇒ `true`; wrong-but-same-length token ⇒ `false`; wrong-length ⇒ `false`; missing `Authorization` header ⇒ `false`; header present but non-`Bearer` scheme ⇒ `false`.
- `resolve_bind_with(args, env_lookup, tailnet_ip)` — injected sources, returns the resolved addr + a decision enum:
  - `--bind 100.96.1.2:9909` wins over env and auto.
  - env `MINIMONITOR_AGENT_BIND=100.96.1.2:9909` (non-empty) wins over auto when no flag.
  - empty env is ignored ⇒ falls through to auto.
  - auto: `tailnet_ip = Some("100.96.1.2")` ⇒ `100.96.1.2:9909`.
  - auto with `tailnet_ip = None` ⇒ fail-closed `127.0.0.1:9909` (assert the warning/decision flag is set).
  - **self-guard:** a resolved `0.0.0.0:9909` / `[::]:9909` without `--allow-non-tailnet` ⇒ exit-2 decision; with `--allow-non-tailnet` ⇒ allowed; loopback always allowed unconditionally.
  - **untokened-tailnet refusal (§3.3):** non-loopback (CGNAT) bind + `token=None` + no `--allow-untokened-tailnet` ⇒ refuse-to-start decision; with the flag ⇒ allowed; loopback + no token ⇒ allowed.
- routing (pure `route(method, path)` helper): `GET /healthz` ⇒ healthz (unauth); `GET /snapshot?x=1` and `GET /snapshot/` (trailing slash + query stripped) ⇒ snapshot; `POST /snapshot` ⇒ 404; `GET /anything-else` ⇒ 404.

**Implementation.**
- Factor pure helpers into the agent crate: `ct_eq`, `authorized` (spec §3.3 verbatim — uses `h.field.equiv("Authorization")` + `ct_eq`), `route(method, &str) -> Route` (strip query at first `?`, strip one trailing `/`, then match the allowlist), and `resolve_bind_with(args, env_lookup, tailnet_ip) -> BindDecision`. Precedence: flag → env(non-empty) → `network_identity(host).tailnet_ip` → loopback. Run `minimonitor_core::net::validate_tailnet_bind` on the resolved value for the self-guard; gate non-tailnet on `--allow-non-tailnet` and untokened-tailnet on `--allow-untokened-tailnet`.
- `main.rs`: replace `let addr = "127.0.0.1:9909"` (line ~35) with `resolve_bind_with(std::env::args, |k| std::env::var(k).ok(), minimonitor_core::net::network_identity(host).tailnet_ip)`; on a refuse/exit decision, stderr-warn and `std::process::exit(2)`. Token from `MINIMONITOR_AGENT_TOKEN` (empty/unset ⇒ `None`). Replace the catch-all dispatch with the `route()` allowlist; `/healthz` unauth; `/snapshot` calls `authorized(...)` and returns a static `"unauthorized"` 401 body when it fails; else static 404.
- **Token is never logged:** the bind/serve line echoes only the address; 401 body is the static string; no path interpolates the token. Do not rely on `redact`.

**Acceptance.** All agent unit tests pass; `cargo run -p minimonitor-agent -- --bind 0.0.0.0:9909` exits 2 with a stderr warning; `--bind 127.0.0.1:9909` serves; clippy/fmt clean; agent deps unchanged (still only `minimonitor-core`, `serde_json`, `tiny_http`). One commit: `feat(agent): tailnet bind resolution, allowlist routing, constant-time bearer, fail-safe guards`. **Cut the agent-bind PR here.**

---

## Collect / storage / UI PR (C1–C9)

### C1 — core `Deserialize` unblock + recorded fixture + round-trip test

**Goal.** Make `MonitorSnapshot` and every reachable type `Deserialize` so fleet can `serde_json::from_slice::<MonitorSnapshot>` with zero schema duplication (§2.3). Record and scrub the fixture in the same commit.

**Test first** (`crates/fleet/tests/snapshot_roundtrip.rs`):
```rust
let snap: MonitorSnapshot =
    serde_json::from_str(include_str!("fixtures/snapshot.json")).unwrap();
assert!(snap.total_memory_bytes > 0);
assert!(!snap.ports.is_empty());                 // macOS fixture has ports
assert_eq!(snap.ports[0].port > 0, true);
let _ = snap.load_average.0;                       // tuple field round-trips
assert_eq!(snap.sort_mode, SortMode::Cpu);         // enum round-trips
```
This **fails to compile today** (no `Deserialize`). The RED is a compile error — note this in the commit message; generate+scrub the fixture and add the derives in this same commit so the tree compiles for the gate harness.

**Implementation.**
- Add `Deserialize` to the existing `#[derive(...)]` on: `MonitorSnapshot`, `ProcessRow`, `CoreUsage`, `DiskVolume`, `SortMode`, `PortRow`, `ConnGroup`, `NetIdentity`, `AiSnapshot`, `AiWorkload` (every type reachable from `MonitorSnapshot`). For any enum with a custom `Serialize`, add a matching `Deserialize` (derive or manual) so the round-trip is symmetric.
- Generate the fixture: `cargo run -p minimonitor-agent -- --once > crates/fleet/tests/fixtures/snapshot.json` (run on this Mac mini so `ports`/`gpu`/`ai` are populated). Keep the full field set + null shapes (gpu null where applicable).
- **Scrub the fixture by hand to synthetic values** so the §8 fixture-scrub gate (added in C2) passes: every `command` / `example_command` becomes an allowlisted synthetic string (e.g. `/usr/bin/ollama serve`) with no secret-shaped substrings.

**Acceptance.** Round-trip test compiles and passes; `cargo test --workspace` green; secret-scan clean over the new fixture. One commit: `feat(core): derive Deserialize on snapshot types; add scrubbed fleet fixture`.

---

### C2 — extend `secrets::redact_str` + `scrub_command`

**Goal.** Robust collect-time scrubbing (§4.5): extend the redaction patterns and add `scrub_command` so stored argv never carries secrets, independent of the fixture being clean.

**Tests first** (`crates/fleet/src/secrets.rs` unit tests):
- `redact_str` / `scrub_command` redacts planted secrets in: `--password=hunter2`, `?api_key=AKIA123`, `token=ghp_abc123`, `secret=...`, `apikey=...` (assert case-insensitive: `--PASSWORD=`), `https://user:pass@host/path` (scheme://user:pass@), and the pre-existing `Bearer xyz`.
- Non-secret argv is unchanged: `/usr/bin/ollama serve --model llama3` round-trips untouched.
- **Fixture-scrub gate** (`crates/fleet/tests/fixture_scrub_test.rs`): parse `fixtures/snapshot.json`, assert every `ProcessRow.command` and `AiWorkload.example_command` is in a small synthetic allowlist and contains no secret-shaped substring.

**Implementation.**
- Extend `redact_str` patterns to cover `key=`, `token=`, `password=`, `secret=`, `apikey=` (case-insensitive, value-bearing) and `scheme://user:pass@host`, in addition to the existing `Bearer\s+\S+`.
- Add `pub fn scrub_command(&str) -> String` applying those patterns to a full command line.

**Acceptance.** Unit + fixture-scrub tests pass; secret-scan clean. One commit: `feat(fleet): extend redact patterns + scrub_command for argv-at-rest`.

---

### C3 — `M003` migration + `db/host.rs` write helpers

**Goal.** Land migration `M003` (the four tables, §5.2), plus `insert_snapshot` (scrub + parent + child rows + status upsert), `retention_sweep` (latest-guard), and `record_collect_failure` (status-only) — all going through `db::open` so cascades fire (§5.3–§5.5).

**Tests first.**
- **Migration-lock bump** (`crates/fleet/src/db/mod.rs`): change the existing `migration_applies_m001_all_tables` assert from `user_version == 2` to `== 3`, and extend its table-existence list with `host_snapshot, host_port, host_workload, host_collect_status`. This is the expected RED that signals M003 applied.
- **Storage tests** (`crates/fleet/tests/host_storage_test.rs`, temp DB via `db::open`, parse `fixtures/snapshot.json`):
  - `insert_snapshot` persists one `host_snapshot` parent with correct rollups (`workload_count = ai_snapshot.workload_count` un-truncated, `port_count = ports.len()`, cpu/mem/gpu), N `host_port` child rows, M `host_workload` child rows.
  - **scrub-at-rest:** plant a secret in a `ProcessRow.command` of the parsed snapshot, insert, then assert the planted secret does NOT appear in the stored `snapshot_json` blob nor in any `host_workload.example_command`.
  - **cascade:** delete the parent ⇒ child `host_port`/`host_workload` rows gone (proves `PRAGMA foreign_keys=ON` via `db::open`).
  - **retention latest-guard:** insert an old snapshot (collected_at < cutoff) and a recent one for the same node, plus an old-only snapshot for a second node; `retention_sweep(cutoff)` deletes the old one for node A but KEEPS node A's recent and KEEPS node B's sole (old) latest.
  - **`record_collect_failure`:** after a prior success, calling it stamps `last_attempt_at` + `last_error` but leaves `last_success_at` intact; first-ever failure leaves `last_success_at` NULL.

**Implementation.**
- Add `M003` SQL (spec §5.2 DDL verbatim: `host_snapshot`, `host_port`, `host_workload`, `host_collect_status` + indexes) and append `M::up(M003)` to the `Migrations::new(vec![...])` list (db/mod.rs ~line 103).
- New `crates/fleet/src/db/host.rs` (`pub mod host;` in `db/mod.rs`):
  - `insert_snapshot(conn: &mut Connection, node_id, raw: &[u8], snap: &MonitorSnapshot, now) -> Result<i64>` — one txn: scrub a working copy of `snap` (`scrub_command` over every `ProcessRow.command` and `AiWorkload.example_command`), serialize THAT as `snapshot_json`; INSERT parent rollups; `last_insert_rowid()`; INSERT each `host_port` from `snap.ports`; INSERT each `host_workload` from `snap.ai_snapshot.top_workloads` (scrubbed `example_command`); UPSERT `host_collect_status` success (`last_attempt_at = last_success_at = now`, `last_error = NULL`); commit. `collected_at = now.to_rfc3339()` (collector clock — NOT payload `captured_at`).
  - `retention_sweep(conn: &mut Connection, retention_days: u32) -> Result<usize>` — own txn; spec §5.3 `DELETE ... WHERE collected_at < cutoff AND id NOT IN (SELECT MAX(id) FROM host_snapshot GROUP BY node_id)`; children cascade.
  - `record_collect_failure(conn, node_id, err) -> Result<()>` — spec §5.5 upsert touching only `last_attempt_at` + `last_error`.

**Acceptance.** Migration-lock + all storage tests pass; cascades and latest-guard verified; clippy/fmt clean. One commit: `feat(fleet): M003 host-snapshot tables + write helpers (insert/retention/failure)`.

---

### C4 — config `[collect]` section + `snapshot_stale_secs`

**Goal.** Surface collect tunables and the serve staleness knob via the existing figment config (§4 defaults, §5.5, §6.5).

**Tests first** (`crates/fleet/src/config.rs` / `config_secrets_doctor_test.rs`):
- Defaults when `[collect]` absent: `agent_port == 9909`, `concurrency == 8`, `per_host_timeout_ms == 10_000`, `retention_days == 14`, `stale_after_hours == 3`, `token_env == None`; top-level `snapshot_stale_secs == 10_800`.
- TOML override of a field (e.g. `per_host_timeout_ms = 5000`) is read.
- Env override `FLEET_COLLECT__CONCURRENCY=16` (figment `__` nesting) is read.

**Implementation.** Add a `CollectConfig` struct with the fields above and serde defaults; nest it as `collect` on `Config`; add top-level `snapshot_stale_secs` (default 10_800). Confirm the existing figment provider already maps `FLEET_COLLECT__*` (it uses `__` nesting); add a profile/default if needed.

**Acceptance.** Config tests pass; clippy/fmt clean. One commit: `feat(fleet): [collect] config + snapshot_stale_secs`.

---

### C5 — `agent_client.rs` `AgentClient`

**Goal.** A fixture-testable connection seam (§4.3) returning `(Vec<u8>, MonitorSnapshot)` — no `bytes` dep.

**Tests first** (`crates/fleet/tests/agent_client_test.rs`, wiremock):
- 200 serving `fixtures/snapshot.json` ⇒ `Ok((raw, snap))`; `raw` non-empty and `snap.ports` populated.
- 500 ⇒ `Err` (assert the `HTTP 500` context).
- bearer: with `token = Some("t")`, assert the mock matched an `Authorization: Bearer t` header (wiremock `header(...)` matcher); with `None`, no auth header sent.
- malformed body (mock returns `not json`) ⇒ `Err` with the decode context.

**Implementation.** New `crates/fleet/src/agent_client.rs` — spec §4.3 `AgentClient` verbatim: `new(per_host_timeout)` builds a `reqwest::Client`; `async fn fetch_snapshot(&self, base_url, token: Option<&str>) -> anyhow::Result<(Vec<u8>, MonitorSnapshot)>` — `{base}/snapshot` (trim trailing `/`), optional `.bearer_auth`, non-2xx ⇒ bail, `resp.bytes().await?.to_vec()` (Vec<u8>, no `bytes` type named), `from_slice::<MonitorSnapshot>`. Add `wiremock` as a dev-dep. Confirm `bytes` is NOT added.

**Acceptance.** Wiremock tests pass; `grep bytes crates/fleet/Cargo.toml` empty; clippy/fmt clean. One commit: `feat(fleet): AgentClient snapshot fetch seam (Vec<u8>, no bytes dep)`.

---

### C6 — `commands/collect.rs` run loop + `Commands::Collect`

**Goal.** The resilient hourly pull loop (§4.1–§4.5): retention-first, `tier:agent` selection, bounded-concurrency per-host-timed pulls, additive persist, never `?`-propagate a per-host failure.

**Tests first** (`crates/fleet/tests/collect_loop_test.rs`, two wiremock targets + temp DB):
- Stand up one `MockServer` serving `fixtures/snapshot.json` at `GET /snapshot` (200) and a second unreachable target (a closed port / a server returning 500). Seed two `tier:agent` nodes whose addresses point at the two base URLs.
- Run `commands::collect::run(&cfg, &db_path).await` and assert it returns `Ok(())` even though one target is down.
- Reachable node: has a `host_snapshot` row + child `host_port`/`host_workload` rows + `host_collect_status.last_success_at` set.
- Unreachable node: NO `host_snapshot` row, but a `host_collect_status` row with `last_error` set and `last_success_at` NULL.
- (Optional) seed a non-agent (`tier != Agent`) node and assert it is never contacted / has no status row.

**Implementation.**
- New `crates/fleet/src/commands/collect.rs`, `pub async fn run(cfg, db_path) -> anyhow::Result<()>`:
  1. `db::open` (foreign_keys ON).
  2. **`retention_sweep` FIRST** in its own txn, before any HTTP.
  3. Select targets: `db::nodes::list_filtered(&conn, &ListFilter { tier: Some(Tier::Agent), ..Default::default() })`; in-loop skip nodes with no parseable v4 (`addresses.iter().filter_map(parse::<IpAddr>).find(is_ipv4)`); base URL `http://{ip}:{agent_port}`. **No `n.stale`** anywhere.
  4. Resolve token once: `cc.token_env.as_deref().map(|e| secrets::resolve(e, e)).transpose()?`.
  5. Bounded-concurrency pulls: `futures_util::stream::iter(targets).map(...).buffer_unordered(cc.concurrency)` with an outer `tokio::time::timeout(to, fetch)` per host (§4.4).
  6. Persist: each `Ok((raw, snap))` ⇒ `host::insert_snapshot`; each `Err(e)` ⇒ `host::record_collect_failure(..., secrets::redact(e))` + a redacted `eprintln!`. Return `Ok(())` regardless.
- Register `Commands::Collect` in `main.rs` mirroring `Probe` (load config, derive db_path, `commands::collect::run(&cfg, &db_path).await`).
- **Make `futures-util` explicit:** change fleet's `Cargo.toml` dep to `futures-util = { version = "0.3.32", features = ["std"] }` (so `StreamExt`/`buffer_unordered` don't rely on accidental feature unification).

**Acceptance.** Two-target additive-on-failure test passes; `run` returns `Ok(())` with a dead agent; `fleet collect --help` lists the verb; clippy/fmt clean. One commit: `feat(fleet): fleet collect pull loop (resilient, retention-first, bounded concurrency)`.

---

### C7 — `db/host.rs` read helpers + `model::is_stale` + `AppState` wiring

**Goal.** Indexed, blob-free read helpers for the UI (§6) plus the read-derived staleness rule and the `AppState` threshold — wired through **all three** router constructors so the crate compiles.

**Tests first.**
- `model::is_stale` (`crates/fleet/src/model.rs` tests): fresh `collected_at = now` ⇒ `false`; `now - 4h` with 3h threshold ⇒ `true`; unparseable timestamp ⇒ `true` (fail-safe).
- Read helpers (`host_storage_test.rs` additions, seed multiple snapshots per node): `all_ports` returns ALL ports of each node's **newest** snapshot (`MAX(id)` join — seed two snapshots for one node, assert only the newer snapshot's ports appear, and ALL of them, proving no `GROUP BY node` collapse); `all_workloads` likewise ordered by `total_cpu_percent DESC`; `latest_for_node` returns the newest by `id DESC`; `ports_for_node` / `workloads_for_node` scoped to one node's latest.

**Implementation.**
- In `db/host.rs`: `latest_for_node` (§6.1 query → `HostSnapshotDetail`), `all_ports` (§6.2 query → `FleetPortRow`), `all_workloads` (§6.3 query → `FleetWorkloadRow`), `ports_for_node`, `workloads_for_node` — all using `WHERE hs.id IN (SELECT MAX(id) FROM host_snapshot GROUP BY node_id)` and joining on `host_snapshot.id` (never `GROUP BY node`).
- `model::is_stale(collected_at: &str, threshold: Duration) -> bool` next to `is_online`; unparseable ⇒ stale.
- `AppState` gains `pub snapshot_stale_threshold: Duration`; set it in **`build_router`, `run_with`, and the test helper `full_router`** (compile-forcing). Wire it in `run_with` from `Config::snapshot_stale_secs`.

**Acceptance.** Read-helper + `is_stale` tests pass; all three constructors set the threshold (crate compiles); clippy/fmt clean. One commit: `feat(fleet): host read helpers (MAX(id) latest-per-node) + is_stale + AppState threshold`.

---

### C8 — `/node` host section, `/ports`, `/workloads` pages

**Goal.** The serve UI (§6.1–§6.3): an additive host section on `/node/{id}` that never 500s on absence, plus fleet-wide `/ports` and `/workloads` HTML pages with empty-states, staleness badges, and nav links.

**Tests first** (`crates/fleet/src/serve/...` tests, axum oneshot over seeded temp DB; add a `seed_host_snapshot(conn, node_id, collected_at, ports, workloads)` helper inserting one parent + child rows):
- `/ports`: with seeded rows ⇒ 200 + a row per seeded port (node link, port, proto, process, pid, bind rendered raw); meta line says "TCP only".
- `/workloads`: with seeded rows ⇒ 200 + a row per workload; "showing top 6" note when parent `workload_count` exceeds rendered rows.
- `/node/{id}` **with** a snapshot ⇒ 200 + host section (cpu/mem/gpu rollup, ports, top processes); **without** a snapshot ⇒ 200 + `No host snapshot collected yet` (NOT 404/500).
- **Staleness:** seed a snapshot at `now - 4h` ⇒ the `stale` badge/class is present; fresh ⇒ absent.
- **Empty DB** across `/ports`, `/workloads`, `/node/{id}` ⇒ 200 + `<p class="empty">`.

**Implementation.**
- Extend `get_node_html`/`NodePage` with an additive `Option<HostSnapshotView>` rendered from the **child tables** (`ports_for_node`/`workloads_for_node`) + rollup columns (don't block on blob Deserialize); empty-state on `None`. All byte→human / %→fixed formatting in the handler; askama `{% match Option %}` for nullables; render `bind` raw.
- New `/ports` + `/workloads` routes + handlers in `build_router_with` (clone `get_paths_html`): `ro_conn → all_ports/all_workloads → render`. Templates `templates/ports.html` and `templates/workloads.html` cloning `paths.html` with `{% if rows.is_empty() %}` empty-state and a per-row `stale` class (via `model::is_stale`). On `/workloads`, truncate `example_command` to ~80 chars. Add both pages to `base.html` nav.

**Acceptance.** All oneshot serve tests pass (seeded, empty, stale, empty-state); clippy/fmt clean. One commit: `feat(fleet): host section on /node + /ports + /workloads pages`.

---

### C9 — `/api/ports` + `/api/workloads` export builders + schema-lock + doctor agent checks

**Goal.** JSON APIs with frozen shapes (§6.4) and the doctor agent-bind / live-bind / token checks (§3.4).

**Tests first.**
- **Export builders** (`export_tests.rs`): `build_ports_json_at(rows, generated_at)` / `build_workloads_json_at(...)` produce deterministic structs with a per-row `stale: bool` (via `model::is_stale`).
- **Schema-lock golden** (mirror `schema_lock_api_fleet_key_paths`): `/api/ports` row keys exactly `{hostname, fleet_id, port, proto, process, pid, bind, collected_at, stale}` with `port` a number and `stale` a boolean; `/api/workloads` row keys exactly `{hostname, fleet_id, label, category, process_count, total_cpu_percent, total_memory_bytes, example_command, collected_at, stale}`. A field rename breaks the test.
- **Doctor** (`config_secrets_doctor_test.rs`): `check_agent_bind` accepts `100.96.1.2:9909` and `127.0.0.1:9909`, rejects `0.0.0.0:9909` / `[::]:9909`; token-resolvability: a set-but-unresolvable `token_env` ⇒ WARN (returns the service *name*, not value); a locally-detected tailnet-bound agent with no token ⇒ ERROR.

**Implementation.**
- `export::build_ports_json` / `build_workloads_json` (+ `_at` variants taking `generated_at`) returning `pub Serialize + Deserialize` structs with per-row `stale`. Register `/api/ports` + `/api/workloads` in `build_router_with`, handlers clone `get_fleet` (`ro_conn → all_ports/all_workloads → Json(build_*_json(...))`).
- Doctor: add `check_agent_bind(bind)` (spec §3.4 — loopback ok, else delegate to `core::net::validate_tailnet_bind`) and `check_agent_live_bind()` (scan `listening_ports()` for `:9909`, flag non-loopback/non-CGNAT binds). Wire into `run_doctor` after the serve-bind check: static agent-bind ERROR on fail, local active ERROR on fail, token check (`check_secret_resolvability` on `[collect].token_env` ⇒ WARN if unresolvable; tailnet-bound + untokened local agent ⇒ ERROR), and a line listing nodes with a non-null `host_collect_status.last_error`. Remote nodes get the static check only.

**Acceptance.** API + schema-lock + doctor tests pass; `cargo test --workspace` green; clippy/fmt/secret-scan/`cargo audit` clean. One commit: `feat(fleet): /api/ports + /api/workloads (schema-locked) + doctor agent-bind/token checks`. **Cut the collect/storage/UI PR here.**

---

## Follow-ons (named, not built in this plan)
`install.sh` agent LaunchAgent (`--bind <HOST_TS_IP>:9909`) + hourly `collect` cron; HTMX `?node=`/`?proto=` filters on `/ports`/`/workloads`; capped `host_process` table + fleet-wide process page; Linux collection paths in core; snapshot signing / mutual auth.

---

Plan file (recommended save location per CLAUDE.md workflow): `cat /Users/caguabot/Desktop/1/tools/minimonitor-wt-fleet/docs/superpowers/plans/2026-06-23-fleet-host-snapshots.md`
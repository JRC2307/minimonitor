# Fleet Phase 0+1 — TDD Implementation Plan

**Plan for:** `2026-06-20-fleet-phase0+1-inventory-registry-observability.md` (spec)
**Date:** 2026-06-20
**Implements:** plane 2 (inventory registry) + plane 5 (observability) of the fleet north-star, as the new `crates/fleet` member of the `minimonitor` workspace plus a git-tracked `deploy/` Docker stack.

---

## Preamble

### Branch

```
git switch -c feat/fleet-phase0-1
```

One commit per numbered task. Do not merge to `main` until the whole sequence is green and `/code-review` + `finishing-a-development-branch` have run.

### How to run tests

This is the workspace's first async crate. From the repo root (`/Users/caguabot/Desktop/1/tools/minimonitor`):

```bash
# fast inner loop, just the new crate
cargo test -p fleet

# a single task's tests (every task names its test module)
cargo test -p fleet <module>::

# whole workspace must still build/pass (core/agent/menubar untouched)
cargo test --workspace

# lint gate run before each commit
cargo clippy -p fleet --all-targets -- -D warnings
cargo fmt --check

# supply-chain gate (serde_yaml_ng over serde_yaml exists to keep this green)
cargo audit
```

TDD discipline for **every** task: write the test(s) first, watch them fail (compile-fail counts as red only after the type/sig exists — stub the signature with `todo!()` so the failure is an assertion failure, not a missing symbol), implement, watch them pass, run clippy+fmt, then make the single commit. Never write implementation before its test.

### External integrations — all tested against RECORDED FIXTURES, never live

| Surface | Test mechanism | Fixture location |
|---|---|---|
| Tailscale OAuth + `/devices` | `wiremock` `MockServer`, reqwest base-URL injected | `crates/fleet/tests/fixtures/tailscale/*.json` |
| Beszel PocketBase REST | `wiremock` | `crates/fleet/tests/fixtures/beszel/*.json` |
| Cloudflare REST (zones, cert-packs) | `wiremock` | `crates/fleet/tests/fixtures/cloudflare/*.json` |
| ntfy publish / healthchecks ping | `wiremock` | `crates/fleet/tests/fixtures/ntfy/*.json` |
| **Uptime-Kuma socket.io** | in-memory **fake `KumaClient`** for logic; **recorded payload fixture** for the `MonitorSpec` contract test; one non-ignored transport/replay test against frames or an ephemeral container | `crates/fleet/tests/fixtures/kuma/*.json` |
| **trippy-core MTR** | never traced live; `evaluate()` is pure over in-memory `HopStat` | n/a (synthetic structs) |

Fixtures are recorded **once** against the pinned versions (Kuma 1.23.16, Beszel 0.9.1 hub+agent, `trippy-core =0.13.0`, Homepage v0.10.9, ntfy v2.11.0). Re-record only on a deliberate version bump; a bump is its own commit.

### Residual questions to resolve during execution

Three residual questions from spec §10 are answered **by recording a fixture during the task that needs them**, not up front:
- Beszel agent's self-registered `name` (Task 14 / blocks Task 11's match key) → record against live `henrygd/beszel-agent:0.9.1` before finalizing Task 11's fixture.
- Kuma 1.23.16 frame shapes (Task 12) → record login-ack / `monitorList` / add-edit-delete frames before Task 12.
- Homepage v0.10.9 static-serve of `/app/public` (Task 13) → verify; caddy-sidecar fallback documented in Task 13.

---

## Task 1 — Repo hygiene + workspace scaffold

**Goal.** `.gitignore` covers all deploy secrets/data, a gitleaks CI check scans tracked files, and an empty `fleet` binary builds in the workspace and answers `--version`.

**Tests to write first.**
- `crates/fleet/tests/cli_smoke.rs::version_flag_prints_semver` — run the built binary via `assert_cmd` (add `assert_cmd = "2"` to dev-deps) with `--version`; assert exit 0 and stdout matches `^fleet 0\.2\.0`.
- `crates/fleet/tests/gitignore_test.rs::ignores_deploy_secrets` — read the repo-root `.gitignore`; assert it contains `deploy/.env`, `deploy/*_data/`, `deploy/ntfy/`, and `deploy/homepage/fleet/*.json`.
- A shell-level check (lives in the CI workflow, not cargo): `scripts/secret-scan.sh` greps tracked files for `tk_`, `Bearer `, `client_secret` and exits non-zero on a hit; a test fixture file under `crates/fleet/tests/fixtures/.gitkeep` proves the scan ignores untracked/fixture noise correctly (scan only `git ls-files`).

**Implementation.**
- Root `Cargo.toml`: add `"crates/fleet"` to `members`; hoist the fleet deps into `[workspace.dependencies]` exactly as spec §3.1 (clap, tokio, reqwest rustls-only, rusqlite bundled, rusqlite_migration, figment, serde_yaml_ng, chrono, anyhow, thiserror). Leave existing `version=0.2.0`, `edition=2024`, `resolver=2`, profiles untouched.
- `crates/fleet/Cargo.toml`: per spec §3.1 (package+bin both `fleet`, `version.workspace`, `edition.workspace`, path dep on `minimonitor-core`, the unstable pins `trippy-core="=0.13.0"` and `rust_socketio="0.6"` with `async`, `ipnet`, `async-trait`); dev-deps `wiremock`, `tempfile`, `assert_cmd`, `tokio`.
- `crates/fleet/src/main.rs`: `#[tokio::main]` stub that wires only clap `--version` for now (clap derive picks up `version` from `Cargo.toml`).
- Append to `.gitignore`: the four deploy patterns.
- `scripts/secret-scan.sh` + a `.github/workflows/secret-scan.yml` (or the repo's existing CI mechanism — if none, ship the script and document running it from `install.sh`).

**Acceptance.** `cargo build -p fleet` succeeds; `cargo test -p fleet` green; `cargo audit` clean (proves serde_yaml_ng choice); `scripts/secret-scan.sh` exits 0 on the clean tree. Commit.

---

## Task 2 — Config + secrets + doctor

**Goal.** Typed `Config` from `fleet.toml` + `FLEET_*` env; a deterministic secret resolver (env → Keychain → hard error); a redaction helper; `fleet doctor` preflight for bind-address and secret-resolvability.

**Tests to write first** (`config.rs`, `secrets.rs`, `doctor.rs` unit tests).
- `secrets::resolve` precedence: with `FLEET_X=foo` set, returns `foo` without shelling to `security`; empty env var falls through to Keychain path; both absent → `Err` whose message names the env var **and** the keychain service.
- `secrets::redact`: given an `anyhow::Error` chain containing a URL with `hc-ping.com/SECRETKEY/slug` and an `Authorization: Bearer tk_...` header string, the redacted `Display` contains neither `SECRETKEY` nor `tk_`. Explicit case: a `heartbeat`-style error must exclude the ping_key.
- `config`: a `fleet.toml` fixture parses into the typed `Config` (tailnets vec, beszel/kuma/cloudflare/ntfy/healthchecks/probe sections); `FLEET_ONLINE_THRESHOLD_SECS=600` env overrides the TOML value (figment layering); `~`-prefixed paths expand to `$HOME`.
- `doctor`: given a parsed compose-file string with a port published on `0.0.0.0:8090:8090`, the bind check returns an error; on `100.71.2.3:8090:8090` (in `100.64.0.0/10`) it passes; on a non-CGNAT public IP it fails. Use `ipnet` to test membership. Secret-resolvability check returns the list of unresolved secret names without printing their values.

**Implementation.**
- `config.rs`: figment `Toml::file(path).merge(Env::prefixed("FLEET_"))` → `Config` structs mirroring spec §3.5; `expand_tilde` helper.
- `secrets.rs`: `resolve(env_var, keychain_service)` exactly as spec §7 (env first, then `security find-generic-password -s <svc> -a fleet -w`, then `anyhow::ensure!`); `redact(e)` / `redact_ping(e, slug)` strippers (regex over the rendered chain for `Bearer \S+`, tokenized hc-ping/ntfy URLs).
- `doctor.rs`: parse published ports out of the compose YAML (`serde_yaml_ng`), validate each host bind against `100.64.0.0/10` via `ipnet`, reject `0.0.0.0`/empty; iterate every configured `*_env`/keychain secret and attempt `resolve`, collecting failures.

**Acceptance.** All unit tests green; redaction test proves no secret leaks. Commit.

---

## Task 3 — Model + DDL + migrations

**Goal.** `model.rs` types with `FleetId` validation/slugify; SQLite open (WAL + FK on) with `M001` migration; `node` upsert/list/get round-trip.

**Tests to write first.**
- `model::FleetId::new`: accepts `nas-01`, `mk:abc.def`, `n-1a2b3c4d`; **rejects** `foo;rm -rf`, `-oProxyCommand=x`, `a"b`, `a`backtick`b`, empty.
- `model::slugify`: `"Worker-01.local"` → `worker-01.local` style (lowercase, non-`[A-Za-z0-9._:-]` → `-`); a hostname with a leading `-` slugifies so it can't become an ssh option; quotes/semicolons/backticks gone.
- `db`: opening a fresh temp DB applies `M001` (assert `PRAGMA user_version` advances and every table from spec §3.6 exists: `node`, `node_seen`, `sync_run`, `enrollment`, `probe_run`, `probe_hop`, `cf_zone`); `PRAGMA foreign_keys` is ON; deleting a `node` cascades its `node_seen`/`enrollment`/`probe_run` rows.
- `db::nodes`: `upsert_node` then `get` returns an equal `Node` (addresses/raw_tags survive the JSON-array round-trip; `first_seen` set once and not overwritten on a second upsert; `updated_at` bumps).

**Implementation.**
- `model.rs`: `Node`, `TailnetRef`, `Tags`, `Tier`, `DedupeKind` exactly as spec §3.2; `FleetId(String)` newtype with `new() -> Result` enforcing `^[A-Za-z0-9._:-]+$`; `slugify`.
- `db/mod.rs`: `open(path) -> Connection` setting `journal_mode=WAL`, `foreign_keys=ON`; `migrations::to_latest` via `rusqlite_migration` with the `M001` baseline DDL (spec §3.6 verbatim).
- `db/nodes.rs`: `upsert_node` (`INSERT … ON CONFLICT(fleet_id) DO UPDATE`, JSON-encode `addresses`/`raw_tags`, preserve `first_seen`), `list`, `get`.

**Acceptance.** Migration + cascade + round-trip tests green; injection strings rejected. Commit.

---

## Task 4 — Tailscale client + merge (pure)

**Goal.** Tailscale OAuth + `/devices` client with injectable base URL; the pure multi-tailnet merge/dedupe.

**Tests to write first.**
- `tailscale` (wiremock): OAuth `POST /api/v2/oauth/token` fixture returns a bearer; `GET /api/v2/tailnet/-/devices?fields=default` fixture deserializes into `Vec<TsDevice>` (camelCase). A `429` with `Retry-After: 1` triggers one backoff+retry then succeeds. `lastSeen` with a `-05:00` offset parses and normalizes to UTC (assert the UTC instant, not the raw string).
- `merge` (pure, no I/O — the bulk of test weight):
  - **clean machineKey merge:** same `machineKey` across accounts `personal` + `client-acme` → one `Node`, `dedupe_key_kind=machinekey`, `seen_in` has both pairs, `addresses` = sorted union, `last_seen` = max.
  - **wiped-state via alias:** different machineKeys but an override alias collapses them → `dedupe_key_kind=alias`.
  - **colliding hostnames must NOT merge:** two `worker` boxes, different machineKeys, same hostname, no alias → **two** Nodes (fuzzy keys differ only if os/slug differ; assert they stay separate and are flagged fuzzy).
  - **filtering:** `isExternal=true` dropped; `authorized=false` dropped unless `include_unauthorized`.
  - **fuzzy synthetic-id minting + re-link:** first sight of a fuzzy box mints `n-<8hex>`; a second sync where that box was renamed re-links to the same minted id via the `fuzzy_hint`, not a new id.
  - **canonical row:** freshest `lastSeen` device supplies hostname/fqdn/os.

**Implementation.**
- `tailscale.rs`: `TsClient::new(base_url)`; `oauth_token` (`grant_type=client_credentials`, scope `devices:read`); `devices(tailnet)` with `Retry-After` backoff; `TsDevice` deser (camelCase, parse `lastSeen` via `parse_from_rfc3339().with_timezone(&Utc)`).
- `merge.rs`: `merge(per_account, overrides, prior, threshold) -> Vec<Node>` implementing spec §3.4 steps 1–7 (collect/filter, `merge_key` ladder, group, canonical, fold, mint+relink, overrides applied in §3.4 step 7 — but the override-apply *logic* may be stubbed to call into Task-5's `overrides::apply`; here test the merge keying and folding).
- `model.rs`: add `TsDevice`.

**Acceptance.** All merge cases green against synthetic vecs; tailscale client green against fixtures; offset normalization proven. Commit.

---

## Task 5 — Overrides + `fleet sync`

**Goal.** Load/validate/apply `fleet-overrides.yaml` (incl. cross-owner guard); wire the full `sync` pipeline (pull → merge → overrides → upsert → epoch-scoped sweep → `fleet.yaml`).

**Tests to write first.**
- `overrides`:
  - aliases collapse `members` under one `fleet_id` **before** grouping.
  - per-node layering: `nodes[fleet_id].tags` overwrites parsed facets; absent fields fall through (precedence override > parsed > default); `tier`/`notes` applied.
  - **cross-owner guard:** an alias whose members span `owner=self` and `owner=client-acme` → **load-time error** unless `ack_cross_owner: true`; an override flipping `client-*`→`self` emits a warning.
- `commands::sync` (DB + wiremock):
  - **additive on account failure:** account `client-acme` returns 500; `personal` succeeds → `personal` rows upserted, prior `client-acme` rows **remain** (not wiped); `sync_run.accounts_ok=["personal"]`.
  - **epoch-scoped sweep:** a `node_seen(personal, devX)` present last run but absent this run (account succeeded) → its provenance row deleted; a node whose every `seen_in` is gone becomes **stale-marked, not deleted**.
  - **YAML excludes volatile fields:** written `fleet.yaml` is sorted by `fleet_id` and contains **no** `last_seen`/`online`/`updated_at` keys (R-export); two syncs with only `last_seen` changing produce byte-identical YAML.

**Implementation.**
- `overrides.rs`: `load(path) -> Result<Overrides>` with the cross-owner validation; `apply(node, &Overrides)`; `alias_for(account, device_id)`.
- `db/nodes.rs`: epoch sweep (`last_confirmed_run != this_run` per succeeded account), stale-marking.
- `db/mod.rs`: `sync_run` insert + `accounts_ok` recording.
- `export.rs` (partial): `write_fleet_yaml` excluding volatile fields, sorted.
- `commands/sync.rs`: orchestrate per spec §3.7 `fleet sync` steps 1–8; non-zero exit on hard failure.

**Acceptance.** Sweep set-difference, additive-resilience, stale-marking, and volatile-exclusion tests green. Commit.

---

## Task 6 — `fleet list` / `show` / `ssh`

**Goal.** Pure SQLite reads with filters; safe `ssh` argv construction.

**Tests to write first.**
- `commands::list`: `--tag role:host` filters the `role` column; `--tier agent` filters tier; `--online` **recomputes** freshness at query time (a node with stale `online=1` but `last_seen` older than threshold renders ○); `--json` emits `Vec<Node>`; fuzzy rows get a `~` marker in the table.
- `commands::show`: resolve by fleet_id, hostname, and fqdn; an ambiguous hostname lists candidates and exits non-zero; output includes every `seen_in` pair, `dedupe_key_kind`, joined enrollment status.
- `commands::ssh` (**security-critical**): build argv for a node whose API fqdn is `-oProxyCommand=evil` → the built argv connects to the **validated `100.x` `IpAddr`** parsed from `addresses` (an `IpAddr`, not the fqdn), passes `user@IP` as **separate** argv elements, and inserts `--` before the host token; assert the crafted name never appears as an option. `--ts` swaps to `tailscale ssh`. `--all` reports per-node exit. (Test the argv vector via a seam — `build_ssh_argv()` returns `Vec<String>` — do not exec.)

**Implementation.**
- `db/nodes.rs`: `list` with facet/tier filters; `get_by_ref` (id|hostname|fqdn, ambiguity → candidates).
- `commands/list.rs`, `commands/show.rs`: table formatter (aligned columns per spec §3.7) + `--json`.
- `commands/ssh.rs`: `build_ssh_argv(node, user, ts)` parsing a `100.x` `IpAddr` from `addresses`, `user@IP` split, `--` separator; `exec` only outside tests.

**Acceptance.** Filter recompute + ambiguity + the `-`-prefixed-fqdn safety test green. Commit.

---

## Task 7 — `fleet export` + schema lock

**Goal.** Build `fleet.json` / `cf.json` / `path-health.json` for Homepage with a frozen, fixtures-tested schema.

**Tests to write first.**
- `export::build_fleet_json`: produces the exact shape of spec §3.7 (`generated_at`, `nodes[]` with `id/hostname/tier/online/site/role/owner/last_seen`); `online` is the registry-derived value (1/0).
- **schema-lock test:** serialize a known fixture `Vec<Node>` and assert the JSON's depended-on dotted key paths (`nodes`, `nodes[].hostname`, `nodes[].online`, `nodes[].site`) match a checked-in golden file byte-for-byte (Homepage `customapi` is brittle to renames — a rename must fail this test).
- `cf.json` and `path-health.json` builders emit **empty-but-valid** structures (`{"zones":[]}`, `{"hops":[]}`) until their source commands land, and validate against their golden shape.

**Implementation.**
- `export.rs`: `build_fleet_json`, `build_cf_json`, `build_path_health_json`; `write_exports(dir)` writing all three into `export_dir`.
- `commands/export.rs`: load from DB, write files.
- Golden fixtures under `crates/fleet/tests/fixtures/export/`.

**Acceptance.** Schema-lock golden matches; empty-but-valid cf/path files validate. Commit.

---

## Task 8 — `fleet cf-sync`

**Goal.** Read-only Cloudflare pull (zones + cert-packs), nested `min(expires_on)`, SSL-warn ntfy. REST only, no GraphQL.

**Tests to write first** (wiremock + pure fold).
- **envelope `success:false`:** an HTTP 200 carrying `{"success":false,"errors":[...]}` → `Err` (must check `success` AND `errors`, not just status).
- `GET /user/tokens/verify` preflight failure aborts.
- zone pagination: two pages of `?per_page=50&page=N` merge; `healthy := status=="active" && !paused`.
- **cert-pack expiry fold (load-bearing):** `GET /zones/{id}/ssl/certificate_packs?status=all&per_page=50` fixture with multiple packs each holding RSA+ECDSA certs → `min_cert_expiry = min(pack.certificates[].expires_on)` across all; assert `status=all` is in the requested URL (omitting it hides expired packs).
- threshold: a zone with `min_cert_expiry` within `ssl_warn_days` triggers an ntfy publish (assert the wiremock ntfy endpoint was hit at priority 4); an unhealthy zone also alerts.
- upsert into `cf_zone` round-trips.

**Implementation.**
- `cloudflare.rs`: `CfClient::new(base_url, token)`; `verify_token`, `zones` (paginated), `cert_packs(zone_id)` always with `status=all`; envelope check helper; pure `min_cert_expiry` fold.
- `db/cf.rs`: `upsert_cf_zone`.
- `commands/cf_sync.rs`: pull → upsert → evaluate thresholds → `alert::ntfy` on breach.

**Acceptance.** Envelope-false, nested-min, `status=all`, threshold-alert tests green. Commit.

---

## Task 9 — `fleet probe` (the built MTR prober)

**Goal.** trippy-core adapter (unprivileged + Classic + startup self-check); per-hop aggregation; pure destination-hop-only `evaluate()`; severity; retention-at-start; breach ntfy; `path-health.json`.

**Tests to write first** (pure `evaluate`/aggregation + DB; never trace live).
- `evaluate` **destination-hop-only (the #1 false-positive trap):**
  - a *middle* hop at 100% loss with later responding hops → **no alert**.
  - the destination (last responding) hop over `loss_threshold` or `rtt_threshold` → alert.
  - **fully unreachable** (all hops `???`/non-responding) → `dest` resolution yields `None` → no destination alert (handled, not panic).
- severity mapping: hop at 0.7× threshold → `warn`; over → `breach`; under → `ok`; the computed `severity` strings are what land in `path-health.json`.
- aggregation: a synthetic `Vec<HopStat>` → one `probe_run` + N `probe_hop` rows with loss%/RTT stats persisted; `path_type` (`underlay`/`overlay`) stored per target.
- **retention runs in its own transaction at command start (R-13):** seed a run older than `retention_days`; even when the current run early-returns on a breach, the old run is GC'd (assert the old row is gone after a breach-path invocation).
- `path-health.json` carries the latest run's destination-hop summary with precomputed `severity`.
- adapter self-check: a constructor that cannot open the unprivileged dgram-ICMP socket returns a loud `Err` (inject the socket-open via a seam so the test doesn't need privileges).

**Implementation.**
- `probe.rs`: the **single** file touching `trippy-core =0.13.0` — `Builder` with `PrivilegeMode::Unprivileged`, `MultipathStrategy::Classic`, `Protocol::Icmp`, `max_rounds`; v4-only; startup socket self-check; `aggregate(trace) -> Vec<HopStat>`; pure `evaluate(hops, loss, rtt) -> Option<Alert>` and `severity(hop, thresholds)`.
- `db/probe.rs`: `insert_run`+`insert_hops`; `retention_sweep(days)` in its own txn called **first** in the command.
- `commands/probe.rs`: retention → resolve targets (pinned `[[probe.target]]` + registry-derived `[[probe.selector]]`, tagged `path`) → per target trace via `spawn_blocking` → persist → evaluate → ntfy breach (priority 4) → rebuild `path-health.json`.

**Acceptance.** Middle-hop-not-alerted, all-`???`, severity, retention-on-breach tests green; trippy isolated to `probe.rs`. Commit.

---

## Task 10 — `fleet heartbeat`

**Goal.** External dead-man's-switch ping to hc-ping.com with `?create=1`, env-resolvable ping_key, redacted errors.

**Tests to write first** (wiremock standing in for hc-ping base URL).
- URL is built as `{base}/{ping_key}/{slug}?create=1` (self-provisioning).
- non-2xx response → non-zero exit (`error_for_status`).
- **ping_key never in error output:** a forced failure's error `Display` excludes the ping_key (uses `redact_ping`).
- ping_key resolves from `FLEET_HC_PING_KEY` env **without** Keychain (so it pages even when Keychain is locked — R-8).

**Implementation.**
- `alert.rs`: `heartbeat(base, ping_key, slug)` per spec §6 with `redact_ping`; 10s timeout.
- `commands/heartbeat.rs`: resolve ping_key (env-preferred), call, propagate exit.

**Acceptance.** `?create=1`, non-2xx→nonzero, redaction, env-resolvability tests green. Commit.

---

## Task 11 — Beszel enroll (PocketBase REST)

**Goal.** Idempotent agent-tier enroll: `users` auth (raw token), match-on-self-reported-identity (never blind create-by-fleet_id), backfill/PATCH drift, decommission under the 40% guard, on-demand universal-token enable.

**Tests to write first** (wiremock; record Beszel 0.9.1 fixtures — lock the agent self-registered `name`/`host` shape first, residual Q2).
- auth: `POST /api/collections/users/auth-with-password` → token; header sent **raw** (no `Bearer`), against the **`users`** collection (asserts not `_superusers`).
- **parameterized filter (R-2):** the systems lookup uses a bound params object (`filter=host={:h}`), never string-interpolated; a node whose host/name contains injection chars is pre-validated/slugified and cannot alter the filter.
- **idempotent, no dup:** existing `systems` record matching the agent's self-reported host → enroll **PATCHes** the friendly `name`/links `users`, records `remote_id` in `enrollment`, and a **second run creates nothing** (asserts no `POST /systems/records`).
- **never blind-create:** a desired agent node with no matching record → enroll does **not** create-by-fleet_id; instead it enables the bootstrap token on-demand (see below).
- **decommission under guard:** an `enrollment(beszel)` whose fleet_id is no longer a desired agent → `DELETE /systems/records/{id}` + row drop, **but** if >40% of existing systems would be deleted (hardcoded constant) → abort loudly, delete nothing.
- **on-demand universal-token (R-15):** when a not-yet-registered desired agent exists → token enabled; when **no** new agent nodes exist → token **not** re-enabled (assert the enable endpoint is not hit) so agents' baked-in env tokens don't stale.

**Implementation.**
- `beszel.rs`: PocketBase client (`auth_with_password` users-collection, raw `Authorization`), `list_systems`, `patch_system`, `delete_system`, parameterized `filter`; on-demand token enable.
- `db` (`enrollment`): upsert/list/delete for `system='beszel'`.
- `commands/enroll.rs` (beszel half): reconcile with the 40% guard; `--dry-run` prints the plan.

**Acceptance.** No-dup, never-blind-create, guard-abort, on-demand-token tests green. Commit.

---

## Task 12 — Kuma enroll (socket.io — the load-bearing surface)

**Goal.** `kuma/sio.rs` connect/login-ack/await-`monitorList`/emit-with-ack against pinned 1.23.16; pure `reconcile()` with delete-guard; `MonitorSpec` contract fixture; one non-ignored transport test.

**Tests to write first** (record Kuma 1.23.16 frames first — residual Q3).
- **`MonitorSpec` serialization contract:** serialize a `MonitorSpec` and assert it matches a recorded 1.23.16 payload fixture byte-for-byte (full object: `type∈ping|http|port`, `hostname`/`url`/`port`, `interval`, `maxretries`, `notificationIDList:{<ntfy_id>:true}`). A field rename fails **here**, not production.
- `reconcile()` against an **in-memory fake `KumaClient`**:
  - desired monitor absent on server → `add` (never blind-add when present).
  - present + drifted → `edit` with the **full** object.
  - present + in-sync → no-op.
  - undesired present → `delete`.
  - **delete-guard boundaries:** empty `have`, empty `want`, exactly `guard_pct` (40), and just over → over-guard aborts before any delete.
- **transport/replay (non-ignored):** replay recorded engine.io/socket.io frames (login ack carrying JWT, `monitorList` broadcast, add/edit/delete acks) against a local socket.io mock **OR** run against an ephemeral `louislam/uptime-kuma:1.23.16` container in CI; assert `list()` resolves from the **pushed** broadcast (the oneshot armed before connect) and `add` returns the new monitorID. This proves the push-based dance, not just the logic.

**Implementation.**
- `kuma/mod.rs`: `KumaClient` trait (`list`/`add`/`edit`/`delete`); pure `reconcile(c, want, guard_pct)` exactly as spec §3.7 (name = fleet_id as idempotency key; full `MonitorSpec` always sent).
- `kuma/sio.rs`: the **only** wire-protocol file — `connect_and_login` arming the `monitorList` oneshot before `connect()`, `emit_with_ack("login")` extracting the JWT, `add/edit/delete` via `emit_with_ack`.
- `model.rs`: `MonitorSpec`, `RemoteMonitor`.
- `commands/enroll.rs` (kuma half): build `MonitorSpec`s from agentless nodes (with `notificationIDList` → ntfy id), call `reconcile`, store `monitorID` in `enrollment`.

**Acceptance.** Contract fixture matches; all reconcile + guard-boundary cases green; the non-ignored transport/replay test passes against pinned 1.23.16. Commit.

---

## Task 13 — Docker stack + Homepage config

**Goal.** Pinned `deploy/docker-compose.yml` bound to `${MINI_TS_IP}`; Homepage `services.yaml` (customapi + native widgets); `.env.example`; verify static-serve of exports.

**Tests to write first** (file/lint-level — no live containers in cargo).
- `crates/fleet/tests/deploy_test.rs::compose_binds_tailnet_only` — parse `deploy/docker-compose.yml`; assert every published port is templated on `${MINI_TS_IP}` and **no** literal `0.0.0.0`/bare `host:container` wildcard appears; assert image tags are pinned (Beszel `0.9.1`, Kuma `1.23.16`, Homepage `v0.10.9`, ntfy `v2.11.0`).
- `compose_kuma_has_net_raw` — Kuma service has `cap_add: [NET_RAW]`.
- `homepage_services_use_var_refs` — `services.yaml` contains only `{{HOMEPAGE_VAR_*}}` references, no literal secrets; the three customapi widgets point at `http://localhost:3000/fleet/{fleet,path-health,cf}.json`.
- `doctor` (from Task 2) run over this real compose file passes the bind check.

**Implementation.**
- `deploy/docker-compose.yml` exactly as spec §4 (all four services pinned, `${MINI_TS_IP}` host-bound, healthcheck on Beszel, `NET_RAW` on Kuma, Homepage `public/fleet` bind-mount + `HOMEPAGE_ALLOWED_HOSTS`, ntfy `deny-all`).
- `deploy/homepage/config/services.yaml` per spec §4.1 (customapi Fleet Nodes / Path Health / SSL & Zones + native uptimekuma + beszel rollups).
- `deploy/.env.example` (no real secrets).
- **Static-serve verification (residual Q4):** manually `docker compose up` Homepage v0.10.9 and confirm `http://localhost:3000/fleet/fleet.json` serves the bind-mounted file. **If it does not**, add the documented **caddy sidecar** (a `caddy:2` service serving `./homepage/fleet` on an internal port, customapi URLs repointed at it) and note the decision in `deploy/README.md`.

**Acceptance.** Compose-bind, pin, NET_RAW, and var-ref tests green; manual static-serve confirmed (or caddy fallback added). Commit.

---

## Task 14 — Beszel agent rollout (doc + compose)

**Goal.** Per-owned-box agent compose using the one-time bootstrap token + push-through-NAT; document the rollout and lock the agent's self-registered identity used by Task 11's match key.

**Tests to write first.**
- `crates/fleet/tests/deploy_test.rs::agent_compose_is_push_model` — parse the per-box `beszel-agent` compose; assert `network_mode: host`, `image: henrygd/beszel-agent:0.9.1` (matches hub), `TOKEN: ${BESZEL_BOOTSTRAP_TOKEN}`, **no inbound published port** (outbound WS only), docker.sock mounted read-only.

**Implementation.**
- `deploy/agent/docker-compose.yml` per spec §4.2 (host network, `LISTEN: 45876`, `HUB_URL` tailnet, bootstrap token env).
- `deploy/agent/README.md`: rollout steps, the push-through-NAT rationale (no SSH-key model, do not run `install-agent.sh`), and **the recorded self-registered `name`/`host`** value (residual Q2) that Task 11's enroll matches on — record it against the live agent and write it down here so the match key is locked.

**Acceptance.** Push-model compose test green; rollout doc records the locked match identity. Commit.

---

## Task 15 — Install + scheduling

**Goal.** Extend `scripts/install.sh`: `fleet doctor` preflight → template `${MINI_TS_IP}` from `tailscale ip -4` (hard-fail empty) → `docker compose up -d` → install LaunchAgents on the spec cadence/boot-order.

**Tests to write first** (script-level, runnable in CI with stubbed `tailscale`/`docker`).
- `scripts/install_test.sh` (or bats): with a stubbed `tailscale ip -4` returning empty → install **hard-fails** before any compose action (R-5).
- with `tailscale ip -4` → `100.71.2.3`, the rendered compose/env has `MINI_TS_IP=100.71.2.3`.
- `fleet doctor` is invoked **before** `docker compose up` (assert ordering via a trace/log).
- the generated LaunchAgent plists exist for: `heartbeat` (60s / `* * * * *`-equivalent `StartInterval 60`), `sync`/`enroll`/`probe` (300s, **offset** start times), `cf-sync` (900s), and `export` chained after sync/probe/cf-sync; boot order = stack up → sync → enroll → probe/cf-sync → export.

**Implementation.**
- Extend `scripts/install.sh` (keep the existing menubar install intact): build `fleet` release, run `fleet doctor`, resolve `MINI_TS_IP` (fail on empty), write `deploy/.env` `MINI_TS_IP`, `docker compose -f deploy/docker-compose.yml up -d`, then emit the per-command LaunchAgent plists with the cadences/offsets above (model them on the existing `com.caguabot.minimonitor.plist` template).
- `deploy/README.md`: the full boot/schedule table + rotation runbook pointer (spec §7).

**Acceptance.** Empty-IP hard-fail, IP templating, doctor-before-up ordering, and all plist-presence tests green. Commit.

---

## Done criteria

- `cargo test --workspace` green; `cargo clippy -p fleet --all-targets -- -D warnings` and `cargo fmt --check` clean; `cargo audit` clean (no `serde_yaml`).
- All nine verbs (`sync · enroll · cf-sync · export · probe · heartbeat · list · show · ssh`) plus `doctor` exist and are tested.
- Every external surface is fixture-backed; the Kuma socket.io surface has a contract test **and** a non-ignored transport test; no test makes a live network call (except the optional ephemeral-Kuma CI container).
- Security tests prove: injection-bearing hostnames are slugified/rejected, PocketBase filters bind, `fleet ssh` connects to a validated IP with `--`, and no secret (ntfy/CF token, hc.io ping-key) appears in any error output.
- Then run `/code-review`, address findings, and `superpowers:finishing-a-development-branch` to open the PR.

The plan above is the deliverable. Spec build order (§9) is preserved one-to-one across Tasks 1–15; each task is test-first, self-contained, and ends in a single commit. External integrations using recorded fixtures are called out in the preamble table and reiterated per task (Tailscale/Beszel/Cloudflare/ntfy/healthchecks via wiremock JSON fixtures; Kuma via a recorded `MonitorSpec` payload fixture + replay frames; trippy never traced live).
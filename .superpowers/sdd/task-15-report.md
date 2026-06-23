## Task 15 — Install + Scheduling — Implementation Report

**Date:** 2026-06-22  
**Branch:** fleet-phase-0-1  
**Status:** Complete — all tests GREEN

---

### What was implemented

**TDD order: test → RED → implement → GREEN**

#### 1. `scripts/install_test.sh` (new file)

Script-level TDD test suite with PATH stubs for `tailscale`, `docker`, `fleet`, `launchctl`, `cargo`, and `install`. Uses `FLEET_INSTALL_DRYRUN=1` + `FLEET_INSTALL_ROOT` to avoid any system mutation.

Tests and results:

| Test | Description | Result |
|------|-------------|--------|
| (a) | Empty `tailscale ip -4` → hard-fail before any `docker compose up` | PASS |
| (a) | `docker compose` NOT called when IP is empty | PASS |
| (b) | `tailscale ip -4 → 100.71.2.3` → `deploy/.env` has `HOST_TS_IP=100.71.2.3` | PASS |
| (c) | `fleet doctor` invoked BEFORE `docker compose up` (trace-log ordering) | PASS |
| (d) | All 6 expected plists exist | PASS (6/6) |
| (d) | heartbeat plist has StartInterval 60 | PASS |
| (d) | sync/enroll/probe plists have StartInterval 300 | PASS (3/3) |
| (d) | cf-sync plist has StartInterval 900 | PASS |
| (d) | export plist has WatchPaths or StartInterval | PASS |
| (e) | NO `com.caguabot.fleet.serve.plist` (Task 18) | PASS |

**Total: 18 passed, 0 failed**

#### 2. `scripts/install.sh` (extended)

Added `--fleet` mode alongside the existing menubar path (unchanged). Fleet install path:
1. `cargo build --release -p fleet` + install binary
2. `fleet doctor` preflight (BEFORE compose)
3. `tailscale ip -4` → hard-fail on empty (R-5)
4. Write `deploy/.env` with `HOST_TS_IP=<100.x>` (idempotent, preserves other keys)
5. `docker compose -f deploy/docker-compose.yml up -d`
6. Write + load 6 fleet LaunchAgent plists
7. `FLEET_INSTALL_DRYRUN=1` skips `launchctl load` and actual compose up

`install_menubar()` preserved verbatim — no regressions.

#### 3. LaunchAgent plists (written at install time into `~/Library/LaunchAgents/`)

| Label | Verb | StartInterval | Stagger offset |
|-------|------|---------------|----------------|
| `com.caguabot.fleet.heartbeat` | `heartbeat` | 60 s | 0 |
| `com.caguabot.fleet.sync` | `sync` | 300 s | 0 |
| `com.caguabot.fleet.enroll` | `enroll` | 300 s | +30 s (`sleep 30 && fleet enroll`) |
| `com.caguabot.fleet.probe` | `probe` | 300 s | +60 s (`sleep 60 && fleet probe`) |
| `com.caguabot.fleet.cf-sync` | `cf-sync` | 900 s | +120 s |
| `com.caguabot.fleet.export` | `export` | 300 s fallback | WatchPaths on `fleet.yaml` |

Stagger encoded directly in the plist via `/bin/sh -c "sleep N && fleet <verb>"` — no external wrapper script needed.

`fleet serve` plist intentionally absent (Task 18).

#### 4. `deploy/README.md` (new file)

Documents:
- Full boot/schedule table with offsets
- Port binding table
- First-time setup steps
- Secrets split table (native CLI vs Docker)
- Rotation runbook per token type (ntfy, Tailscale OAuth, CF token, CF tunnel token, hc.io ping key)
- Image pin / update procedure
- Log locations

---

### Check suite results

| Check | Result |
|-------|--------|
| `bash scripts/install_test.sh` | 18/18 PASS |
| `cargo test --workspace` | ok (7 core tests + fleet doc-tests) |
| `cargo clippy -p fleet --all-targets -- -D warnings` | clean |
| `cargo fmt --check` | clean |
| `bash scripts/secret-scan.sh` | clean |

---

### Files changed

- `scripts/install.sh` — extended with `--fleet` path
- `scripts/install_test.sh` — new, TDD test suite
- `deploy/README.md` — new, boot/schedule table + rotation runbook

## Fix round 1

**Reviewer findings addressed (commit amend):**

- **C-1 (Critical):** Removed the `docker compose up -d` call from the `FLEET_INSTALL_DRYRUN=1` else-branch. Dryrun now only prints `[DRYRUN] would run: docker compose -f <file> up -d` and never execs real docker.
- **C-2 (Critical):** Removed the `launchctl load` call from the dryrun else-branch. Dryrun now prints `[DRYRUN] would load: <plist path>` for each plist and never calls real launchctl.
- **I-1 (Important):** Test (a) now uses a single run (not two runs sharing the same `$TRACE_FILE`). Return-code and trace assertions are both made on that one run.
- **I-2 (Important):** Replaced the no-op stagger grep with real assertions: sync plist must NOT contain `sleep`, enroll plist must contain `sleep 30`, probe plist must contain `sleep 60`.
- **Minor (ENV_FILE path):** `ENV_FILE` now resolves to `$DEPLOY_DIR/.env` (co-located with `docker-compose.yml`) in production, and to `$_HOME/deploy/.env` (under `FLEET_INSTALL_ROOT`) when running tests — keeps tests clean and fixes the prod path mismatch.
- **Test (c) ordering check:** Updated to use a new `run_fleet_install_nodryrun` helper so the docker stub is actually invoked and traced, confirming ordering in the non-dryrun path without touching any real system tools.

**Post-fix check results:** 20/20 PASS (install_test.sh) · cargo test ok · clippy clean · fmt clean · secret-scan clean

# Task 1 Report — repo hygiene + workspace scaffold (fleet crate)

**Branch:** `fleet-phase-0-1`
**Commit:** `24f5ee5`
**Date:** 2026-06-22

---

## What Was Built

1. **`crates/fleet/` crate** — new workspace member with:
   - `src/main.rs` — minimal clap `--version` binary
   - `Cargo.toml` — all fleet dependencies wired via workspace, plus pinned `trippy-core =0.13.0` and `rust_socketio 0.6`
   - `tests/cli_smoke.rs` — integration test asserting `fleet --version` prints `fleet 0.2.0`
   - `tests/gitignore_test.rs` — test asserting `.gitignore` contains `deploy/.env`, `deploy/*_data/`, `deploy/ntfy/`
   - `tests/fixtures/.gitkeep` — empty fixture directory

2. **Root `Cargo.toml` updated** — added `crates/fleet` to workspace members; added 14 workspace deps (clap, tokio, reqwest, rusqlite, rusqlite_migration, figment, serde_yaml_ng, chrono, anyhow, thiserror, axum, askama, tower-http, async-trait, ipnet, tower)

3. **`.gitignore` updated** — appended `deploy/.env`, `deploy/*_data/`, `deploy/ntfy/`, `deploy/certs/`

4. **`scripts/secret-scan.sh`** — bash script scanning `git ls-files` for `tk_`, `Bearer `, `client_secret`; exits non-zero on hit; chmod +x applied

5. **`.github/workflows/secret-scan.yml`** — CI workflow running secret-scan on every push and PR

---

## TDD RED Evidence

Before creating `crates/fleet/Cargo.toml` and `src/main.rs`:

```
$ cargo test -p fleet 2>&1 | head -5
error: package ID specification `fleet` did not match any packages
help: a package with a similar name exists: `flate2`
```

The test files existed but the crate did not — confirming RED state.

---

## TDD GREEN Evidence

After implementation:

```
$ cargo test -p fleet 2>&1 | tail -20
     Running tests/cli_smoke.rs (target/debug/deps/cli_smoke-e1f81388d3547812)

running 1 test
test version_flag_prints_semver ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.20s

     Running tests/gitignore_test.rs (target/debug/deps/gitignore_test-1d3a7d60cd72daea)

running 1 test
test ignores_deploy_secrets ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

Workspace-wide: 9 tests total (7 core + 2 fleet), all passing.

---

## Files Created/Changed

| Action | Path |
|--------|------|
| Created | `crates/fleet/Cargo.toml` |
| Created | `crates/fleet/src/main.rs` |
| Created | `crates/fleet/tests/cli_smoke.rs` |
| Created | `crates/fleet/tests/gitignore_test.rs` |
| Created | `crates/fleet/tests/fixtures/.gitkeep` |
| Created | `scripts/secret-scan.sh` |
| Created | `.github/workflows/secret-scan.yml` |
| Modified | `Cargo.toml` (members + workspace.dependencies) |
| Modified | `.gitignore` (deploy/* entries) |
| Modified | `Cargo.lock` (new dep tree) |

---

## Version Adjustments vs Brief

| Dep | Brief Requested | Actual Used | Reason |
|-----|----------------|-------------|--------|
| `rusqlite` | `0.31` | `0.35` | `rusqlite_migration 2.1` requires rusqlite `^0.35`; brief's `0.31` caused libsqlite3-sys link conflict |
| `rusqlite_migration` | `1.3` | `2.1` | `1.3` requires rusqlite `^0.32` (again conflicts with `0.31`); `2.1` aligns with rusqlite `0.35` |
| `askama` | `0.13` | `0.13.1` | Resolved correctly as requested |
| `tower-http` | `0.6` | `0.6.11` | Resolved correctly as requested |
| `thiserror` | `2` | `2.0.x` | Resolved correctly as requested |
| All others | per brief | per brief | No changes needed |

**Root cause of rusqlite conflict:** `rusqlite_migration` versions and rusqlite versions must be aligned because rusqlite_migration re-exports rusqlite types. Each rusqlite_migration minor version pins a specific rusqlite minor version. The brief's combination (rusqlite `0.31` + rusqlite_migration `1.3`) was internally inconsistent because `1.3` resolves to require rusqlite `^0.32`.

---

## Concerns

1. **Secret scan false positives in docs**: `scripts/secret-scan.sh` exits non-zero because `docs/superpowers/plans/` and `docs/superpowers/specs/` contain the literal strings `tk_`, `Bearer `, and `client_secret` as documentation/spec text (pattern descriptions, not real credentials). The scanner is working correctly — these are design documents describing the patterns the scanner is supposed to catch. Future work should either add a `.secret-scan-ignore` mechanism or exclude `docs/` from the scan.

2. **`cargo fmt --check` on existing crates**: The pre-existing crates (agent, core, menubar) have formatting diffs vs the workspace's rustfmt edition. These are pre-existing issues not introduced by this task. The fleet crate itself passes `cargo fmt -p fleet --check` cleanly.

3. **`cargo audit` not run**: `cargo-audit` was not installed in this environment. No known vulnerability check was performed on the dependency tree.

4. **`trippy-core 0.13.0` and `rust_socketio 0.6`** build cleanly on macOS (confirmed in build output). These are pinned directly in fleet's Cargo.toml per spec — not workspace deps.

---

## Commit SHA

`24f5ee5` — feat(fleet): scaffold fleet crate — workspace member, clap --version, CI secret scan

---

## Fix round 1

**Date:** 2026-06-22
**Base commit amended:** `24f5ee5` → `e6fc36a`

### C-1 — Remove OpenSSL/native-tls (rust_socketio deferred to Task 12)

**Finding:** `rust_socketio 0.6` pulled `native-tls`, `hyper-tls`, and `tokio-native-tls` into the fleet dep tree. `trippy-core 0.13.0` was confirmed clean (no OpenSSL/native-tls).

**What was done:**
- Removed `rust_socketio = { version = "0.6", features = ["async"] }` from `crates/fleet/Cargo.toml`
- Left a comment in its place: `# rust_socketio: deferred to Task 12 (Kuma) — pulls native-tls/OpenSSL, decision owned there`
- Note: `rust_socketio` was NOT in workspace deps (it was a direct fleet dep) — no workspace change needed

**cargo tree verification:**
```
$ cargo tree -p fleet -i openssl-sys 2>&1
error: package ID specification `openssl-sys` did not match any packages

$ cargo tree -p fleet 2>&1 | grep -iE 'openssl|native-tls|hyper-tls|tokio-native-tls'
(no output — clean)
```

### C-2 — secret-scan.sh exits 0 on clean tree

**Finding:** The script scanned itself (`scripts/secret-scan.sh`) and `docs/` design files, which legitimately contain the literal patterns `tk_`, `Bearer `, `client_secret` as documentation of what the scanner looks for.

**What was done:**
- Changed `git ls-files` pipeline to `git ls-files -- ':!:scripts/secret-scan.sh' ':!:docs/'`
- Added `scripts/secret-scan.test.sh` — shell integration test that:
  1. Plants `tk_aaaabbbbcccc1234` in a staged temp file under `crates/fleet/tests/fixtures/`, asserts scanner exits non-zero (detection works)
  2. Unstages + deletes the file, asserts scanner exits 0 (clean tree)
- Added `crates/fleet/tests/secret_scan_test.rs` — two Rust tests calling the scripts via `std::process::Command`:
  - `secret_scan_clean_tree_exits_zero` — runs `secret-scan.sh` directly, asserts exit 0
  - `secret_scan_detects_and_clean_integration` — runs `secret-scan.test.sh`, asserts exit 0

**Live run proof:**
```
$ bash scripts/secret-scan.sh; echo "exit=$?"
Secret scan passed
exit=0
```

**Integration test run:**
```
=== Test 1: scanner detects a staged secret ===
SECRET FOUND in crates/fleet/tests/fixtures/fake_secret_DO_NOT_COMMIT.tmp: tk_
Secret scan FAILED
PASS: scanner correctly exited non-zero
=== Test 2: scanner exits 0 on clean tree ===
rm 'crates/fleet/tests/fixtures/fake_secret_DO_NOT_COMMIT.tmp'
Secret scan passed
PASS: scanner exits 0 on clean tree
All secret-scan tests passed.
test-exit=0
```

### I-1 — cargo audit

**Installed:** `cargo-audit v0.22.2`

**Result:** 0 vulnerabilities. 13 warnings (all `unmaintained` or `unsound` at warn level):
- 10x gtk-rs GTK3 bindings (`atk`, `atk-sys`, `gdk`, `gdk-sys`, `gdkwayland-sys`, `gdkx11`, `gdkx11-sys`, `gtk`, `gtk-sys`, `gtk3-macros`) — RUSTSEC-2024-041x/042x — from `menubar` crate's wry dep, not fleet
- `paste v1.0.15` — RUSTSEC-2024-0436 — unmaintained, from transitive dep
- `proc-macro-error v1.0.4` — RUSTSEC-2024-0370 — unmaintained, from transitive dep
- `glib v0.18.5` — RUSTSEC-2024-0429 — unsound VariantStrIter, from menubar/wry

**None of these are in the fleet crate's dep tree.** All are from `menubar`'s GTK3/wry dependencies. Exit code: 0.

### I-2 — axum empty features = []

**What was done:** Removed `features = []` from the workspace dep:
```toml
# Before:
axum = { version = "0.8", features = [] }
# After:
axum = { version = "0.8" }
```

### cargo fmt

Pre-existing formatting drift in `crates/agent`, `crates/core`, `crates/menubar` was also fixed by running `cargo fmt` across the workspace. All four crates now pass `cargo fmt --check`.

### Test results

```
$ cargo test -p fleet
     Running tests/cli_smoke.rs
test version_flag_prints_semver ... ok
test result: ok. 1 passed

     Running tests/gitignore_test.rs
test ignores_deploy_secrets ... ok
test result: ok. 1 passed

     Running tests/secret_scan_test.rs
test secret_scan_clean_tree_exits_zero ... ok
test secret_scan_detects_and_clean_integration ... ok
test result: ok. 2 passed

$ cargo test --workspace
(4 fleet tests + 7 core tests = 11 total, all ok)

$ cargo build -p fleet
Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.55s

$ cargo clippy -p fleet --all-targets -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.52s
(exit 0 — clean)

$ cargo fmt --check
(exit 0 — clean)
```

### Amended commit SHA

`e6fc36a` — feat(fleet): scaffold fleet crate — workspace member, clap --version, CI secret scan

```
$ git log --oneline -3
e6fc36a feat(fleet): scaffold fleet crate — workspace member, clap --version, CI secret scan
f77f349 docs: retarget monitor to Intel mini host + custom fleet serve UI
6ac0b2a docs: Phase 0+1 spec + TDD plan (fleet registry + observability)
```

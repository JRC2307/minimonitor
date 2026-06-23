#!/usr/bin/env bash
# scripts/install_test.sh — TDD tests for the fleet install path in scripts/install.sh
#
# Tests (all run in CI with stubbed tailscale/docker):
#   (a) Empty tailscale ip -4  → hard-fail BEFORE any docker compose up
#   (b) tailscale ip -4 → 100.71.2.3 → deploy/.env has HOST_TS_IP=100.71.2.3
#   (c) fleet doctor invoked BEFORE docker compose up (trace-log ordering)
#   (d) Generated plists exist for: heartbeat(60s), sync/enroll/probe(300s offset),
#       cf-sync(900s), export (chained after sync/probe/cf-sync)
#   (e) NO fleet serve plist exists yet (Task 18)
#
# Run from the repo root:  bash scripts/install_test.sh
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

PASS=0
FAIL=0
TRACE_FILE=""
STUB_BIN_DIR=""
TMPDIR_ROOT=""

# ── helpers ─────────────────────────────────────────────────────────────────

pass() { echo "PASS: $*"; ((PASS++)) || true; }
fail() { echo "FAIL: $*"; ((FAIL++)) || true; }

cleanup() {
    [[ -n "${TMPDIR_ROOT:-}" ]] && rm -rf "$TMPDIR_ROOT"
}
trap cleanup EXIT

setup_stubs() {
    local ts_ip="${1:-}"          # What tailscale ip -4 should return (empty = empty)
    local doctor_exit="${2:-0}"   # Exit code for 'fleet doctor' stub
    TMPDIR_ROOT="$(mktemp -d)"
    STUB_BIN_DIR="$TMPDIR_ROOT/stubs"
    TRACE_FILE="$TMPDIR_ROOT/trace.log"
    mkdir -p "$STUB_BIN_DIR"

    # tailscale stub
    cat > "$STUB_BIN_DIR/tailscale" <<STUBEOF
#!/usr/bin/env bash
echo "tailscale \$*" >> "$TRACE_FILE"
if [[ "\$1" == "ip" && "\$2" == "-4" ]]; then
    printf '%s' "${ts_ip}"
fi
STUBEOF
    chmod +x "$STUB_BIN_DIR/tailscale"

    # docker stub — logs every invocation
    cat > "$STUB_BIN_DIR/docker" <<STUBEOF
#!/usr/bin/env bash
echo "docker \$*" >> "$TRACE_FILE"
exit 0
STUBEOF
    chmod +x "$STUB_BIN_DIR/docker"

    # fleet stub — logs every invocation; doctor exits per param
    cat > "$STUB_BIN_DIR/fleet" <<STUBEOF
#!/usr/bin/env bash
echo "fleet \$*" >> "$TRACE_FILE"
if [[ "\$1" == "doctor" ]]; then
    exit ${doctor_exit}
fi
exit 0
STUBEOF
    chmod +x "$STUB_BIN_DIR/fleet"

    # launchctl stub — just log; never actually load
    cat > "$STUB_BIN_DIR/launchctl" <<STUBEOF
#!/usr/bin/env bash
echo "launchctl \$*" >> "$TRACE_FILE"
exit 0
STUBEOF
    chmod +x "$STUB_BIN_DIR/launchctl"

    # cargo stub — log and exit 0 (so build doesn't run)
    cat > "$STUB_BIN_DIR/cargo" <<STUBEOF
#!/usr/bin/env bash
echo "cargo \$*" >> "$TRACE_FILE"
# Simulate: produce a fake fleet binary so install -m 755 ... works
if [[ "\$1" == "build" ]]; then
    mkdir -p "$TMPDIR_ROOT/target/release"
    printf '#!/usr/bin/env bash\necho fake-fleet\n' > "$TMPDIR_ROOT/target/release/fleet"
    chmod +x "$TMPDIR_ROOT/target/release/fleet"
fi
exit 0
STUBEOF
    chmod +x "$STUB_BIN_DIR/cargo"

    # Provide a fake 'install' that copies the binary without needing root
    cat > "$STUB_BIN_DIR/install" <<STUBEOF
#!/usr/bin/env bash
# Minimal install(1) stub: install -m MODE src dst
# Args: [-m mode] src dst
args=("\$@")
src="\${args[\${#args[@]}-2]}"
dst="\${args[\${#args[@]}-1]}"
mkdir -p "\$(dirname "\$dst")"
cp -f "\$src" "\$dst" 2>/dev/null || true
STUBEOF
    chmod +x "$STUB_BIN_DIR/install"
}

run_fleet_install() {
    # Run the fleet portion of install.sh with stubs on PATH.
    # Sets HOME to a tmpdir so plist writes go somewhere safe.
    export FLEET_INSTALL_DRYRUN=1
    export FLEET_INSTALL_ROOT="$TMPDIR_ROOT"
    PATH="$STUB_BIN_DIR:$PATH" HOME="$TMPDIR_ROOT" \
        bash "$REPO_ROOT/scripts/install.sh" --fleet 2>&1 || true
}

run_fleet_install_expect_fail() {
    # Returns the exit code of install.sh (expecting non-zero).
    export FLEET_INSTALL_DRYRUN=1
    export FLEET_INSTALL_ROOT="$TMPDIR_ROOT"
    set +e
    PATH="$STUB_BIN_DIR:$PATH" HOME="$TMPDIR_ROOT" \
        bash "$REPO_ROOT/scripts/install.sh" --fleet 2>&1
    local rc=$?
    set -e
    echo "$rc"
}

run_fleet_install_nodryrun() {
    # Run WITHOUT dryrun so real docker/launchctl stubs are called and traced.
    # Stubs are on PATH, so nothing real is touched.
    unset FLEET_INSTALL_DRYRUN
    export FLEET_INSTALL_ROOT="$TMPDIR_ROOT"
    PATH="$STUB_BIN_DIR:$PATH" HOME="$TMPDIR_ROOT" \
        bash "$REPO_ROOT/scripts/install.sh" --fleet 2>&1 || true
    export FLEET_INSTALL_DRYRUN=1   # restore for subsequent tests
}

# ── Test (a): Empty tailscale ip -4 → hard-fail before docker compose up ────

echo ""
echo "=== Test (a): empty tailscale ip -4 → hard-fail before any docker compose up ==="
setup_stubs ""    # empty IP

# Single run: capture both output+rc in one shot to avoid TRACE_FILE reuse
set +e
OUTPUT_A="$(PATH="$STUB_BIN_DIR:$PATH" HOME="$TMPDIR_ROOT" FLEET_INSTALL_DRYRUN=1 \
    FLEET_INSTALL_ROOT="$TMPDIR_ROOT" bash "$REPO_ROOT/scripts/install.sh" --fleet 2>&1)"
RC=$?
set -e

if [[ "$RC" != "0" ]]; then
    pass "(a) install.sh exits non-zero when tailscale ip -4 is empty"
else
    fail "(a) install.sh should exit non-zero on empty tailscale IP"
fi

# docker compose must NOT have been called
if grep -q "docker compose\|docker-compose" "$TRACE_FILE" 2>/dev/null; then
    fail "(a) docker compose was called despite empty tailscale IP"
else
    pass "(a) docker compose was NOT called (correctly blocked)"
fi

# ── Test (b): tailscale ip -4 → 100.71.2.3 → deploy/.env has HOST_TS_IP ────

echo ""
echo "=== Test (b): tailscale ip -4 → 100.71.2.3 → deploy/.env has HOST_TS_IP=100.71.2.3 ==="
setup_stubs "100.71.2.3"
run_fleet_install

ENV_FILE="$TMPDIR_ROOT/deploy/.env"
if [[ -f "$ENV_FILE" ]] && grep -q "HOST_TS_IP=100.71.2.3" "$ENV_FILE"; then
    pass "(b) deploy/.env contains HOST_TS_IP=100.71.2.3"
else
    fail "(b) deploy/.env missing or HOST_TS_IP not set to 100.71.2.3 (file: ${ENV_FILE})"
    [[ -f "$ENV_FILE" ]] && cat "$ENV_FILE" || echo "  (file not found)"
fi

# ── Test (c): fleet doctor invoked BEFORE docker compose up ─────────────────

echo ""
echo "=== Test (c): fleet doctor invoked BEFORE docker compose up ==="
setup_stubs "100.71.2.3"
# Run WITHOUT dryrun so the stub docker is actually invoked and traced.
# Stubs are on PATH, so nothing real is touched.
run_fleet_install_nodryrun

if [[ ! -f "$TRACE_FILE" ]]; then
    fail "(c) trace file not written"
else
    # Get line numbers for first occurrence of each
    DOCTOR_LINE=$(grep -n "fleet doctor" "$TRACE_FILE" | head -1 | cut -d: -f1 || echo "")
    COMPOSE_LINE=$(grep -n "docker compose" "$TRACE_FILE" | head -1 | cut -d: -f1 || echo "")

    if [[ -z "$DOCTOR_LINE" ]]; then
        fail "(c) 'fleet doctor' never invoked"
    elif [[ -z "$COMPOSE_LINE" ]]; then
        fail "(c) 'docker compose up' never invoked"
    elif [[ "$DOCTOR_LINE" -lt "$COMPOSE_LINE" ]]; then
        pass "(c) fleet doctor (line $DOCTOR_LINE) invoked before docker compose up (line $COMPOSE_LINE)"
    else
        fail "(c) docker compose up (line $COMPOSE_LINE) ran BEFORE fleet doctor (line $DOCTOR_LINE)"
    fi
fi

# ── Test (d): generated plists exist with correct cadences ──────────────────

echo ""
echo "=== Test (d): generated plist files exist for fleet commands ==="

LAUNCHAGENTS="$TMPDIR_ROOT/Library/LaunchAgents"

setup_stubs "100.71.2.3"
run_fleet_install

# Expected plists
EXPECTED_PLISTS=(
    "com.caguabot.fleet.heartbeat.plist"
    "com.caguabot.fleet.sync.plist"
    "com.caguabot.fleet.enroll.plist"
    "com.caguabot.fleet.probe.plist"
    "com.caguabot.fleet.cf-sync.plist"
    "com.caguabot.fleet.export.plist"
)

for plist in "${EXPECTED_PLISTS[@]}"; do
    plist_path="$LAUNCHAGENTS/$plist"
    if [[ -f "$plist_path" ]]; then
        pass "(d) $plist exists"
    else
        fail "(d) $plist MISSING at $plist_path"
    fi
done

# Verify heartbeat has StartInterval 60
HEARTBEAT="$LAUNCHAGENTS/com.caguabot.fleet.heartbeat.plist"
if [[ -f "$HEARTBEAT" ]] && grep -q "StartInterval" "$HEARTBEAT" && grep -q ">60<" "$HEARTBEAT"; then
    pass "(d) heartbeat plist has StartInterval 60"
else
    fail "(d) heartbeat plist missing StartInterval 60"
    [[ -f "$HEARTBEAT" ]] && cat "$HEARTBEAT" || true
fi

# Verify sync/enroll/probe have StartInterval 300
for verb in sync enroll probe; do
    PLIST="$LAUNCHAGENTS/com.caguabot.fleet.${verb}.plist"
    if [[ -f "$PLIST" ]] && grep -q "StartInterval" "$PLIST" && grep -q ">300<" "$PLIST"; then
        pass "(d) $verb plist has StartInterval 300"
    else
        fail "(d) $verb plist missing StartInterval 300"
    fi
done

# Verify stagger offsets are actually encoded in the generated plists.
# sync has no stagger (stagger_sleep=0) — its ProgramArguments must NOT contain a sleep.
# enroll has stagger_sleep=30 — ProgramArguments must contain "sleep 30".
# probe has stagger_sleep=60 — ProgramArguments must contain "sleep 60".

SYNC_PLIST="$LAUNCHAGENTS/com.caguabot.fleet.sync.plist"
ENROLL_PLIST="$LAUNCHAGENTS/com.caguabot.fleet.enroll.plist"
PROBE_PLIST="$LAUNCHAGENTS/com.caguabot.fleet.probe.plist"

if [[ -f "$SYNC_PLIST" ]] && ! grep -q "sleep" "$SYNC_PLIST"; then
    pass "(d) sync plist has no sleep stagger (offset=0)"
else
    fail "(d) sync plist unexpectedly contains a sleep (should have stagger_sleep=0)"
fi

if [[ -f "$ENROLL_PLIST" ]] && grep -q "sleep 30" "$ENROLL_PLIST"; then
    pass "(d) enroll plist contains 'sleep 30' stagger"
else
    fail "(d) enroll plist missing 'sleep 30' stagger"
    [[ -f "$ENROLL_PLIST" ]] && grep -A2 "ProgramArguments" "$ENROLL_PLIST" || true
fi

if [[ -f "$PROBE_PLIST" ]] && grep -q "sleep 60" "$PROBE_PLIST"; then
    pass "(d) probe plist contains 'sleep 60' stagger"
else
    fail "(d) probe plist missing 'sleep 60' stagger"
    [[ -f "$PROBE_PLIST" ]] && grep -A2 "ProgramArguments" "$PROBE_PLIST" || true
fi

# Verify cf-sync has StartInterval 900
CFSYNC="$LAUNCHAGENTS/com.caguabot.fleet.cf-sync.plist"
if [[ -f "$CFSYNC" ]] && grep -q "StartInterval" "$CFSYNC" && grep -q ">900<" "$CFSYNC"; then
    pass "(d) cf-sync plist has StartInterval 900"
else
    fail "(d) cf-sync plist missing StartInterval 900"
    [[ -f "$CFSYNC" ]] && cat "$CFSYNC" || true
fi

# Verify export plist exists and is chained (WatchPaths or QueueDirectories on sync/probe/cf-sync output)
EXPORT="$LAUNCHAGENTS/com.caguabot.fleet.export.plist"
if [[ -f "$EXPORT" ]]; then
    if grep -qE "WatchPaths|QueueDirectories|StartInterval" "$EXPORT"; then
        pass "(d) export plist exists with chaining trigger"
    else
        fail "(d) export plist exists but has no chaining trigger (WatchPaths/QueueDirectories/StartInterval)"
    fi
else
    fail "(d) export plist MISSING"
fi

# ── Test (e): fleet serve plist IS present (Task 18) ────────────────────────
#
# Task 18 ships the `fleet serve` LaunchAgent.  After implementation this plist
# MUST exist, have KeepAlive true (long-running daemon, NOT StartInterval), and
# RunAtLoad true.

echo ""
echo "=== Test (e): com.caguabot.fleet.serve.plist exists (Task 18) ==="

# Re-use the last dryrun run from Test (d) — LAUNCHAGENTS is already set.
SERVE_PLIST="$LAUNCHAGENTS/com.caguabot.fleet.serve.plist"

if [[ -f "$SERVE_PLIST" ]]; then
    pass "(e) com.caguabot.fleet.serve.plist exists"
else
    fail "(e) com.caguabot.fleet.serve.plist is MISSING — Task 18 must emit it"
fi

# ── Test (f): serve_launchagent_present — shape assertions ─────────────────
#
# The serve plist must:
#   - Contain KeepAlive true  (long-running daemon, NOT StartInterval)
#   - Contain RunAtLoad true
#   - NOT contain StartInterval (that is the interval-agent pattern; serve is a daemon)

echo ""
echo "=== Test (f): serve_launchagent_present — KeepAlive/RunAtLoad shape ==="

# Fresh dryrun run (independent tmpdir) so assertions are deterministic.
setup_stubs "100.71.2.3"
run_fleet_install

SERVE_PLIST2="$TMPDIR_ROOT/Library/LaunchAgents/com.caguabot.fleet.serve.plist"

if [[ -f "$SERVE_PLIST2" ]]; then
    pass "(f) com.caguabot.fleet.serve.plist emitted by install (fresh run)"
else
    fail "(f) com.caguabot.fleet.serve.plist not found after install"
fi

# KeepAlive true — must contain both the key and the value on adjacent lines
if grep -q "<key>KeepAlive</key>" "$SERVE_PLIST2" 2>/dev/null; then
    pass "(f) serve plist contains <key>KeepAlive</key>"
else
    fail "(f) serve plist missing <key>KeepAlive</key>"
    [[ -f "$SERVE_PLIST2" ]] && cat "$SERVE_PLIST2" || true
fi

if grep -q "<true/>" "$SERVE_PLIST2" 2>/dev/null; then
    pass "(f) serve plist contains <true/> (KeepAlive value)"
else
    fail "(f) serve plist missing <true/> for KeepAlive"
fi

# RunAtLoad true
if grep -q "<key>RunAtLoad</key>" "$SERVE_PLIST2" 2>/dev/null; then
    pass "(f) serve plist contains <key>RunAtLoad</key>"
else
    fail "(f) serve plist missing <key>RunAtLoad</key>"
fi

# Must NOT contain StartInterval (serve is a long-running daemon, not interval-scheduled)
if grep -q "StartInterval" "$SERVE_PLIST2" 2>/dev/null; then
    fail "(f) serve plist must NOT contain StartInterval (it is a KeepAlive daemon, not a cron agent)"
    grep "StartInterval" "$SERVE_PLIST2" || true
else
    pass "(f) serve plist correctly has no StartInterval"
fi

# dryrun guard — in dryrun mode launchctl must NOT be called; output must print
# [DRYRUN] would load ... serve.plist
DRYRUN_OUT="$(setup_stubs "100.71.2.3" && run_fleet_install 2>&1)"
if echo "$DRYRUN_OUT" | grep -q "\[DRYRUN\].*serve"; then
    pass "(f) dryrun prints [DRYRUN] for serve plist instead of calling launchctl"
else
    fail "(f) dryrun output missing [DRYRUN] marker for serve plist"
fi

# ── Summary ──────────────────────────────────────────────────────────────────

echo ""
echo "────────────────────────────────────────────"
echo "Results: $PASS passed, $FAIL failed"
echo "────────────────────────────────────────────"

if [[ "$FAIL" -gt 0 ]]; then
    exit 1
fi

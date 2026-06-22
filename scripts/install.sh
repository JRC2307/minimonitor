#!/usr/bin/env bash
# scripts/install.sh — MiniMonitor + Fleet install script
#
# Usage:
#   bash scripts/install.sh            # installs the menubar app (existing behaviour)
#   bash scripts/install.sh --fleet    # installs the fleet observability stack
#
# Environment variables:
#   FLEET_INSTALL_DRYRUN=1   Skip launchctl load and docker compose up (testing/CI)
#   FLEET_INSTALL_ROOT       Override root path for plist/bin writes (testing/CI)
#
set -euo pipefail

# ── Resolve paths ─────────────────────────────────────────────────────────────

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"

# Allow tests to redirect writes to a tmpdir
_HOME="${FLEET_INSTALL_ROOT:-$HOME}"

MODE="${1:-}"   # --fleet or empty (menubar)

# ─────────────────────────────────────────────────────────────────────────────
# MENUBAR PATH (existing behaviour, preserved)
# ─────────────────────────────────────────────────────────────────────────────

install_menubar() {
    APP_DIR="$_HOME/Applications/MiniMonitor"
    BIN_DIR="$APP_DIR/bin"
    BIN_PATH="$BIN_DIR/minimonitor"
    PLIST_PATH="$_HOME/Library/LaunchAgents/com.caguabot.minimonitor.plist"

    mkdir -p "$BIN_DIR" "$_HOME/Library/LaunchAgents"

    cd "$ROOT_DIR"
    cargo build --release
    install -m 755 "$ROOT_DIR/target/release/minimonitor" "$BIN_PATH"

    cat > "$PLIST_PATH" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.caguabot.minimonitor</string>

  <key>ProgramArguments</key>
  <array>
    <string>$BIN_PATH</string>
  </array>

  <key>RunAtLoad</key>
  <true/>

  <key>KeepAlive</key>
  <true/>

  <key>WorkingDirectory</key>
  <string>$APP_DIR</string>
  <key>StandardOutPath</key>
  <string>/tmp/minimonitor.log</string>
  <key>StandardErrorPath</key>
  <string>/tmp/minimonitor.error.log</string>
</dict>
</plist>
PLIST

    if [[ "${FLEET_INSTALL_DRYRUN:-0}" != "1" ]]; then
        launchctl unload "$PLIST_PATH" >/dev/null 2>&1 || true
        launchctl load "$PLIST_PATH"
    fi

    printf 'Installed MiniMonitor to %s\n' "$BIN_PATH"
    printf 'LaunchAgent loaded from %s\n' "$PLIST_PATH"
}

# ─────────────────────────────────────────────────────────────────────────────
# FLEET PATH
# ─────────────────────────────────────────────────────────────────────────────

install_fleet() {
    FLEET_APP_DIR="$_HOME/Applications/Fleet"
    FLEET_BIN_DIR="$FLEET_APP_DIR/bin"
    FLEET_BIN_PATH="$FLEET_BIN_DIR/fleet"
    LAUNCHAGENTS_DIR="$_HOME/Library/LaunchAgents"
    DEPLOY_DIR="$ROOT_DIR/deploy"

    mkdir -p "$FLEET_BIN_DIR" "$LAUNCHAGENTS_DIR"

    # ── 1. Build fleet binary ───────────────────────────────────────────────

    cd "$ROOT_DIR"
    cargo build --release -p fleet
    install -m 755 "$ROOT_DIR/target/release/fleet" "$FLEET_BIN_PATH"

    # ── 2. Preflight: fleet doctor ──────────────────────────────────────────
    # Must happen BEFORE any docker compose up (spec §4, R-5)

    printf 'Running fleet doctor preflight...\n'
    if ! fleet doctor; then
        printf 'ERROR: fleet doctor preflight failed. Aborting install.\n' >&2
        exit 1
    fi

    # ── 3. Resolve HOST_TS_IP from tailscale ip -4 (R-5: hard-fail on empty) ─

    printf 'Resolving tailscale IP...\n'
    HOST_TS_IP="$(tailscale ip -4 2>/dev/null || true)"
    HOST_TS_IP="${HOST_TS_IP%%$'\n'*}"   # take first line only, strip trailing newline
    HOST_TS_IP="${HOST_TS_IP// /}"       # strip any spaces

    if [[ -z "$HOST_TS_IP" ]]; then
        printf 'ERROR: tailscale ip -4 returned empty. Is Tailscale running?\n' >&2
        printf 'Fleet install aborted. No docker compose up was attempted.\n' >&2
        exit 1
    fi

    printf 'HOST_TS_IP=%s\n' "$HOST_TS_IP"

    # ── 4. Write deploy/.env with HOST_TS_IP ───────────────────────────────

    # ENV_FILE co-locates with docker-compose.yml so --env-file points at the right
    # file.  In production FLEET_INSTALL_ROOT is unset, so ENV_FILE resolves to
    # $ROOT_DIR/deploy/.env (alongside the compose file).  In tests FLEET_INSTALL_ROOT
    # redirects _HOME to a tmpdir and we mirror that so the test's deploy/.env assertion
    # still works without touching the real repo.
    if [[ -n "${FLEET_INSTALL_ROOT:-}" ]]; then
        ENV_FILE="${_HOME}/deploy/.env"
    else
        ENV_FILE="$DEPLOY_DIR/.env"
    fi
    ENV_DIR="$(dirname "$ENV_FILE")"
    mkdir -p "$ENV_DIR"
    {
        # Preserve existing .env entries other than HOST_TS_IP (safe idempotent update)
        if [[ -f "$ENV_FILE" ]]; then
            grep -v "^HOST_TS_IP=" "$ENV_FILE" || true
        else
            # Seed from .env.example if first time
            [[ -f "$DEPLOY_DIR/.env.example" ]] && grep -v "^HOST_TS_IP=" "$DEPLOY_DIR/.env.example" || true
        fi
        printf 'HOST_TS_IP=%s\n' "$HOST_TS_IP"
    } > "${ENV_FILE}.tmp"
    mv "${ENV_FILE}.tmp" "$ENV_FILE"
    chmod 600 "$ENV_FILE"
    printf 'Wrote %s\n' "$ENV_FILE"

    # ── 5. docker compose up -d ─────────────────────────────────────────────

    if [[ "${FLEET_INSTALL_DRYRUN:-0}" != "1" ]]; then
        printf 'Starting observability stack...\n'
        docker compose -f "$DEPLOY_DIR/docker-compose.yml" --env-file "$ENV_FILE" up -d
    else
        printf '[DRYRUN] would run: docker compose -f %s/docker-compose.yml up -d\n' "$DEPLOY_DIR"
    fi

    # ── 6. Install fleet LaunchAgent plists ────────────────────────────────
    #
    # Cadences (spec §9 step 15):
    #   heartbeat  → StartInterval 60   (every minute, dead-man's-switch)
    #   sync       → StartInterval 300  (offset: 0s via sleep in wrapper)
    #   enroll     → StartInterval 300  (offset: 30s  — enroll after sync is 1 cycle, so 30s stagger)
    #   probe      → StartInterval 300  (offset: 60s  — probe after sync settles)
    #   cf-sync    → StartInterval 900  (offset: 120s — less frequent CF pull)
    #   export     → chained via WatchPaths on fleet.yaml (written by sync/probe/cf-sync)
    #
    # Boot order: stack up (step 5 above) → sync → enroll → probe/cf-sync → export
    # Stagger for 300s-interval agents via a wrapper script with a sleep so they don't
    # all fire simultaneously on login.
    #
    # NB: fleet serve LaunchAgent is Task 18; NOT installed here.

    _write_fleet_plist() {
        local label="$1"
        local verb="$2"
        local interval="$3"
        local stagger_sleep="${4:-0}"
        local plist_path="$LAUNCHAGENTS_DIR/${label}.plist"
        local log_base="/tmp/${label}"

        # Build the program arguments array entries.
        # If a stagger_sleep > 0, wrap in a shell one-liner so the delay is
        # encoded in the plist itself (no external wrapper script needed).
        local prog_array
        if [[ "$stagger_sleep" -gt 0 ]]; then
            prog_array="    <string>/bin/sh</string>
    <string>-c</string>
    <string>sleep ${stagger_sleep} &amp;&amp; ${FLEET_BIN_PATH} ${verb}</string>"
        else
            prog_array="    <string>${FLEET_BIN_PATH}</string>
    <string>${verb}</string>"
        fi

        cat > "$plist_path" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${label}</string>

  <key>ProgramArguments</key>
  <array>
${prog_array}
  </array>

  <key>StartInterval</key>
  <integer>${interval}</integer>

  <key>RunAtLoad</key>
  <true/>

  <key>WorkingDirectory</key>
  <string>${ROOT_DIR}</string>

  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>

  <key>StandardOutPath</key>
  <string>${log_base}.log</string>
  <key>StandardErrorPath</key>
  <string>${log_base}.error.log</string>
</dict>
</plist>
PLIST
        printf 'Wrote plist: %s\n' "$plist_path"
    }

    # heartbeat — every 60s (the one that must NOT depend on Keychain, §6)
    _write_fleet_plist \
        "com.caguabot.fleet.heartbeat" \
        "heartbeat" \
        "60" \
        "0"

    # sync — every 300s, no stagger (boots first among the 300s group)
    _write_fleet_plist \
        "com.caguabot.fleet.sync" \
        "sync" \
        "300" \
        "0"

    # enroll — every 300s, 30s stagger (enroll after sync has written nodes)
    _write_fleet_plist \
        "com.caguabot.fleet.enroll" \
        "enroll" \
        "300" \
        "30"

    # probe — every 300s, 60s stagger (probe after sync settles)
    _write_fleet_plist \
        "com.caguabot.fleet.probe" \
        "probe" \
        "300" \
        "60"

    # cf-sync — every 900s, 120s stagger
    _write_fleet_plist \
        "com.caguabot.fleet.cf-sync" \
        "cf-sync" \
        "900" \
        "120"

    # export — chained after sync/probe/cf-sync via WatchPaths on the fleet.yaml snapshot.
    # When fleet sync/probe/cf-sync complete they update fleet.yaml; launchd fires export.
    # Fallback interval 300s so it runs periodically even if WatchPaths misses an update.
    FLEET_YAML="${ROOT_DIR}/fleet.yaml"
    EXPORT_PLIST="$LAUNCHAGENTS_DIR/com.caguabot.fleet.export.plist"
    cat > "$EXPORT_PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.caguabot.fleet.export</string>

  <key>ProgramArguments</key>
  <array>
    <string>${FLEET_BIN_PATH}</string>
    <string>export</string>
  </array>

  <key>WatchPaths</key>
  <array>
    <string>${FLEET_YAML}</string>
  </array>

  <key>StartInterval</key>
  <integer>300</integer>

  <key>WorkingDirectory</key>
  <string>${ROOT_DIR}</string>

  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>

  <key>StandardOutPath</key>
  <string>/tmp/com.caguabot.fleet.export.log</string>
  <key>StandardErrorPath</key>
  <string>/tmp/com.caguabot.fleet.export.error.log</string>
</dict>
</plist>
PLIST
    printf 'Wrote plist: %s\n' "$EXPORT_PLIST"

    # ── 7. Load all fleet LaunchAgents (skip in dryrun) ────────────────────

    FLEET_PLISTS=(
        "com.caguabot.fleet.heartbeat.plist"
        "com.caguabot.fleet.sync.plist"
        "com.caguabot.fleet.enroll.plist"
        "com.caguabot.fleet.probe.plist"
        "com.caguabot.fleet.cf-sync.plist"
        "com.caguabot.fleet.export.plist"
    )

    if [[ "${FLEET_INSTALL_DRYRUN:-0}" != "1" ]]; then
        for p in "${FLEET_PLISTS[@]}"; do
            ppath="$LAUNCHAGENTS_DIR/$p"
            launchctl unload "$ppath" >/dev/null 2>&1 || true
            launchctl load "$ppath"
            printf 'LaunchAgent loaded: %s\n' "$ppath"
        done
    else
        for p in "${FLEET_PLISTS[@]}"; do
            ppath="$LAUNCHAGENTS_DIR/$p"
            printf '[DRYRUN] would load: %s\n' "$ppath"
        done
    fi

    printf '\nFleet install complete.\n'
    printf '  Binary : %s\n' "$FLEET_BIN_PATH"
    printf '  Config : %s\n' "$ROOT_DIR/fleet.toml"
    printf '  DB     : ~/.local/state/fleet/fleet.db\n'
    printf '  Deploy : %s\n' "$ENV_FILE"
    printf '\nSee deploy/README.md for the boot/schedule table and secrets rotation runbook.\n'
}

# ─────────────────────────────────────────────────────────────────────────────
# DISPATCH
# ─────────────────────────────────────────────────────────────────────────────

case "$MODE" in
    --fleet)
        install_fleet
        ;;
    "")
        install_menubar
        ;;
    *)
        printf 'Usage: %s [--fleet]\n' "$0" >&2
        exit 1
        ;;
esac

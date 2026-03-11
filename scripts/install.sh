#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
APP_DIR="$HOME/Applications/MiniMonitor"
BIN_DIR="$APP_DIR/bin"
BIN_PATH="$BIN_DIR/minimonitor"
PLIST_PATH="$HOME/Library/LaunchAgents/com.caguabot.minimonitor.plist"

mkdir -p "$BIN_DIR" "$HOME/Library/LaunchAgents"

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

launchctl unload "$PLIST_PATH" >/dev/null 2>&1 || true
launchctl load "$PLIST_PATH"

printf 'Installed MiniMonitor to %s\n' "$BIN_PATH"
printf 'LaunchAgent loaded from %s\n' "$PLIST_PATH"

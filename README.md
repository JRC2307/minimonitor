# MiniMonitor

Minimal macOS menu bar activity monitor in Rust.

## What it does

- Lives in the macOS status bar instead of staying pinned on screen
- Shows live summary text like `52% RAM 23% CPU AI2`
- Opens a native tray menu with snapshot-based process and AI workload summaries
- Opens an on-demand inspector window with search, process details, localhost filtering, and token tools
- Hides background noise under `0.2%` CPU and `20 MB` RAM so the list stays minimal
- Infers likely AI runtimes and agent tools from local process names and command lines
- Estimates tokens locally for pasted text without sign-in
- Supports `Cmd+V` into the token field and includes explicit `Paste` and `Clear` controls
- Lets you store OpenAI and Anthropic API keys in macOS Keychain and validate connectivity
- Uses a confirmation step before killing any process
- Uses a generated `MM` app/tray icon

## Run

```bash
cargo run
```

## Build release

```bash
cargo build --release
```

## Install for login startup

This installs the compiled binary into a stable user app path and loads a LaunchAgent:

```bash
./scripts/install.sh
```

Installed paths:

- Binary: `~/Applications/MiniMonitor/bin/minimonitor`
- LaunchAgent: `~/Library/LaunchAgents/com.caguabot.minimonitor.plist`

## Inspector

Use the tray menu item `Show Inspector` to open the richer window.

The inspector:

- Freezes on a snapshot until you manually refresh it
- Lets you search by process name or PID
- Highlights current-user and localhost listener processes
- Shows inferred AI workloads separately from the raw process list
- Includes a manual token checker panel
- Includes provider key management for OpenAI and Anthropic
- Keeps the process table compact by hiding full command text

## Token checker

- Local estimation does not require sign-in
- OpenAI models use an OpenAI-compatible tokenizer when available
- Claude/Anthropic estimates are heuristic in v1
- The token input includes `Paste` and `Clear` controls for quick reuse
- Provider validation requires an API key stored in Keychain
- Provider usage/quota totals are not available in v1, so validation currently reports connectivity only

## Run at login

Preferred path:

```bash
./scripts/install.sh
```

The checked-in `launchd/com.caguabot.minimonitor.plist` is the reference template and already points at the stable install location under `~/Applications/MiniMonitor/`.

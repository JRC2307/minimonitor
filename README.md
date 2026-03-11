# MiniMonitor

Minimal macOS menu bar activity monitor in Rust.

## What it does

- Lives in the macOS status bar instead of staying pinned on screen
- Shows live summary text like `52% RAM 23% CPU AI2`
- Opens a native tray menu with snapshot-based process and AI workload summaries
- Opens an on-demand inspector window with search, process details, localhost filtering, and token tools
- Infers likely AI runtimes and agent tools from local process names and command lines
- Estimates tokens locally for pasted text without sign-in
- Lets you store OpenAI and Anthropic API keys in macOS Keychain and validate connectivity
- Uses a confirmation step before killing any process

## Run

```bash
cargo run
```

## Build release

```bash
cargo build --release
```

## Inspector

Use the tray menu item `Show Inspector` to open the richer window.

The inspector:

- Freezes on a snapshot until you manually refresh it
- Lets you search by process name, command, or PID
- Highlights current-user and localhost listener processes
- Shows inferred AI workloads separately from the raw process list
- Includes a manual token checker panel
- Includes provider key management for OpenAI and Anthropic

## Token checker

- Local estimation does not require sign-in
- OpenAI models use an OpenAI-compatible tokenizer when available
- Claude/Anthropic estimates are heuristic in v1
- Provider validation requires an API key stored in Keychain
- Provider usage/quota totals are not available in v1, so validation currently reports connectivity only

## Run at login

Build the release binary first, then copy `launchd/com.caguabot.minimonitor.plist` into `~/Library/LaunchAgents/` and load it:

```bash
cp launchd/com.caguabot.minimonitor.plist ~/Library/LaunchAgents/
launchctl load ~/Library/LaunchAgents/com.caguabot.minimonitor.plist
```

# Port → Service Naming — Design

**Date:** 2026-06-22
**Branch:** `fleet-phase-0-1` (builds directly on the host-snapshots feature; `/ports` page exists only here)
**Status:** Approved design → ready for plan

## Problem

The `/ports` page (and the `/node` host section) shows each listening port mapped to a
**process name pulled from `lsof`**, which on macOS is truncated to 9 chars and is usually a
generic runtime, not the app:

| Port | Shown today | What it actually is |
|---|---|---|
| 8789 | `python3.1` | cuentas |
| 8787 | `python3.1` | command-center (consulting) |
| 3006 | `python3.1` | tradingbot panel |
| 4321 | `node` | javierr (Astro dev) |
| 3030 | `com.docke` | uptime-kuma (docker-proxied) |

A port list you can't read by app name is not insightful. We want each port to resolve to the
**friendly name of the app it belongs to**, automatically where possible and via a small curated
override file for the rest.

## Key insight (no new data needed)

Every snapshot already stores a full `processes` list in `snapshot_json`, each entry carrying the
**complete command line** (`command`) and `pid`. The `host_port` rows carry `pid`. So we can join
`port.pid → process.command` and the command almost always reveals the real app via its path:

```
8789  →  …/projects/experiments/cuentas/.venv/…           → "cuentas"
8787  →  …/projects/client/consulting/.venv/… --port 8787 → "consulting"
3006  →  …/experiments/tradingbot/panel/server.py         → "tradingbot"
4321  →  …/personal/javierr/web/…astro dev                → "javierr"
4096  →  opencode web --port 4096                         → "opencode"
```

No DB migration, no schema change, no recollection — the data is already on disk.

## Architecture

### Resolution happens at read time, in `serve`

When `/ports` (or `/node`) renders, the handler already loads the latest snapshot for the node.
It builds a `pid → command` map from that snapshot's `processes` list, then runs each port row
through a **pure resolver** to produce a `service` label.

Chosen over storing a `service` column in `host_port` because:

- **No migration**, and it works on already-collected data.
- **The labels file is live** — edit it, refresh the page, names update. No `collect` re-run.
- The resolver is a pure function, trivially unit-testable in isolation.

Cost: building one pid map per page render (a few dozen rows). Negligible.

### The resolver

```
resolve_service(port: u16, command: Option<&str>, process: &str, labels: &Labels) -> String
```

**Resolution order — first match wins:**

1. **Labels override** — `labels.get(port)`. Highest priority. Curated, always wins.
2. **Command path** — extract the segment immediately after `projects/<type>/` in `command`,
   where `<type>` ∈ {`startup`, `client`, `personal`, `experiments`, `tools`}. That segment is
   the project name (`cuentas`, `tradingbot`, `javierr`, …). For `client/<client>/<project>/`,
   take the last path segment before the project's own files — i.e. the directory under the type
   that is followed by more path. Keep it simple: the first segment after the matched type token.
3. **Binary name** — if `command`'s argv[0] basename is a real binary, not a generic runtime
   (`python`, `python3`, `python3.x`, `Python`, `node`, `ruby`, `sh`, `bash`), use that basename
   (`opencode`, `ttyd`, `rustdesk`). Strip a `.app`-style framework path → basename only.
4. **Fallback** — the existing `process` string (today's behavior). Guarantees we are never worse
   than the current page.

A port whose `pid` has no matching process in the snapshot (race, or proxied) goes straight to
tier 1 then tier 4 (no command to parse) — i.e. label override or raw `process`.

### Labels file

- Path: `~/.config/fleet/service-labels.toml` (sits beside `fleet.toml`).
- Format: `port = "name"` under a `[ports]` table:
  ```toml
  [ports]
  3030 = "uptime-kuma"
  8090 = "beszel-hub"
  5432 = "paros-postgres"
  5433 = "dinara-fuzz-pg"
  21115 = "rustdesk-server"
  ```
- **Missing file ⇒ empty labels** (auto-derivation only). Never an error.
- Seeded once from `reboot-services-checklist.md` for the docker-proxied / unparseable ports that
  auto-derivation can't reach. Committed to the repo as `service-labels.example.toml`; the live
  file is the user's to edit.
- Loaded once into `AppState` at `serve` startup. (Live-edit means restart `serve`, or — stretch,
  not in scope — stat-on-render. Scope: load at startup.)

### UI

- `/ports`: add a **Service** column (the resolved name, primary/bold). Keep the raw `process` and
  `pid` visible but de-emphasized (smaller/muted) so ground truth is never lost.
- `/node` host section's port list: same resolved name, same treatment.
- Sort/grouping unchanged.

## Components & boundaries

| Unit | Responsibility | Depends on |
|---|---|---|
| `service_label` module (new, in `fleet`) | pure `resolve_service` + `Labels` type + TOML load | nothing (std + serde/toml) |
| serve `/ports` + `/node` handlers | build pid→command map, call resolver, pass labels from AppState | `service_label`, existing snapshot read |
| `AppState` | hold loaded `Labels` | `service_label` |
| `service-labels.example.toml` (new) | seed values | — |

## Testing

Resolver is pure → table-driven unit tests:

- **Tier 1:** override present for a port → override wins even when a command would derive something else.
- **Tier 2:** path extraction for each project type (`experiments/cuentas`, `client/consulting`,
  `personal/javierr`, `startup/x`, `tools/y`) → correct project name.
- **Tier 3:** generic runtime + no project path → binary basename (`opencode web …` → `opencode`).
- **Tier 3 negative:** `python … server.py` with no `projects/` path and a generic argv[0] → does
  NOT return `python`; falls through to tier 4.
- **Tier 4:** unparseable / empty command → raw `process` string.
- **No-pid:** port with no matching process → tier 1 or tier 4 only.
- **Labels load:** missing file → empty labels (no error); malformed → surfaced as load error at
  startup (fail loud at boot, not per-request).

Serve test: render `/ports` against a fixture snapshot, assert the Service column shows a derived
name (e.g. `cuentas`) and the raw process is still present.

## Out of scope

- Live re-read of the labels file per request (startup load only).
- Editing labels from the UI.
- Resolving names for remote nodes differently — the resolver is host-agnostic; same code path.
- Any change to the agent or the collection pipeline.

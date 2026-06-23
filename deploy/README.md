# deploy/ — Fleet Observability Stack

Docker-based observability services for the Intel Mac mini fleet hub.  
All images are `linux/amd64` (Intel host), all pinned, all ports bound to the
host's Tailscale `100.x` IP only (never `0.0.0.0`).

---

## Boot / Schedule Table

| Step | What | How | Cadence |
|------|------|-----|---------|
| 1 | **Docker stack up** | `docker compose -f deploy/docker-compose.yml up -d` | Once at install; survives reboots via `restart: unless-stopped` |
| 2 | **`fleet sync`** | LaunchAgent `com.caguabot.fleet.sync` | Every **300 s** (offset: 0 s) |
| 3 | **`fleet enroll`** | LaunchAgent `com.caguabot.fleet.enroll` | Every **300 s** (offset: +30 s — runs after sync settles) |
| 4 | **`fleet probe`** | LaunchAgent `com.caguabot.fleet.probe` | Every **300 s** (offset: +60 s — runs after sync settles) |
| 5 | **`fleet cf-sync`** | LaunchAgent `com.caguabot.fleet.cf-sync` | Every **900 s** (offset: +120 s) |
| 6 | **`fleet export`** | LaunchAgent `com.caguabot.fleet.export` | Triggered by `WatchPaths` on `fleet.yaml` + fallback every **300 s** |
| — | **`fleet heartbeat`** | LaunchAgent `com.caguabot.fleet.heartbeat` | Every **60 s** — external dead-man's-switch ping to hc-ping.com |

> `fleet serve` LaunchAgent (`KeepAlive`, web UI on `:8099`) is installed alongside the other agents in step 6 of `install.sh --fleet`.

### Boot order details

The offsets encode the intended ordering without rigid sequencing:

```
boot
 └─ docker compose up -d      (stack: Beszel, Kuma, ntfy, cloudflared)
 └─ fleet sync     [0s offset]   ← pull registry from Tailscale API
 └─ fleet enroll   [+30s]        ← register new nodes in Beszel + Kuma
 └─ fleet probe    [+60s]        ← MTR path traces (native, not in Docker)
 └─ fleet cf-sync  [+120s]       ← read-only Cloudflare zones + SSL expiry
 └─ fleet export   [WatchPaths]  ← write fleet.yaml snapshot (git-tracked)
```

`fleet heartbeat` runs independently every 60 s and must never depend on the
Keychain — its `FLEET_HC_PING_KEY` is resolved from env first (spec §6 / R-8).

---

## Ports

| Service | Bind | Notes |
|---------|------|-------|
| Beszel hub | `${HOST_TS_IP}:8090` | Tailnet-only; agents push outbound |
| Uptime-Kuma | `${HOST_TS_IP}:3001` | Tailnet-only |
| ntfy | `${HOST_TS_IP}:8082` | Tailnet-only; phone must be on tailnet |
| cloudflared | (outbound only) | No published port; tunnels `fleet serve` |
| `fleet serve` | `${HOST_TS_IP}:8099` | Native LaunchAgent (Task 18), not a container |

---

## Cloudflare Access (Zero-Trust) setup

`fleet serve` binds `${HOST_TS_IP}:8099` on the tailnet only.  For off-tailnet
access (operator phone, etc.) a named `cloudflared` tunnel fronted by Cloudflare
Access (Zero Trust) exposes `fleet.<domain>` without opening any public inbound port.

### 1. Create the named tunnel

```bash
# On the Intel mini, authenticated to the Cloudflare account that owns <domain>
cloudflared tunnel create fleet-serve
# Copy the tunnel UUID from the output
```

In the Zero Trust dashboard (**Networks → Tunnels**) the tunnel will appear.  Go to
its **Configure → Public Hostname** tab and add one hostname rule:

| Field | Value |
|-------|-------|
| Subdomain | `fleet` |
| Domain | `<domain>` (your CF-managed zone) |
| Service type | HTTP |
| URL | `http://${HOST_TS_IP}:8099` |

> Replace `${HOST_TS_IP}` with the Intel mini's actual Tailscale `100.x` IP
> (e.g. `http://100.71.2.3:8099`).  The tunnel connects **outbound** from the
> mini to Cloudflare — no inbound port is opened.

### 2. Attach a Cloudflare Access application (operator-only policy)

1. **Zero Trust dashboard → Access → Applications → Add an application →
   Self-hosted.**
2. **Application domain:** `fleet.<domain>` (must match the tunnel hostname above).
3. **Session duration:** 24h (adjust to taste).
4. **Policy — operator only:**
   - Policy name: `operator-only`
   - Action: Allow
   - Include rule: `Emails` → `<your-operator-email>` (or use a GitHub / Google
     identity provider with your account's email).
   - No other identities allowed — this is a single-operator gate.
5. Save.  From this point any browser hitting `https://fleet.<domain>` must
   authenticate through Cloudflare Access before reaching `fleet serve`.

### 3. Add the tunnel token to deploy/.env

After creating the tunnel, generate a token:

```bash
cloudflared tunnel token fleet-serve
```

Add the output to `deploy/.env` (never commit this file — it is git-ignored):

```
FLEET_CF_TUNNEL_TOKEN=<token-from-above>
```

The `cloudflared` compose service reads this variable and connects to Cloudflare
automatically on `docker compose up`.

### Security posture

- **No public port.** The cloudflared tunnel is outbound-only.  `fleet serve`
  binds `${HOST_TS_IP}:8099` (tailnet IP, never `0.0.0.0`).
- **Two-layer gate:** tailnet ACLs protect the direct `:8099` path; Cloudflare
  Access guards the `fleet.<domain>` path.  An attacker who bypasses Access still
  needs to be on the tailnet.
- **Zero Trust policy:** only the named operator identity (email/device posture)
  can pass the Access gate — not "anyone with a Cloudflare account".

### Rotation

See the **Secrets → Rotation runbook** section below for `FLEET_CF_TUNNEL_TOKEN`
rotation steps (`docker compose restart cloudflared` after updating `.env`).

---

## First-time setup

```bash
# 1. Copy .env and fill in secrets
cp deploy/.env.example deploy/.env
chmod 600 deploy/.env
$EDITOR deploy/.env

# 2. Run install (builds fleet binary, runs doctor preflight, starts stack)
bash scripts/install.sh --fleet
```

`install.sh --fleet` does, in order:
1. Builds `fleet` (`cargo build --release -p fleet`)
2. Runs `fleet doctor` preflight (bind-address + secret-resolvability checks)
3. Reads `tailscale ip -4` — **hard-fails on empty** (never defaults to `0.0.0.0`)
4. Writes `deploy/.env` with `HOST_TS_IP=<100.x>`
5. `docker compose up -d`
6. Writes + loads all fleet LaunchAgent plists

---

## Secrets

| Secret | Consumer | Store |
|--------|----------|-------|
| `HOST_TS_IP` | compose stack | `deploy/.env` (written at install from `tailscale ip -4`) |
| `FLEET_CF_TUNNEL_TOKEN` | cloudflared container | `deploy/.env` |
| Tailscale OAuth secrets | `fleet` CLI | macOS Keychain or `FLEET_*` env |
| Beszel password | `fleet` CLI | macOS Keychain or `FLEET_BESZEL_PASSWORD` env |
| CF token (read-only) | `fleet` CLI | macOS Keychain or `FLEET_CF_TOKEN` env |
| ntfy token | `fleet` CLI + Beszel + Kuma | macOS Keychain (`fleet`) + `deploy/.env` (containers) |
| hc.io ping key | `fleet heartbeat` | **Env only** (`FLEET_HC_PING_KEY`) — must not require Keychain |

`deploy/.env` is **git-ignored** (`chmod 600`). Never commit it.

### Rotation runbook

| Token | Where to rotate |
|-------|----------------|
| ntfy token | 1. Generate new token in ntfy admin UI. 2. Update Keychain: `security add-generic-password -s fleet-ntfy-token -a fleet -w <new>`. 3. Update `deploy/.env` (`FLEET_NTFY_TOKEN`). 4. Update Beszel UI → Notifications → ntfy token. 5. Update Kuma UI → Notifications → ntfy token. 6. `docker compose restart ntfy`. |
| Tailscale OAuth secret | Rotate the client secret in the Tailscale admin console. Update Keychain only (no compose restart needed). |
| CF API token (read-only) | Revoke old token in Cloudflare dashboard. Create a new one with the same scopes (`Zone:Read`, `SSL and Certificates:Read`). Update Keychain + `deploy/.env`. |
| `FLEET_CF_TUNNEL_TOKEN` | Recreate the tunnel token in the CF Zero Trust dashboard. Update `deploy/.env`. `docker compose restart cloudflared`. |
| hc.io ping key | Recreate the ping key in healthchecks.io. Update `FLEET_HC_PING_KEY` env / LaunchAgent env override. |

> **sops/age not adopted** — net-negative key-management overhead for a solo operator; deferred to the plane-6 secrets decision (spec §7).

---

## Updating pinned images

All images are pinned (see `docker-compose.yml` comments). To bump a pin:

1. Update the tag in `docker-compose.yml`.
2. Re-record wiremock fixtures if the API shape changed (Beszel, Kuma).
3. Run `cargo test --workspace` to verify fixture compatibility.
4. `docker compose pull && docker compose up -d`.

---

## Logs

Fleet LaunchAgent logs land in `/tmp/com.caguabot.fleet.<verb>.{log,error.log}`.

Docker stack logs: `docker compose -f deploy/docker-compose.yml logs -f`.

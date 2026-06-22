# Beszel Agent — Per-Box Rollout

Deploy this compose on every **owned box** to wire it into the Beszel hub running on the Intel mini.

## Push-through-NAT rationale

Beszel supports two enrollment models:

| Model | How the hub contacts the agent | Works behind NAT? |
|-------|-------------------------------|-------------------|
| SSH-key | Hub SSHes inbound to each agent box | **No** — fails if agent is behind NAT/CGNAT |
| Universal token / WebSocket (push) | Agent connects OUTBOUND to the hub | **Yes** — outbound WS through any NAT |

This fleet uses the **push model** (universal token). The agent opens an outbound WebSocket to the hub at `${INTEL_MINI_HOST}:8090` and self-registers. No inbound port is published on the agent box. No SSH key is needed.

> **Do NOT run Beszel's interactive `install-agent.sh` script.** Even when invoked with a
> universal token, the script demands an SSH key and attempts inbound connectivity. Use this
> compose file directly instead.

## Rollout steps

Repeat for each owned box:

1. **Copy this directory** (`deploy/agent/`) to the target box — or clone the repo and navigate here.

2. **Create a `.env` file** alongside `docker-compose.yml`:

   ```
   INTEL_MINI_HOST=100.x.x.x          # Tailscale IP of the Intel mini hub
   BESZEL_BOOTSTRAP_TOKEN=<token>      # One-time bootstrap token from the Beszel hub UI
   ```

   To get the bootstrap token: open the Beszel hub UI → Settings → "Add system" → select
   "Universal token" → copy the token. The token is only needed during initial enrollment;
   once the agent self-registers, it uses a persistent per-agent credential.

3. **Start the agent:**

   ```bash
   docker compose up -d
   ```

4. **Confirm the agent appears in the Beszel hub** (Settings → Systems). Note the
   exact `name` and `host` values the agent self-registers — see the section below.

5. **Disable the bootstrap token** in the Beszel hub UI after all agents are enrolled
   (Settings → Universal token → Disable). The token is one-time-bootstrap only; leaving
   it enabled is a security risk (anyone with the token can register a rogue agent).

6. **Update the registry** (`fleet sync`) so the enrolled agent's system ID is recorded
   and `fleet enroll` knows not to re-enroll it.

---

## Self-registered identity (confirm at deploy)

> **This section is a residual open question (Spec Q2 / Task-11).** The exact `host` and
> `name` fields that `henrygd/beszel-agent:0.9.1` reports on first self-registration
> **must be verified against a live agent** before Task-11's enroll match key can be
> trusted. Do not assume — record what you observe here.

Task-11's `fleet enroll` matches a registry `FleetNode` to a Beszel `systems` record using:

```
node.tailnet_ip == beszel_system.host
```

Where `beszel_system.host` is the `host` field the agent POSTs during self-registration.

**To verify the identity the agent reports:**

After starting the agent on a box and confirming it appears in the Beszel hub UI, query
the hub's PocketBase API to inspect the raw `systems` record:

```bash
# Replace HUB with the Intel mini's tailnet IP and TOKEN with your PocketBase admin token
curl -s "http://${INTEL_MINI_HOST}:8090/api/collections/systems/records" \
  -H "Authorization: Bearer <pb-admin-token>" | jq '.items[] | {id, name, host, status}'
```

Record the output here before trusting `fleet enroll`:

| Box hostname | Tailscale IP | `name` reported by agent | `host` reported by agent | Verified |
|---|---|---|---|---|
| _(fill in)_ | _(fill in)_ | _(fill in)_ | _(fill in)_ | [ ] |

If `host` is NOT the Tailscale `100.x` IP (e.g. it reports a LAN IP or the box hostname),
update the `enroll` match logic in `crates/fleet/src/enroll.rs` before shipping Task-11.

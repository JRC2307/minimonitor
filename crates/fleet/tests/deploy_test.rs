//! Task-13 / Task-14 integration tests — Docker stack file validation.
//!
//! Parse-only: no live containers, no networking.
//! Task-13 tests read `deploy/docker-compose.yml` (hub compose, Intel mini).
//! Task-14 tests read `deploy/agent/docker-compose.yml` (per-box agent compose).

use fleet::doctor::check_compose_binds;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Resolve a path relative to the workspace root (the directory that contains
/// the top-level `Cargo.toml`).  Works regardless of which crate or worktree
/// the test binary is built from, because `CARGO_MANIFEST_DIR` points at the
/// *crate*'s manifest — so we walk up two levels (crates/fleet → repo root).
fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = .../crates/fleet  → parent = crates → parent = root
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("crates/")
        .parent()
        .expect("workspace root")
        .to_owned()
}

fn compose_path() -> PathBuf {
    workspace_root().join("deploy").join("docker-compose.yml")
}

fn compose_text() -> String {
    std::fs::read_to_string(compose_path()).expect("deploy/docker-compose.yml must exist")
}

// ─── Minimal serde model matching compose shape ───────────────────────────────

#[derive(Debug, Deserialize)]
struct ComposeFile {
    #[serde(default)]
    services: HashMap<String, ComposeService>,
}

#[derive(Debug, Deserialize, Default)]
struct ComposeService {
    image: Option<String>,
    #[serde(default)]
    ports: Vec<serde_yaml_ng::Value>,
    #[serde(default)]
    cap_add: Vec<String>,
    #[serde(default)]
    environment: serde_yaml_ng::Value,
}

fn parse_compose() -> ComposeFile {
    let text = compose_text();
    serde_yaml_ng::from_str(&text).expect("compose YAML must parse")
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Every published port must be templated as `${HOST_TS_IP}:PORT:PORT`.
/// No literal `0.0.0.0` and no bare `HOST_PORT:CONTAINER_PORT` binds allowed.
/// Also asserts that the pinned image tags match the spec exactly.
#[test]
fn compose_binds_tailnet_only() {
    let text = compose_text();
    let compose = parse_compose();

    // --- port binding assertions ---
    for (svc_name, svc) in &compose.services {
        for port_val in &svc.ports {
            let port_str = match port_val {
                serde_yaml_ng::Value::String(s) => s.clone(),
                serde_yaml_ng::Value::Number(n) => {
                    panic!(
                        "service `{svc_name}`: bare integer port `{n}` — must use \
                         ${{HOST_TS_IP}}:PORT:PORT form"
                    );
                }
                other => panic!("service `{svc_name}`: unexpected port format: {other:?}"),
            };

            // Must start with the template variable, never a literal IP or bare port
            assert!(
                port_str.starts_with("${HOST_TS_IP}:"),
                "service `{svc_name}`: port `{port_str}` is not bound to ${{HOST_TS_IP}} \
                 (wildcard or bare bind)"
            );

            // Belt-and-suspenders: no literal 0.0.0.0
            assert!(
                !port_str.contains("0.0.0.0"),
                "service `{svc_name}`: port `{port_str}` contains literal 0.0.0.0"
            );
        }
    }

    // No literal 0.0.0.0 anywhere in the file
    assert!(
        !text.contains("0.0.0.0"),
        "compose file contains literal 0.0.0.0"
    );

    // --- pinned image tags ---
    let beszel = compose
        .services
        .get("beszel")
        .expect("beszel service must exist");
    assert_eq!(
        beszel.image.as_deref().unwrap_or(""),
        "henrygd/beszel:0.9.1",
        "beszel image tag must be pinned to 0.9.1"
    );

    let kuma = compose
        .services
        .get("uptime-kuma")
        .expect("uptime-kuma service must exist");
    assert_eq!(
        kuma.image.as_deref().unwrap_or(""),
        "louislam/uptime-kuma:1.23.16",
        "uptime-kuma image tag must be pinned to 1.23.16"
    );

    let ntfy = compose
        .services
        .get("ntfy")
        .expect("ntfy service must exist");
    assert_eq!(
        ntfy.image.as_deref().unwrap_or(""),
        "binwiederhier/ntfy:v2.11.0",
        "ntfy image tag must be pinned to v2.11.0"
    );

    let cf = compose
        .services
        .get("cloudflared")
        .expect("cloudflared service must exist");
    let cf_image = cf.image.as_deref().unwrap_or("");
    assert!(
        cf_image.starts_with("cloudflare/cloudflared:"),
        "cloudflared image must be from cloudflare/cloudflared with a pinned tag; got: `{cf_image}`"
    );
    let cf_tag = cf_image.trim_start_matches("cloudflare/cloudflared:");
    assert!(
        !cf_tag.is_empty() && cf_tag != "latest",
        "cloudflared tag must be pinned (not empty/latest); got: `{cf_tag}`"
    );
    // Must be a 2024.x or later dated release tag (YYYY.MM.D format)
    assert!(
        cf_tag.starts_with("202"),
        "cloudflared tag must be a dated release like 2024.x.y; got: `{cf_tag}`"
    );
}

/// Uptime-Kuma service must have `NET_RAW` in `cap_add` (needed for ICMP ping monitors).
#[test]
fn compose_kuma_has_net_raw() {
    let compose = parse_compose();
    let kuma = compose
        .services
        .get("uptime-kuma")
        .expect("uptime-kuma service must exist");
    assert!(
        kuma.cap_add.iter().any(|c| c == "NET_RAW"),
        "uptime-kuma must have NET_RAW in cap_add (needed for ICMP ping); \
         got cap_add: {:?}",
        kuma.cap_add
    );
}

/// No homepage/gethomepage service in compose, and no services.yaml under deploy/.
#[test]
fn compose_has_no_homepage() {
    let compose = parse_compose();

    // No service named "homepage" or containing "gethomepage"
    for svc_name in compose.services.keys() {
        assert!(
            svc_name != "homepage",
            "compose must not have a 'homepage' service (Homepage is replaced by `fleet serve`)"
        );
        if let Some(svc) = compose.services.get(svc_name)
            && let Some(img) = &svc.image
        {
            assert!(
                !img.contains("gethomepage"),
                "service `{svc_name}` image `{img}` references gethomepage — \
                 Homepage is dropped; use `fleet serve` instead"
            );
        }
    }

    // No services.yaml file under deploy/
    let services_yaml = workspace_root().join("deploy").join("services.yaml");
    assert!(
        !services_yaml.exists(),
        "deploy/services.yaml must not exist (Homepage is replaced by `fleet serve`)"
    );
}

/// cloudflared service: publishes no port (outbound tunnel only), reads its
/// token from `${FLEET_CF_TUNNEL_TOKEN}` — never a literal token value.
#[test]
fn compose_cloudflared_no_published_port() {
    let text = compose_text();
    let compose = parse_compose();

    let cf = compose
        .services
        .get("cloudflared")
        .expect("cloudflared service must exist");

    // No published ports (outbound tunnel only)
    assert!(
        cf.ports.is_empty(),
        "cloudflared must not publish any port (outbound tunnel only); \
         got ports: {:?}",
        cf.ports
    );

    // Token must come from the env variable, not be a literal value
    let env_str = serde_yaml_ng::to_string(&cf.environment).unwrap_or_default();
    assert!(
        env_str.contains("FLEET_CF_TUNNEL_TOKEN"),
        "cloudflared environment must reference FLEET_CF_TUNNEL_TOKEN; \
         env block: {env_str}"
    );

    // Belt-and-suspenders: no literal token-shaped value in the file
    // Cloudflare tunnel tokens are base64url-ish, 100+ chars; a placeholder
    // like "your-token-here" should not look like one.  We check the env block
    // for ${FLEET_CF_TUNNEL_TOKEN} template form.
    assert!(
        text.contains("${FLEET_CF_TUNNEL_TOKEN}") || text.contains("FLEET_CF_TUNNEL_TOKEN"),
        "compose file must reference FLEET_CF_TUNNEL_TOKEN for the cloudflared tunnel token"
    );
}

/// Run the Task-2 doctor bind-check over the real compose file — must pass.
#[test]
fn doctor_bind_check_passes_on_real_compose() {
    let text = compose_text();
    let result = check_compose_binds(&text);
    assert!(
        result.is_ok(),
        "doctor bind-check failed on deploy/docker-compose.yml: {:?}",
        result
    );
}

// ─── Task-14: per-box agent compose assertions ────────────────────────────────

fn agent_compose_path() -> PathBuf {
    workspace_root()
        .join("deploy")
        .join("agent")
        .join("docker-compose.yml")
}

fn agent_compose_text() -> String {
    std::fs::read_to_string(agent_compose_path())
        .expect("deploy/agent/docker-compose.yml must exist")
}

#[derive(Debug, Deserialize)]
struct AgentComposeFile {
    #[serde(default)]
    services: HashMap<String, AgentComposeService>,
}

#[derive(Debug, Deserialize, Default)]
struct AgentComposeService {
    image: Option<String>,
    network_mode: Option<String>,
    #[serde(default)]
    ports: Vec<serde_yaml_ng::Value>,
    #[serde(default)]
    volumes: Vec<serde_yaml_ng::Value>,
    #[serde(default)]
    environment: serde_yaml_ng::Value,
}

fn parse_agent_compose() -> AgentComposeFile {
    let text = agent_compose_text();
    serde_yaml_ng::from_str(&text).expect("deploy/agent/docker-compose.yml YAML must parse")
}

/// Task-14: The per-box Beszel agent compose must be push-model only.
///
/// Assertions:
/// - `network_mode: host` (host metrics + outbound WS)
/// - image pinned to `henrygd/beszel-agent:0.9.1` (matches hub)
/// - `TOKEN: ${BESZEL_BOOTSTRAP_TOKEN}` in environment
/// - NO published ports (outbound WS only — push-through-NAT)
/// - docker.sock mounted read-only (`:ro`)
#[test]
fn agent_compose_is_push_model() {
    let text = agent_compose_text();
    let compose = parse_agent_compose();

    let agent = compose
        .services
        .get("beszel-agent")
        .expect("beszel-agent service must exist in deploy/agent/docker-compose.yml");

    // network_mode: host
    assert_eq!(
        agent.network_mode.as_deref().unwrap_or(""),
        "host",
        "beszel-agent must use network_mode: host (for host metrics + outbound WS)"
    );

    // pinned image
    assert_eq!(
        agent.image.as_deref().unwrap_or(""),
        "henrygd/beszel-agent:0.9.1",
        "beszel-agent image must be pinned to henrygd/beszel-agent:0.9.1 (match the hub)"
    );

    // TOKEN env var must use the bootstrap token variable
    let env_str = serde_yaml_ng::to_string(&agent.environment).unwrap_or_default();
    assert!(
        env_str.contains("BESZEL_BOOTSTRAP_TOKEN"),
        "beszel-agent environment must reference BESZEL_BOOTSTRAP_TOKEN; env block: {env_str}"
    );
    assert!(
        text.contains("${BESZEL_BOOTSTRAP_TOKEN}"),
        "TOKEN must be templated as ${{BESZEL_BOOTSTRAP_TOKEN}} in deploy/agent/docker-compose.yml"
    );

    // NO published ports (outbound WS only — push-through-NAT)
    assert!(
        agent.ports.is_empty(),
        "beszel-agent must NOT publish any port (outbound WS push model only); \
         got ports: {:?}",
        agent.ports
    );

    // docker.sock mounted read-only
    let has_sock_ro = agent.volumes.iter().any(|v| {
        let s = match v {
            serde_yaml_ng::Value::String(s) => s.clone(),
            other => serde_yaml_ng::to_string(other).unwrap_or_default(),
        };
        s.contains("docker.sock") && s.ends_with(":ro")
    });
    assert!(
        has_sock_ro,
        "beszel-agent must mount docker.sock read-only (/var/run/docker.sock:/var/run/docker.sock:ro); \
         got volumes: {:?}",
        agent.volumes
    );
}

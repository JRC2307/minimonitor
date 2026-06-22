//! `fleet doctor` preflight checks.
//!
//! Two checks:
//! 1. **Bind-address check** — parse published ports from a compose YAML string
//!    and reject any that bind to `0.0.0.0`, an empty host, or a non-CGNAT IP.
//!    CGNAT = `100.64.0.0/10` per RFC 6598.
//!
//! 2. **Secret-resolvability check** — attempt to resolve each named secret
//!    and return the names of any that fail. Values are never returned or logged.

use anyhow::Context;
use ipnet::Ipv4Net;
use serde::Deserialize;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::sync::OnceLock;

// ─── CGNAT range ─────────────────────────────────────────────────────────────

/// `100.64.0.0/10` — the CGNAT range used by Tailscale.
fn cgnat_net() -> Ipv4Net {
    static NET: OnceLock<Ipv4Net> = OnceLock::new();
    *NET.get_or_init(|| "100.64.0.0/10".parse().unwrap())
}

fn is_cgnat(ip: Ipv4Addr) -> bool {
    cgnat_net().contains(&ip)
}

// ─── Compose YAML structures (minimal, for port extraction) ──────────────────

/// Minimal representation of a docker-compose YAML service.
#[derive(Debug, Deserialize, Default)]
struct ComposeService {
    #[serde(default)]
    ports: Vec<serde_yaml_ng::Value>,
}

/// Top-level docker-compose YAML structure.
#[derive(Debug, Deserialize)]
struct ComposeFile {
    #[serde(default)]
    services: HashMap<String, ComposeService>,
}

// ─── Bind-address check ──────────────────────────────────────────────────────

/// Parse a docker-compose YAML string, extract all published port host-bind IPs,
/// and return an error if any bind is invalid (0.0.0.0, no host IP, or non-CGNAT).
///
/// Accepts port strings in the forms:
/// - `"HOST_IP:HOST_PORT:CONTAINER_PORT"` → host bind is `HOST_IP`
/// - `"HOST_PORT:CONTAINER_PORT"` → no explicit host IP → treated as wildcard → error
/// - Integer (short form) → no host IP → error
pub fn check_compose_binds(yaml: &str) -> anyhow::Result<()> {
    let compose: ComposeFile =
        serde_yaml_ng::from_str(yaml).context("failed to parse compose YAML")?;

    let mut errors: Vec<String> = Vec::new();

    for (svc_name, service) in &compose.services {
        for port_val in &service.ports {
            let port_str = match port_val {
                serde_yaml_ng::Value::String(s) => s.clone(),
                serde_yaml_ng::Value::Number(n) => {
                    // Integer short-form: container port only, no host bind → reject
                    errors.push(format!(
                        "service `{svc_name}`: port `{n}` has no explicit host IP (implicit wildcard bind)"
                    ));
                    continue;
                }
                other => {
                    errors.push(format!(
                        "service `{svc_name}`: unexpected port format: {other:?}"
                    ));
                    continue;
                }
            };

            match validate_port_bind(svc_name, &port_str) {
                Ok(()) => {}
                Err(e) => errors.push(e),
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("bind-address check failed:\n{}", errors.join("\n"))
    }
}

/// Validate a single port string `"[HOST_IP:]HOST_PORT:CONTAINER_PORT"`.
/// Returns `Ok(())` if the host IP is in the CGNAT range, else an error message.
fn validate_port_bind(svc_name: &str, port_str: &str) -> Result<(), String> {
    let parts: Vec<&str> = port_str.splitn(3, ':').collect();
    match parts.len() {
        3 => {
            // "HOST_IP:HOST_PORT:CONTAINER_PORT"
            let host_ip_str = parts[0];
            if host_ip_str.is_empty() {
                return Err(format!(
                    "service `{svc_name}`: port `{port_str}` has empty host IP (implicit wildcard)"
                ));
            }
            if host_ip_str == "0.0.0.0" {
                return Err(format!(
                    "service `{svc_name}`: port `{port_str}` binds to 0.0.0.0 (wildcard — must bind to tailnet CGNAT IP)"
                ));
            }
            match Ipv4Addr::from_str(host_ip_str) {
                Ok(ip) => {
                    if !is_cgnat(ip) {
                        return Err(format!(
                            "service `{svc_name}`: port `{port_str}` host IP `{ip}` \
                             is not in the CGNAT range 100.64.0.0/10"
                        ));
                    }
                    Ok(())
                }
                Err(_) => {
                    // Might be a hostname/variable (e.g. `${HOST_TS_IP}`) — accept it
                    // as a deferred/template value and let install.sh validate at runtime.
                    // Only hard-reject literal IPs we can parse.
                    Ok(())
                }
            }
        }
        2 => {
            // "HOST_PORT:CONTAINER_PORT" — no host IP → implicit 0.0.0.0
            Err(format!(
                "service `{svc_name}`: port `{port_str}` has no host IP (implicit 0.0.0.0 bind)"
            ))
        }
        1 => {
            // "CONTAINER_PORT" only — implicit bind
            Err(format!(
                "service `{svc_name}`: port `{port_str}` has no host IP (implicit 0.0.0.0 bind)"
            ))
        }
        _ => Err(format!(
            "service `{svc_name}`: unexpected port format `{port_str}`"
        )),
    }
}

// ─── Secret-resolvability check ──────────────────────────────────────────────

/// Attempt to resolve each `(env_var, keychain_service)` pair using the provided
/// `keychain` function. Returns a list of **names** (not values) of secrets that
/// could not be resolved.
///
/// The names returned are the `keychain_service` names so the output is
/// informative without leaking secret values.
pub fn check_secret_resolvability<F>(secrets: &[(&str, &str)], keychain: F) -> Vec<String>
where
    F: Fn(&str) -> anyhow::Result<String> + Copy,
{
    use crate::secrets::resolve_with_keychain;

    let mut unresolved = Vec::new();
    for (env_var, keychain_svc) in secrets {
        if resolve_with_keychain(env_var, keychain_svc, keychain).is_err() {
            unresolved.push(keychain_svc.to_string());
        }
    }
    unresolved
}

// ─── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgnat_100_64_is_in_range() {
        assert!(is_cgnat("100.64.0.0".parse().unwrap()));
        assert!(is_cgnat("100.64.0.1".parse().unwrap()));
        assert!(is_cgnat("100.71.2.3".parse().unwrap()));
        assert!(is_cgnat("100.127.255.255".parse().unwrap()));
    }

    #[test]
    fn outside_cgnat_not_in_range() {
        assert!(!is_cgnat("0.0.0.0".parse().unwrap()));
        assert!(!is_cgnat("100.63.255.255".parse().unwrap()));
        assert!(!is_cgnat("100.128.0.0".parse().unwrap()));
        assert!(!is_cgnat("203.0.113.10".parse().unwrap()));
        assert!(!is_cgnat("192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn validate_cgnat_ok() {
        assert!(validate_port_bind("svc", "100.64.0.1:8090:8090").is_ok());
    }

    #[test]
    fn validate_wildcard_err() {
        assert!(validate_port_bind("svc", "0.0.0.0:8090:8090").is_err());
    }

    #[test]
    fn validate_no_host_ip_err() {
        assert!(validate_port_bind("svc", "8090:8090").is_err());
    }

    #[test]
    fn validate_public_ip_err() {
        assert!(validate_port_bind("svc", "203.0.113.1:8090:8090").is_err());
    }

    #[test]
    fn template_variable_accepted() {
        // ${HOST_TS_IP} is not a parseable IP — treat as deferred template
        assert!(validate_port_bind("svc", "${HOST_TS_IP}:8090:8090").is_ok());
    }
}

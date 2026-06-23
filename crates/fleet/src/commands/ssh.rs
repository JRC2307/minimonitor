//! `fleet ssh <target> [--user U] [--ts] [--all] [-- <cmd...>]`
//!
//! ## Security constraints (spec §3.7 / R-2)
//!
//! - Connect to a **validated `100.x` IP** (`IpAddr`) parsed from `node.addresses`.
//!   Never use `node.fqdn` or `node.hostname` — a crafted MagicDNS name can become
//!   an ssh option (e.g. `-oProxyCommand=evil`).
//! - Pass `user@IP` as **separate** argv element — not interpolated into a single string.
//! - Insert `--` separator before the host token so even a `100.x` IP starting with
//!   unusual patterns can't be misread as an option.
//! - `--ts` swaps the program to `tailscale ssh`.
//!
//! The seam `build_ssh_argv` is pure and fully tested.
//! Real `exec` only happens in the non-test command path.

use crate::db::nodes::{ResolveResult, get_by_ref};
use crate::model::Node;
use anyhow::Result;
use std::net::IpAddr;

/// Pick the first `100.x` (CGNAT `100.64.0.0/10`) address from `node.addresses`.
///
/// Returns `Err` if none exists, so the caller can surface a clear message.
pub fn pick_tailscale_ip(node: &Node) -> Result<IpAddr> {
    for addr_str in &node.addresses {
        if let Ok(ip) = addr_str.parse::<IpAddr>()
            && is_tailscale_ip(ip)
        {
            return Ok(ip);
        }
    }
    anyhow::bail!(
        "no Tailscale 100.64.0.0/10 address found in node {:?} addresses: {:?}",
        node.fleet_id,
        node.addresses
    )
}

/// Returns true if `ip` is in the CGNAT / Tailscale range `100.64.0.0/10`.
///
/// The range is 100.64.0.0 – 100.127.255.255 (i.e., second octet 64–127).
pub fn is_tailscale_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            octets[0] == 100 && octets[1] >= 64 && octets[1] <= 127
        }
        IpAddr::V6(_) => false,
    }
}

/// Build the ssh argv vector for a node.
///
/// Returns `["ssh", "--", "user@100.x.x.x"]` (or `tailscale ssh` with `--ts`).
/// The `user@IP` pair is a **single argv element** (the "@" is the normal ssh
/// `user@host` convention and is safe); the `--` before it prevents any IP
/// from being parsed as an option. The crafted fqdn/hostname never appears.
///
/// Optional `remote_cmd` args are appended after the host element.
pub fn build_ssh_argv(
    node: &Node,
    user: &str,
    use_tailscale_ssh: bool,
    remote_cmd: &[String],
) -> Result<Vec<String>> {
    let ip = pick_tailscale_ip(node)?;
    let host_token = format!("{user}@{ip}");

    let mut argv = if use_tailscale_ssh {
        vec!["tailscale".to_owned(), "ssh".to_owned()]
    } else {
        vec!["ssh".to_owned()]
    };

    argv.push("--".to_owned());
    argv.push(host_token);

    for arg in remote_cmd {
        argv.push(arg.clone());
    }

    Ok(argv)
}

/// Run `fleet ssh` for a single target.
///
/// Resolves the node, builds the argv, then **execs** ssh (never in tests).
pub fn run(
    conn: &rusqlite::Connection,
    target: &str,
    user: &str,
    use_ts: bool,
    remote_cmd: &[String],
) -> Result<()> {
    let node = resolve_single(conn, target)?;
    let argv = build_ssh_argv(&node, user, use_ts, remote_cmd)?;
    exec_ssh(argv)
}

fn resolve_single(conn: &rusqlite::Connection, target: &str) -> Result<Node> {
    match get_by_ref(conn, target)? {
        ResolveResult::Found(n) => Ok(n),
        ResolveResult::Ambiguous(candidates) => {
            eprintln!("fleet ssh: ambiguous target {:?}:", target);
            for c in &candidates {
                eprintln!("  {} ({})", c.fleet_id, c.hostname);
            }
            anyhow::bail!("ambiguous target {:?}", target)
        }
        ResolveResult::NotFound => anyhow::bail!("no node found for {:?}", target),
    }
}

/// Replace the current process with ssh. In unit tests this function is never
/// called (tests call `build_ssh_argv` directly).
#[cfg(not(test))]
fn exec_ssh(argv: Vec<String>) -> Result<()> {
    use anyhow::Context as _;
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    let err = cmd.exec();
    Err(err).with_context(|| format!("exec {:?}", argv[0]))
}

#[cfg(test)]
fn exec_ssh(_argv: Vec<String>) -> Result<()> {
    // Never exec in tests — just a no-op safety net.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DedupeKind, Node, Tags, Tier};
    use chrono::Utc;

    fn node_with_addresses(fqdn: &str, addrs: Vec<&str>) -> Node {
        let now = Utc::now();
        Node {
            fleet_id: "test-ssh-01".to_owned(),
            hostname: fqdn.to_owned(),
            fqdn: fqdn.to_owned(),
            seen_in: vec![],
            addresses: addrs.into_iter().map(str::to_owned).collect(),
            os: "linux".to_owned(),
            online: true,
            last_seen: now,
            tags: Tags::default(),
            tier: Tier::Agent,
            dedupe_key_kind: DedupeKind::Machinekey,
            notes: None,
            first_seen: now,
            updated_at: now,
            fuzzy_hint: None,
        }
    }

    // ── Core safety test: crafted fqdn must never appear in argv ───────────────

    #[test]
    fn crafted_fqdn_never_in_argv() {
        // A node whose fqdn is a malicious SSH option injection attempt.
        // The argv must connect to the validated 100.x IP, not this string.
        let evil_fqdn = "-oProxyCommand=evil";
        let node = node_with_addresses(evil_fqdn, vec!["100.77.1.2"]);

        let argv = build_ssh_argv(&node, "operator", false, &[]).unwrap();

        // The evil fqdn must not appear anywhere in the argv
        for arg in &argv {
            assert!(
                !arg.contains(evil_fqdn),
                "evil fqdn leaked into argv: {:?}",
                argv
            );
            assert!(
                !arg.starts_with('-') || arg == "--",
                "suspicious option in argv (not '--'): {:?} in {:?}",
                arg,
                argv
            );
        }

        // Must connect to the 100.x IP
        assert!(argv.iter().any(|a| a.contains("100.77.1.2")));
    }

    #[test]
    fn argv_connects_to_validated_ip_not_fqdn() {
        let node = node_with_addresses("-oProxyCommand=evil", vec!["100.64.5.10"]);
        let argv = build_ssh_argv(&node, "root", false, &[]).unwrap();

        // Program is "ssh"
        assert_eq!(argv[0], "ssh");

        // -- separator present
        assert!(
            argv.contains(&"--".to_owned()),
            "missing -- separator: {argv:?}"
        );

        // user@IP is one element, and the IP is the validated 100.x one
        let host_arg = argv.iter().find(|a| a.contains("100.64.5.10")).unwrap();
        assert_eq!(host_arg, "root@100.64.5.10");

        // No element is a bare option (starts with '-') except "--"
        for arg in &argv {
            if arg != "--" {
                assert!(
                    !arg.starts_with('-'),
                    "unexpected option in argv: {:?}",
                    arg
                );
            }
        }
    }

    #[test]
    fn user_at_ip_as_separate_element() {
        let node = node_with_addresses("legit.ts.net", vec!["100.100.1.1"]);
        let argv = build_ssh_argv(&node, "alice", false, &[]).unwrap();

        // "alice@100.100.1.1" must be a single element, not split across multiple
        let host_elements: Vec<_> = argv.iter().filter(|a| a.contains('@')).collect();
        assert_eq!(
            host_elements.len(),
            1,
            "should have exactly one user@host element"
        );
        assert_eq!(host_elements[0], "alice@100.100.1.1");
    }

    #[test]
    fn double_dash_before_host_token() {
        let node = node_with_addresses("host.ts.net", vec!["100.80.0.1"]);
        let argv = build_ssh_argv(&node, "bob", false, &[]).unwrap();

        let dash_pos = argv.iter().position(|a| a == "--").expect("no -- in argv");
        let host_pos = argv
            .iter()
            .position(|a| a.contains("100.80.0.1"))
            .expect("no host in argv");

        assert!(
            dash_pos < host_pos,
            "-- must come before host token; argv={argv:?}"
        );
    }

    #[test]
    fn ts_flag_switches_to_tailscale_ssh() {
        let node = node_with_addresses("ts-host.ts.net", vec!["100.99.0.7"]);
        let argv = build_ssh_argv(&node, "admin", true, &[]).unwrap();

        assert_eq!(argv[0], "tailscale");
        assert_eq!(argv[1], "ssh");
        assert!(argv.contains(&"--".to_owned()));
        assert!(argv.iter().any(|a| a.contains("100.99.0.7")));
    }

    #[test]
    fn no_tailscale_ip_returns_error() {
        // Node only has an IPv6 address — no 100.x
        let node = node_with_addresses("noip.ts.net", vec!["fd7a::1", "192.168.1.5"]);
        let err = build_ssh_argv(&node, "root", false, &[]);
        assert!(err.is_err(), "should fail when no 100.x address");
        assert!(err.unwrap_err().to_string().contains("100.64.0.0/10"));
    }

    #[test]
    fn is_tailscale_ip_range() {
        // Valid tailscale IPs (100.64.0.0 – 100.127.255.255)
        assert!(is_tailscale_ip("100.64.0.1".parse().unwrap()));
        assert!(is_tailscale_ip("100.100.5.5".parse().unwrap()));
        assert!(is_tailscale_ip("100.127.255.255".parse().unwrap()));

        // Not in range
        assert!(!is_tailscale_ip("100.63.255.255".parse().unwrap()));
        assert!(!is_tailscale_ip("100.128.0.0".parse().unwrap()));
        assert!(!is_tailscale_ip("192.168.1.1".parse().unwrap()));
        assert!(!is_tailscale_ip("10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn remote_cmd_appended_after_host() {
        let node = node_with_addresses("cmd-host.ts.net", vec!["100.70.1.1"]);
        let remote_cmd = vec!["ls".to_owned(), "-la".to_owned()];
        let argv = build_ssh_argv(&node, "root", false, &remote_cmd).unwrap();

        // Find host position, then check cmd args follow
        let host_pos = argv.iter().position(|a| a.contains('@')).unwrap();
        assert_eq!(argv[host_pos + 1], "ls");
        assert_eq!(argv[host_pos + 2], "-la");
    }

    #[test]
    fn picks_first_tailscale_ip_from_multiple() {
        // Node has both IPv6 and two 100.x addresses — should pick the first 100.x
        let node = node_with_addresses("multi.ts.net", vec!["fd7a::1", "100.65.0.1", "100.66.0.2"]);
        let ip = pick_tailscale_ip(&node).unwrap();
        assert_eq!(ip.to_string(), "100.65.0.1");
    }
}

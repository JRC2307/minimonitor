use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct PortRow {
    pub port: u16,
    pub proto: String,
    pub process: String,
    pub pid: u32,
    pub bind: String,
}

use std::process::Command;

/// Parse one `lsof -nP -iTCP -sTCP:LISTEN` row into a PortRow.
/// Columns: COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME (STATE)
pub fn parse_listen_line(line: &str) -> Option<PortRow> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 9 {
        return None;
    }
    let pid: u32 = parts[1].parse().ok()?; // header row's "PID" fails here → skipped
    let proto = parts[7].to_owned();
    let name = parts[8];
    // NAME is addr:port (no "->" for LISTEN). Split on the last ':'.
    let (addr, port_str) = name.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    let bind = if addr.is_empty() {
        "*".to_owned()
    } else {
        addr.to_owned()
    };
    Some(PortRow {
        port,
        proto,
        process: parts[0].to_owned(),
        pid,
        bind,
    })
}

pub fn parse_listen_output(output: &str) -> Vec<PortRow> {
    output.lines().filter_map(parse_listen_line).collect()
}

pub fn listening_ports() -> Vec<PortRow> {
    let mut rows = match Command::new("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN"])
        .output()
    {
        Ok(out) => parse_listen_output(&String::from_utf8_lossy(&out.stdout)),
        Err(_) => Vec::new(),
    };

    // Linux: unprivileged lsof cannot see sockets owned by other users (e.g.
    // root's docker-proxy), so container ports vanish from snapshots. `ss -ltn`
    // lists every listening socket regardless of owner — merge in the ports
    // lsof missed, with an unknown process (pid 0).
    if cfg!(target_os = "linux")
        && let Ok(out) = Command::new("ss").args(["-ltnH"]).output()
    {
        let seen: std::collections::HashSet<u16> = rows.iter().map(|r| r.port).collect();
        rows.extend(
            parse_ss_listen_output(&String::from_utf8_lossy(&out.stdout))
                .into_iter()
                .filter(|r| !seen.contains(&r.port)),
        );
    }

    rows
}

/// Parse `ss -ltnH` output (no header). Columns:
/// `State Recv-Q Send-Q Local-Address:Port Peer-Address:Port [Process]`
/// Only the local address:port is trusted; the owning process is unknown
/// without root, so `process` is `"?"` and `pid` 0.
pub fn parse_ss_listen_output(output: &str) -> Vec<PortRow> {
    output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 5 || parts[0] != "LISTEN" {
                return None;
            }
            let (addr, port_str) = parts[3].rsplit_once(':')?;
            let port: u16 = port_str.parse().ok()?;
            let bind = match addr {
                "" | "*" | "0.0.0.0" | "[::]" => "*".to_owned(),
                a => a.trim_start_matches('[').trim_end_matches(']').to_owned(),
            };
            Some(PortRow {
                port,
                proto: "TCP".to_owned(),
                process: "?".to_owned(),
                pid: 0,
                bind,
            })
        })
        .collect()
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct ConnGroup {
    pub process: String,
    pub pid: u32,
    pub count: usize,
}

pub fn parse_estab_output(output: &str) -> Vec<ConnGroup> {
    let mut counts: HashMap<(String, u32), usize> = HashMap::new();
    for line in output.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 9 {
            continue;
        }
        let Ok(pid) = parts[1].parse::<u32>() else {
            continue;
        };
        *counts.entry((parts[0].to_owned(), pid)).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .map(|((process, pid), count)| ConnGroup {
            process,
            pid,
            count,
        })
        .collect()
}

pub fn established_connections() -> Vec<ConnGroup> {
    let Ok(out) = Command::new("lsof")
        .args(["-nP", "-iTCP", "-sTCP:ESTABLISHED"])
        .output()
    else {
        return Vec::new();
    };
    parse_estab_output(&String::from_utf8_lossy(&out.stdout))
}

#[derive(Clone, Serialize, Deserialize, Default, PartialEq, Debug)]
pub struct NetIdentity {
    pub hostname: String,
    pub lan_ip: Option<String>,
    pub tailnet_ip: Option<String>,
}

fn first_line(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if s.is_empty() { None } else { Some(s) }
}

/// The interface carrying the default route (e.g. "en0", "en1"), per `route get default`.
fn default_interface() -> Option<String> {
    let out = Command::new("route")
        .args(["-n", "get", "default"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
        l.trim()
            .strip_prefix("interface:")
            .map(|s| s.trim().to_owned())
    })
}

/// IPv4 of the active interface — the default-route one first, then common fallbacks.
/// The mini's active interface isn't always `en0` (Wi-Fi/Ethernet/USB-LAN vary).
fn lan_ip() -> Option<String> {
    if let Some(iface) = default_interface()
        && let Some(ip) = first_line("ipconfig", &["getifaddr", &iface])
    {
        return Some(ip);
    }
    for iface in ["en0", "en1", "en2"] {
        if let Some(ip) = first_line("ipconfig", &["getifaddr", iface]) {
            return Some(ip);
        }
    }
    None
}

pub fn network_identity(hostname: String) -> NetIdentity {
    NetIdentity {
        hostname,
        lan_ip: lan_ip(),
        tailnet_ip: first_line("tailscale", &["ip", "-4"])
            .and_then(|s| s.lines().next().map(|l| l.to_owned())),
    }
}

// ─── Tailnet bind validator (§3.2) ───────────────────────────────────────────
//
// Dependency-free, IPv6-aware. No `ipnet` — hand-rolled octet/segment checks.
// Used by both `fleet doctor` and the agent self-guard before binding.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// RFC 6598 CGNAT `100.64.0.0/10` — the Tailscale IPv4 overlay range.
pub fn is_cgnat(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

/// Tailscale's IPv6 ULA prefix `fd7a:115c:a1e0::/48`.
fn is_tailscale_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
}

/// True for bare loopback host strings (before bracket-stripping).
fn is_loopback_host(host: &str) -> bool {
    host == "127.0.0.1" || host.starts_with("127.") || host == "::1" || host == "[::1]"
}

/// Validate a bind `HOST:PORT` string.
///
/// **ACCEPTS**: loopback (127.x, `::1`), IPv4 CGNAT `100.64.0.0/10` literals,
/// Tailscale ULA IPv6 `fd7a:115c:a1e0::/48` literals, and `${VAR}`/`{{ }}`
/// template strings whose host portion is not a parseable IP.
///
/// **REJECTS**: `0.0.0.0`, `[::]`/`:::PORT`, `[fe80::...]`, non-CGNAT public v4,
/// bare port (no host), empty host, and missing port.
pub fn validate_tailnet_bind(bind: &str) -> Result<(), String> {
    // rsplit_once(':') on `[::]:9909` would give host=`[:]`, port=`9909` — wrong.
    // We bracket-strip AFTER splitting off the trailing :PORT segment.
    let (host_raw, port) = match bind.rsplit_once(':') {
        Some((h, p)) => (h, p),
        None => {
            return Err(format!(
                "`{bind}` has no explicit host (implicit wildcard — bare port or no port)"
            ));
        }
    };

    if port.is_empty() {
        return Err(format!("`{bind}` has no port"));
    }

    // Strip IPv6 brackets: `[::1]` → `::1`, `[::]` → `::`.
    let host = host_raw.trim_start_matches('[').trim_end_matches(']');

    if host.is_empty() {
        return Err(format!("`{bind}` has an empty host (implicit wildcard)"));
    }

    // Check loopback before IP-parsing (handles both `127.x.x.x` and `[::1]`/`::1`).
    if is_loopback_host(host) {
        return Ok(());
    }

    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) if is_cgnat(ip) => Ok(()),
        Ok(IpAddr::V4(ip)) => Err(format!("`{bind}` host {ip} is not in CGNAT 100.64.0.0/10")),
        Ok(IpAddr::V6(ip)) if is_tailscale_v6(ip) => Ok(()),
        Ok(IpAddr::V6(ip)) => Err(format!(
            "`{bind}` host {ip} is not a Tailscale ULA (fd7a:115c:a1e0::/48)"
        )),
        // Only non-IP-parseable strings with template markers are treated as
        // install-time templates. Bare hostnames without `$`/`{` are rejected.
        Err(_) if host.contains('$') || host.contains('{') => Ok(()),
        Err(_) => Err(format!(
            "`{bind}` host `{host}` is neither a tailnet IP nor a ${{VAR}} template"
        )),
    }
}

// ─── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_ss_listen_output ───────────────────────────────────────────────

    #[test]
    fn ss_listen_parses_docker_proxy_rows() {
        let out = "\
LISTEN 0      4096                100.119.198.54:8082       0.0.0.0:*
LISTEN 0      4096                100.119.198.54:8090       0.0.0.0:*
LISTEN 0      511                        0.0.0.0:80          0.0.0.0:*
LISTEN 0      4096                          [::]:23231          [::]:*
";
        let rows = parse_ss_listen_output(out);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].port, 8082);
        assert_eq!(rows[0].bind, "100.119.198.54");
        assert_eq!(rows[0].process, "?");
        assert_eq!(rows[0].pid, 0);
        assert_eq!(rows[2].bind, "*", "0.0.0.0 normalizes to *");
        assert_eq!(rows[3].port, 23231);
        assert_eq!(rows[3].bind, "*", "[::] normalizes to *");
    }

    #[test]
    fn ss_listen_skips_garbage_and_headers() {
        let out = "\
State  Recv-Q Send-Q Local Address:Port Peer Address:Port Process
ESTAB  0      0      1.2.3.4:5          6.7.8.9:10
not a row
";
        assert!(parse_ss_listen_output(out).is_empty());
    }

    // ── is_cgnat ─────────────────────────────────────────────────────────────

    #[test]
    fn cgnat_range_boundaries() {
        assert!(is_cgnat("100.64.0.0".parse().unwrap()));
        assert!(is_cgnat("100.64.0.1".parse().unwrap()));
        assert!(is_cgnat("100.127.255.255".parse().unwrap()));
    }

    #[test]
    fn outside_cgnat() {
        assert!(!is_cgnat("100.63.255.255".parse().unwrap())); // just below
        assert!(!is_cgnat("100.128.0.0".parse().unwrap())); // just above
        assert!(!is_cgnat("192.168.1.5".parse().unwrap()));
        assert!(!is_cgnat("10.0.0.1".parse().unwrap()));
        assert!(!is_cgnat("0.0.0.0".parse().unwrap()));
    }

    // ── validate_tailnet_bind — ACCEPT ────────────────────────────────────────

    #[test]
    fn accept_ipv4_loopback() {
        assert!(validate_tailnet_bind("127.0.0.1:9909").is_ok());
        assert!(validate_tailnet_bind("127.1.2.3:9909").is_ok());
    }

    #[test]
    fn accept_ipv6_loopback() {
        assert!(validate_tailnet_bind("[::1]:9909").is_ok());
    }

    #[test]
    fn accept_cgnat_boundaries() {
        assert!(validate_tailnet_bind("100.64.0.1:9909").is_ok());
        assert!(validate_tailnet_bind("100.127.255.255:9909").is_ok());
        assert!(validate_tailnet_bind("100.96.1.2:9909").is_ok());
    }

    #[test]
    fn accept_tailscale_ula_v6() {
        assert!(validate_tailnet_bind("[fd7a:115c:a1e0::1]:9909").is_ok());
    }

    #[test]
    fn accept_template_dollar_brace() {
        assert!(validate_tailnet_bind("${HOST_TS_IP}:9909").is_ok());
        assert!(validate_tailnet_bind("${BIND_ADDR}:9909").is_ok());
    }

    #[test]
    fn accept_template_double_brace() {
        assert!(validate_tailnet_bind("{{ ts_ip }}:9909").is_ok());
    }

    // ── validate_tailnet_bind — REJECT ────────────────────────────────────────

    #[test]
    fn reject_ipv4_wildcard() {
        assert!(validate_tailnet_bind("0.0.0.0:9909").is_err());
    }

    #[test]
    fn reject_ipv6_wildcard_bracketed() {
        // THE critical bug: IPv4-only rsplit would accept this via template fall-through
        assert!(validate_tailnet_bind("[::]:9909").is_err());
    }

    #[test]
    fn reject_ipv6_wildcard_bare() {
        // `:::9909` — rsplit_once gives host=`::`, port=`9909`
        assert!(validate_tailnet_bind(":::9909").is_err());
    }

    #[test]
    fn reject_link_local_v6() {
        assert!(validate_tailnet_bind("[fe80::1]:9909").is_err());
    }

    #[test]
    fn reject_public_ipv4() {
        assert!(validate_tailnet_bind("1.2.3.4:9909").is_err());
        assert!(validate_tailnet_bind("203.0.113.1:9909").is_err());
    }

    #[test]
    fn reject_just_below_cgnat() {
        assert!(validate_tailnet_bind("100.63.255.255:9909").is_err());
    }

    #[test]
    fn reject_bare_port() {
        assert!(validate_tailnet_bind(":9909").is_err());
        assert!(validate_tailnet_bind("9909").is_err());
    }

    #[test]
    fn reject_no_port() {
        assert!(validate_tailnet_bind("127.0.0.1").is_err());
        assert!(validate_tailnet_bind("100.96.1.2").is_err());
    }

    #[test]
    fn reject_empty_string() {
        assert!(validate_tailnet_bind("").is_err());
    }
}

// ─── lsof / port-parsing tests ───────────────────────────────────────────────

#[cfg(test)]
mod lsof_tests {
    use super::*;

    #[test]
    fn parses_ipv4_listen_line() {
        let line =
            "node      8412 caguabot   23u  IPv4 0x1234      0t0  TCP 127.0.0.1:3000 (LISTEN)";
        let row = parse_listen_line(line).unwrap();
        assert_eq!(
            row,
            PortRow {
                port: 3000,
                proto: "TCP".into(),
                process: "node".into(),
                pid: 8412,
                bind: "127.0.0.1".into(),
            }
        );
    }

    #[test]
    fn parses_wildcard_and_ipv6() {
        let v4 = "ttyd       901 caguabot    3u  IPv4 0x1 0t0 TCP *:7681 (LISTEN)";
        assert_eq!(parse_listen_line(v4).unwrap().bind, "*");
        let v6 = "postgres   455 caguabot    5u  IPv6 0x2 0t0 TCP [::1]:5432 (LISTEN)";
        let r = parse_listen_line(v6).unwrap();
        assert_eq!((r.port, r.bind.as_str()), (5432, "[::1]"));
    }

    #[test]
    fn skips_header_and_garbage() {
        assert!(parse_listen_line("COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME").is_none());
        assert!(parse_listen_line("too few cols").is_none());
    }

    #[test]
    fn parse_output_skips_header_row() {
        let out = "COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME\n\
                   node 8412 me 23u IPv4 0x1 0t0 TCP 127.0.0.1:3000 (LISTEN)\n";
        let rows = parse_listen_output(out);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].port, 3000);
    }

    #[test]
    fn groups_established_by_process() {
        let out = "COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME\n\
            firefox 700 me 50u IPv4 0x1 0t0 TCP 192.168.1.5:54321->1.1.1.1:443 (ESTABLISHED)\n\
            firefox 700 me 51u IPv4 0x2 0t0 TCP 192.168.1.5:54322->1.1.1.2:443 (ESTABLISHED)\n\
            claude  900 me 10u IPv4 0x3 0t0 TCP 192.168.1.5:54400->2.2.2.2:443 (ESTABLISHED)\n";
        let mut groups = parse_estab_output(out);
        groups.sort_by(|a, b| b.count.cmp(&a.count));
        assert_eq!(groups.len(), 2);
        assert_eq!(
            (groups[0].process.as_str(), groups[0].pid, groups[0].count),
            ("firefox", 700, 2)
        );
        assert_eq!((groups[1].process.as_str(), groups[1].count), ("claude", 1));
    }
}

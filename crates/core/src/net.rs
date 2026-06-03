use serde::Serialize;
use std::collections::HashMap;

#[derive(Clone, Serialize, PartialEq, Debug)]
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
    let bind = if addr.is_empty() { "*".to_owned() } else { addr.to_owned() };
    Some(PortRow { port, proto, process: parts[0].to_owned(), pid, bind })
}

pub fn parse_listen_output(output: &str) -> Vec<PortRow> {
    output.lines().filter_map(parse_listen_line).collect()
}

pub fn listening_ports() -> Vec<PortRow> {
    let Ok(out) = Command::new("lsof").args(["-nP", "-iTCP", "-sTCP:LISTEN"]).output() else {
        return Vec::new();
    };
    parse_listen_output(&String::from_utf8_lossy(&out.stdout))
}

#[derive(Clone, Serialize, PartialEq, Debug)]
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
        let Ok(pid) = parts[1].parse::<u32>() else { continue };
        *counts.entry((parts[0].to_owned(), pid)).or_insert(0) += 1;
    }
    counts.into_iter()
        .map(|((process, pid), count)| ConnGroup { process, pid, count })
        .collect()
}

pub fn established_connections() -> Vec<ConnGroup> {
    let Ok(out) = Command::new("lsof").args(["-nP", "-iTCP", "-sTCP:ESTABLISHED"]).output() else {
        return Vec::new();
    };
    parse_estab_output(&String::from_utf8_lossy(&out.stdout))
}

#[derive(Clone, Serialize, Default, PartialEq, Debug)]
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

pub fn network_identity(hostname: String) -> NetIdentity {
    NetIdentity {
        hostname,
        lan_ip: first_line("ipconfig", &["getifaddr", "en0"]),
        tailnet_ip: first_line("tailscale", &["ip", "-4"])
            .and_then(|s| s.lines().next().map(|l| l.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipv4_listen_line() {
        let line = "node      8412 caguabot   23u  IPv4 0x1234      0t0  TCP 127.0.0.1:3000 (LISTEN)";
        let row = parse_listen_line(line).unwrap();
        assert_eq!(row, PortRow {
            port: 3000, proto: "TCP".into(), process: "node".into(),
            pid: 8412, bind: "127.0.0.1".into(),
        });
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
        assert_eq!((groups[0].process.as_str(), groups[0].pid, groups[0].count), ("firefox", 700, 2));
        assert_eq!((groups[1].process.as_str(), groups[1].count), ("claude", 1));
    }
}

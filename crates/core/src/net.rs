use serde::Serialize;

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
}

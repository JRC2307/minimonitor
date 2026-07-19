//! The MTR path prober — the differentiated "built 20%" (spec §5).
//!
//! This is the **single** file in the crate that touches `trippy-core =0.13.0`
//! (the explicitly-unstable, exact-pinned dep, R-7). Everything the rest of the
//! crate consumes is plain in-memory structs (`HopStat`, `Alert`, `Severity`,
//! `PathType`) so the unstable surface never leaks past this module.
//!
//! Three concerns live here, only the first touches trippy:
//!   1. `trace()` — build a `trippy_core::Tracer` (platform [`PRIVILEGE_MODE`] +
//!      Classic + ICMP, v4-only) with a startup socket self-check, run `cfg.cycles` rounds, and
//!      `aggregate()` the snapshot into `Vec<HopStat>`.
//!   2. `aggregate()` — pure: `trippy_core::State` → `Vec<HopStat>`.
//!   3. `evaluate()` / `severity()` — PURE alert policy, destination-hop-only
//!      (the #1 false-positive trap, resolved risk R-probe).
//!
//! NEVER trace live in tests. The pure functions carry the test weight; the
//! socket self-check is exercised through an injectable seam (`SocketChecker`).

use serde::{Deserialize, Serialize};
use std::net::IpAddr;

// ─── Plain data the rest of the crate consumes ───────────────────────────────

/// The two path classes a probe target can belong to (spec §5). A public IP
/// traces the internet **underlay**; a `100.x` Tailscale IP traces the
/// WireGuard/DERP **overlay**. Stored per `probe_run`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathType {
    Underlay,
    Overlay,
}

impl PathType {
    pub fn as_str(self) -> &'static str {
        match self {
            PathType::Underlay => "underlay",
            PathType::Overlay => "overlay",
        }
    }

    /// Parse a config `path` string; anything not `overlay` is `underlay`.
    pub fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case("overlay") {
            PathType::Overlay
        } else {
            PathType::Underlay
        }
    }
}

/// Per-hop severity, computed server-side so the `fleet serve` UI just renders a
/// string (spec §5). `warn` is the 0.7× band up to and including the threshold;
/// `breach` is STRICTLY GREATER than the threshold; `ok` otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Ok,
    Warn,
    Breach,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Ok => "ok",
            Severity::Warn => "warn",
            Severity::Breach => "breach",
        }
    }
}

/// One aggregated hop in a trace — the mtr-style per-hop row. `host == None`
/// means the hop did not respond (`???`); such hops are stored as 100% loss but
/// are NEVER the basis of an alert (only the destination hop is, §5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HopStat {
    pub ttl: u8,
    /// First responding address for this hop, or `None` for a non-responding
    /// (`???`) hop.
    pub host: Option<String>,
    pub sent: u32,
    pub recv: u32,
    pub loss_pct: f64,
    pub last_ms: f64,
    pub avg_ms: f64,
    pub best_ms: f64,
    pub worst_ms: f64,
    pub stddev_ms: f64,
    /// Computed at aggregation/evaluation time against the configured thresholds.
    pub severity: Severity,
}

/// A breach alert carrying the destination hop's identity and metrics, surfaced
/// to ntfy at priority 4 (spec §5).
#[derive(Clone, Debug, PartialEq)]
pub struct Alert {
    pub ttl: u8,
    pub host: Option<String>,
    pub loss_pct: f64,
    pub avg_ms: f64,
}

impl Alert {
    fn breach(dest: &HopStat) -> Self {
        Alert {
            ttl: dest.ttl,
            host: dest.host.clone(),
            loss_pct: dest.loss_pct,
            avg_ms: dest.avg_ms,
        }
    }
}

// ─── Pure alert policy (the heart of the task) ───────────────────────────────

/// The fraction of a threshold at/above which a hop is `warn` (vs `breach` at
/// the threshold itself). 0.7× per spec §5.
const WARN_FACTOR: f64 = 0.7;

/// Classify a single hop against the destination thresholds (spec §5).
///
/// A hop is `breach` when its loss% **or** avg RTT is STRICTLY GREATER THAN its
/// threshold (matching `evaluate()`'s `>` semantics), `warn` when either is at/above
/// 0.7× its threshold (but neither breaches), and `ok` otherwise. At EXACTLY the
/// threshold the hop is `warn`, not `breach`. Pure.
pub fn severity(hop: &HopStat, loss_threshold_pct: f64, rtt_threshold_ms: f64) -> Severity {
    let breach = hop.loss_pct > loss_threshold_pct || hop.avg_ms > rtt_threshold_ms;
    if breach {
        return Severity::Breach;
    }
    let warn = hop.loss_pct >= loss_threshold_pct * WARN_FACTOR
        || hop.avg_ms >= rtt_threshold_ms * WARN_FACTOR;
    if warn { Severity::Warn } else { Severity::Ok }
}

/// Resolve the **destination** hop = the LAST responding hop in the path.
///
/// A middle hop at 100% loss with later responding hops is NORMAL (routers
/// deprioritize ICMP TTL-exceeded) — so the destination is found by walking from
/// the end. Returns `None` when no hop responded (fully unreachable), which the
/// caller treats as "no destination alert", never a panic.
pub fn destination_hop(hops: &[HopStat]) -> Option<&HopStat> {
    hops.iter().rev().find(|h| h.host.is_some())
}

/// Pure alert policy — **destination-hop-only** (resolved risk R-probe).
///
/// Loss/RTT alerts fire only on the destination (last responding) hop, never on
/// intermediates. A fully-unreachable trace (all `???`) yields `None` (handled,
/// not a panic).
pub fn evaluate(hops: &[HopStat], loss_threshold_pct: f64, rtt_threshold_ms: f64) -> Option<Alert> {
    let dest = destination_hop(hops)?;
    (dest.loss_pct > loss_threshold_pct || dest.avg_ms > rtt_threshold_ms)
        .then(|| Alert::breach(dest))
}

/// Stamp each hop with its computed [`Severity`] in place (the value that lands
/// in `probe_hop.severity` and `path-health.json`). Pure.
pub fn apply_severities(hops: &mut [HopStat], loss_threshold_pct: f64, rtt_threshold_ms: f64) {
    for h in hops.iter_mut() {
        h.severity = severity(h, loss_threshold_pct, rtt_threshold_ms);
    }
}

// ─── The trippy adapter (the only unstable surface) ──────────────────────────

/// A seam over "open the unprivileged dgram-ICMP socket" so the startup
/// self-check is testable without privileges. The production impl actually opens
/// the socket; tests inject a fake that succeeds or fails on demand.
pub trait SocketChecker {
    /// Returns `Ok(())` if an unprivileged ICMP probe socket can be opened for
    /// the given target family, a loud `Err` otherwise.
    fn can_open(&self, target: IpAddr) -> anyhow::Result<()>;
}

/// Trippy privilege mode per platform. macOS supports unprivileged dgram-ICMP
/// tracing; Linux dgram-ICMP sockets reject `IP_HDRINCL` (which trippy sets),
/// so tracing there needs `Privileged` raw sockets — grant `CAP_NET_RAW`
/// (e.g. systemd `AmbientCapabilities=CAP_NET_RAW` on the probe unit).
#[cfg(target_os = "linux")]
pub const PRIVILEGE_MODE: trippy_core::PrivilegeMode = trippy_core::PrivilegeMode::Privileged;
#[cfg(not(target_os = "linux"))]
pub const PRIVILEGE_MODE: trippy_core::PrivilegeMode = trippy_core::PrivilegeMode::Unprivileged;

/// Production socket checker: actually attempts to open the same ICMP v4
/// socket family trippy will use on this platform (`SOCK_DGRAM` on macOS,
/// `SOCK_RAW` on Linux — see [`PRIVILEGE_MODE`]). If the OS refuses, we fail
/// loudly rather than silently producing empty traces (R-6).
pub struct RealSocketChecker;

impl SocketChecker for RealSocketChecker {
    fn can_open(&self, target: IpAddr) -> anyhow::Result<()> {
        use socket2::{Domain, Protocol as SockProto, Socket, Type};
        if target.is_ipv6() {
            anyhow::bail!("probe is v4-only for Phase 1; refusing IPv6 target {target}");
        }
        // Match trippy's socket family for PRIVILEGE_MODE; if the OS denies it
        // we cannot trace, so surface a loud error now.
        #[cfg(target_os = "linux")]
        let (ty, hint) = (
            Type::RAW,
            "raw-ICMP socket (probe needs CAP_NET_RAW on Linux)",
        );
        #[cfg(not(target_os = "linux"))]
        let (ty, hint) = (
            Type::DGRAM,
            "unprivileged dgram-ICMP socket (SOCK_DGRAM/IPPROTO_ICMP, no root)",
        );
        Socket::new(Domain::IPV4, ty, Some(SockProto::ICMPV4))
            .map_err(|e| anyhow::anyhow!("cannot open {hint}: {e}"))?;
        Ok(())
    }
}

/// Construct a configured `trippy_core::Tracer` for `target`, after passing the
/// injectable startup self-check. Isolated so the unstable Builder API touches
/// exactly one function.
///
/// Returns a loud `Err` if the self-check fails (no privileges / wrong family)
/// or if trippy rejects the configuration.
pub fn build_tracer(
    target: IpAddr,
    cycles: usize,
    checker: &dyn SocketChecker,
) -> anyhow::Result<trippy_core::Tracer> {
    // Startup self-check FIRST — fail loudly instead of producing empty traces.
    checker
        .can_open(target)
        .map_err(|e| anyhow::anyhow!("probe self-check failed for {target}: {e}"))?;

    if target.is_ipv6() {
        anyhow::bail!("probe is v4-only for Phase 1; refusing IPv6 target {target}");
    }

    let tracer = trippy_core::Builder::new(target)
        .privilege_mode(PRIVILEGE_MODE)
        .multipath_strategy(trippy_core::MultipathStrategy::Classic)
        .protocol(trippy_core::Protocol::Icmp)
        .max_rounds(Some(cycles))
        .build()
        .map_err(|e| anyhow::anyhow!("building trippy tracer for {target}: {e}"))?;
    Ok(tracer)
}

/// Run a full trace against `target` (build → run `cycles` rounds → aggregate).
///
/// Blocking (trippy + SQLite are sync); call from `spawn_blocking`. NEVER call
/// from a test — this opens real sockets and traces the live network.
pub fn trace(
    target: IpAddr,
    cycles: usize,
    checker: &dyn SocketChecker,
) -> anyhow::Result<Vec<HopStat>> {
    let tracer = build_tracer(target, cycles, checker)?;
    tracer
        .run()
        .map_err(|e| anyhow::anyhow!("running trippy trace for {target}: {e}"))?;
    Ok(aggregate(&tracer.snapshot()))
}

/// Pure aggregation: a trippy `State` snapshot → `Vec<HopStat>`.
///
/// Per-hop loss%/RTT stats come straight from trippy's `Hop` accessors; a hop
/// with no addresses is a non-responding (`???`) hop → `host = None`, recorded
/// at 100% loss (informational, never alerted). Severity is left `Ok` here and
/// stamped later by [`apply_severities`] once thresholds are known.
pub fn aggregate(state: &trippy_core::State) -> Vec<HopStat> {
    state
        .hops()
        .iter()
        .map(|hop| {
            let host = hop.addrs().next().map(std::string::ToString::to_string);
            HopStat {
                ttl: hop.ttl(),
                host,
                sent: hop.total_sent() as u32,
                recv: hop.total_recv() as u32,
                loss_pct: hop.loss_pct(),
                last_ms: hop.last_ms().unwrap_or(0.0),
                avg_ms: hop.avg_ms(),
                best_ms: hop.best_ms().unwrap_or(0.0),
                worst_ms: hop.worst_ms().unwrap_or(0.0),
                stddev_ms: hop.stddev_ms(),
                severity: Severity::Ok,
            }
        })
        .collect()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Build a responding hop with given ttl/loss/avg; other stats are filler.
    pub fn hop(ttl: u8, host: Option<&str>, loss_pct: f64, avg_ms: f64) -> HopStat {
        HopStat {
            ttl,
            host: host.map(ToOwned::to_owned),
            sent: 10,
            recv: if host.is_some() { 10 } else { 0 },
            loss_pct,
            last_ms: avg_ms,
            avg_ms,
            best_ms: avg_ms,
            worst_ms: avg_ms,
            stddev_ms: 0.0,
            severity: Severity::Ok,
        }
    }

    /// A non-responding (`???`) hop at 100% loss.
    fn dead_hop(ttl: u8) -> HopStat {
        hop(ttl, None, 100.0, 0.0)
    }

    // ── evaluate: the #1 false-positive trap ────────────────────────────────

    #[test]
    fn middle_hop_100pct_loss_with_responding_dest_is_not_alerted() {
        // hop 2 is a dead middle router (100% loss), hop 3 is the healthy dest.
        let hops = vec![
            hop(1, Some("192.168.1.1"), 0.0, 2.0),
            dead_hop(2),
            hop(3, Some("1.1.1.1"), 0.0, 12.0),
        ];
        assert_eq!(
            evaluate(&hops, 20.0, 250.0),
            None,
            "a middle hop at 100% loss must NOT alert when the destination is healthy"
        );
    }

    #[test]
    fn destination_over_loss_threshold_is_alerted() {
        let hops = vec![
            hop(1, Some("192.168.1.1"), 0.0, 2.0),
            hop(2, Some("1.1.1.1"), 55.0, 30.0), // dest: loss 55% > 20%
        ];
        let alert = evaluate(&hops, 20.0, 250.0).expect("destination breach must alert");
        assert_eq!(alert.ttl, 2);
        assert_eq!(alert.host.as_deref(), Some("1.1.1.1"));
        assert!((alert.loss_pct - 55.0).abs() < f64::EPSILON);
    }

    #[test]
    fn destination_over_rtt_threshold_is_alerted() {
        let hops = vec![hop(2, Some("1.1.1.1"), 0.0, 300.0)]; // avg 300 > 250
        let alert = evaluate(&hops, 20.0, 250.0).expect("RTT breach must alert");
        assert_eq!(alert.ttl, 2);
    }

    #[test]
    fn destination_resolves_to_last_responding_hop_not_a_later_dead_hop() {
        // A breaching responder followed by dead probes (target stopped replying
        // on the very last rounds) — dest is still the last RESPONDING hop.
        let hops = vec![
            hop(1, Some("192.168.1.1"), 0.0, 2.0),
            hop(2, Some("1.1.1.1"), 55.0, 30.0),
            dead_hop(3),
        ];
        let alert = evaluate(&hops, 20.0, 250.0).expect("breach on last responder");
        assert_eq!(
            alert.ttl, 2,
            "dest is the last responding hop, not the ??? hop"
        );
    }

    #[test]
    fn all_non_responding_yields_none_no_panic() {
        let hops = vec![dead_hop(1), dead_hop(2), dead_hop(3)];
        assert_eq!(
            evaluate(&hops, 20.0, 250.0),
            None,
            "fully unreachable path must resolve to no destination alert, not panic"
        );
        assert!(destination_hop(&hops).is_none());
    }

    #[test]
    fn empty_hops_yields_none() {
        assert_eq!(evaluate(&[], 20.0, 250.0), None);
    }

    #[test]
    fn healthy_destination_under_thresholds_is_not_alerted() {
        let hops = vec![hop(2, Some("1.1.1.1"), 5.0, 30.0)];
        assert_eq!(evaluate(&hops, 20.0, 250.0), None);
    }

    // ── severity mapping: 0.7× warn / over breach / under ok ─────────────────

    #[test]
    fn severity_under_threshold_is_ok() {
        // loss 10 < 14 (0.7×20), avg 100 < 175 (0.7×250)
        let h = hop(1, Some("x"), 10.0, 100.0);
        assert_eq!(severity(&h, 20.0, 250.0), Severity::Ok);
    }

    #[test]
    fn severity_at_0_7x_threshold_is_warn() {
        // loss exactly 14 == 0.7×20 → warn; avg 100 stays under its warn band
        let h = hop(1, Some("x"), 14.0, 100.0);
        assert_eq!(severity(&h, 20.0, 250.0), Severity::Warn);
    }

    #[test]
    fn severity_rtt_at_0_7x_threshold_is_warn() {
        // avg exactly 175 == 0.7×250 → warn
        let h = hop(1, Some("x"), 0.0, 175.0);
        assert_eq!(severity(&h, 20.0, 250.0), Severity::Warn);
    }

    #[test]
    fn severity_over_threshold_is_breach() {
        let h = hop(1, Some("x"), 25.0, 100.0); // loss 25 > 20 → breach
        assert_eq!(severity(&h, 20.0, 250.0), Severity::Breach);
    }

    #[test]
    fn severity_at_exact_threshold_is_warn_not_breach() {
        // Boundary fix: exactly AT threshold → warn (alert fires only STRICTLY over).
        let h = hop(1, Some("x"), 20.0, 0.0); // loss exactly == 20.0
        assert_eq!(severity(&h, 20.0, 250.0), Severity::Warn);
    }

    #[test]
    fn severity_rtt_at_exact_threshold_is_warn_not_breach() {
        // Same boundary for RTT.
        let h = hop(1, Some("x"), 0.0, 250.0); // avg exactly == 250.0
        assert_eq!(severity(&h, 20.0, 250.0), Severity::Warn);
    }

    #[test]
    fn severity_just_over_threshold_is_breach() {
        // Strictly over threshold → breach (use a value representably above 20.0).
        let h = hop(1, Some("x"), 20.1, 0.0);
        assert_eq!(severity(&h, 20.0, 250.0), Severity::Breach);
    }

    #[test]
    fn evaluate_at_exact_threshold_no_alert() {
        // Exactly at threshold: no page, no breach (the missing case from the old suite).
        let hops = vec![hop(2, Some("1.1.1.1"), 20.0, 30.0)]; // loss == threshold
        assert_eq!(
            evaluate(&hops, 20.0, 250.0),
            None,
            "destination exactly AT threshold must NOT fire an alert"
        );
    }

    #[test]
    fn evaluate_just_over_threshold_alerts() {
        // Strictly over: must page.
        let hops = vec![hop(2, Some("1.1.1.1"), 20.1, 30.0)];
        assert!(
            evaluate(&hops, 20.0, 250.0).is_some(),
            "destination strictly over threshold must fire an alert"
        );
    }

    #[test]
    fn apply_severities_stamps_each_hop() {
        let mut hops = vec![
            hop(1, Some("a"), 0.0, 1.0),  // ok
            hop(2, Some("b"), 14.0, 0.0), // warn
            hop(3, Some("c"), 90.0, 0.0), // breach
        ];
        apply_severities(&mut hops, 20.0, 250.0);
        assert_eq!(hops[0].severity, Severity::Ok);
        assert_eq!(hops[1].severity, Severity::Warn);
        assert_eq!(hops[2].severity, Severity::Breach);
    }

    // ── PathType ─────────────────────────────────────────────────────────────

    #[test]
    fn path_type_parse_defaults_to_underlay() {
        assert_eq!(PathType::parse("overlay"), PathType::Overlay);
        assert_eq!(PathType::parse("OVERLAY"), PathType::Overlay);
        assert_eq!(PathType::parse("underlay"), PathType::Underlay);
        assert_eq!(PathType::parse("garbage"), PathType::Underlay);
    }

    // ── adapter self-check via the injectable seam ───────────────────────────

    struct FailingChecker;
    impl SocketChecker for FailingChecker {
        fn can_open(&self, _t: IpAddr) -> anyhow::Result<()> {
            anyhow::bail!("simulated EACCES: kernel refused the dgram-ICMP socket")
        }
    }

    struct OkChecker;
    impl SocketChecker for OkChecker {
        fn can_open(&self, _t: IpAddr) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn build_tracer_fails_loudly_when_socket_cannot_open() {
        let target: IpAddr = "1.1.1.1".parse().unwrap();
        let err = build_tracer(target, 3, &FailingChecker)
            .expect_err("a failed self-check must propagate a loud Err");
        let msg = err.to_string();
        assert!(
            msg.contains("self-check failed"),
            "error must name the self-check: {msg}"
        );
    }

    #[test]
    fn build_tracer_rejects_ipv6_target() {
        let target: IpAddr = "2606:4700:4700::1111".parse().unwrap();
        // FailingChecker would also reject, but use OkChecker to prove the
        // v4-only guard fires independently of the socket check.
        let err = build_tracer(target, 3, &OkChecker)
            .expect_err("v6 target must be refused (v4-only Phase 1)");
        assert!(err.to_string().contains("v4-only"), "{err}");
    }
}

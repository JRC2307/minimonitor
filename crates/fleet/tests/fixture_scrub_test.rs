/// Fixture-scrub gate — C2 task.
///
/// Parses the committed snapshot.json fixture and asserts that every
/// `ProcessRow.command` and `AiWorkload.example_command` contains no
/// secret-shaped substring.  This is a belt-and-suspenders check that
/// keeps the fixture clean; the real defense is the collect-time scrub
/// in `secrets::scrub_command`.
///
/// The fixture is synthetic — all process commands are drawn from a
/// small allowlist of safe strings.  Any value NOT in the allowlist
/// and NOT obviously safe (i.e. contains a secret-shaped pattern) should
/// fail here so we catch accidental real data leaking into the fixture.
use fleet::secrets::scrub_command;
use minimonitor_core::snapshot::MonitorSnapshot;

/// Regex patterns that would indicate a leaked secret.  These mirror the
/// patterns in `secrets::redact_str` / `scrub_command`.
fn looks_like_secret(s: &str) -> bool {
    use regex::Regex;
    use std::sync::OnceLock;

    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"(?ix)
            (?:password|token|secret|api_key|apikey)\s*=\s*\S+     # key=value secrets
            | Bearer\s+\S+                                          # Bearer tokens
            | [a-z][a-z0-9+\-.]*://[^:@/\s]+:[^@/\s]+@            # scheme://user:pass@
            ",
        )
        .unwrap()
    });
    re.is_match(s)
}

#[test]
fn fixture_process_commands_contain_no_secrets() {
    let snap: MonitorSnapshot = serde_json::from_str(include_str!("fixtures/snapshot.json"))
        .expect("failed to parse fixtures/snapshot.json");

    for proc in &snap.processes {
        assert!(
            !looks_like_secret(&proc.command),
            "ProcessRow (pid={}) command looks like it contains a secret: {:?}",
            proc.pid,
            proc.command,
        );
    }
}

#[test]
fn fixture_example_commands_contain_no_secrets() {
    let snap: MonitorSnapshot = serde_json::from_str(include_str!("fixtures/snapshot.json"))
        .expect("failed to parse fixtures/snapshot.json");

    for workload in &snap.ai_snapshot.top_workloads {
        assert!(
            !looks_like_secret(&workload.example_command),
            "AiWorkload {:?} example_command looks like it contains a secret: {:?}",
            workload.label,
            workload.example_command,
        );
    }
}

/// Scrub round-trip: calling scrub_command on a clean fixture command returns
/// the string unchanged (no false positives on benign argv).
#[test]
fn scrub_command_does_not_alter_clean_fixture_commands() {
    let snap: MonitorSnapshot = serde_json::from_str(include_str!("fixtures/snapshot.json"))
        .expect("failed to parse fixtures/snapshot.json");

    for proc in &snap.processes {
        let scrubbed = scrub_command(&proc.command);
        assert_eq!(
            scrubbed, proc.command,
            "scrub_command altered a clean fixture command (false positive) for pid={}: {:?} → {:?}",
            proc.pid, proc.command, scrubbed,
        );
    }

    for workload in &snap.ai_snapshot.top_workloads {
        let scrubbed = scrub_command(&workload.example_command);
        assert_eq!(
            scrubbed, workload.example_command,
            "scrub_command altered a clean fixture example_command (false positive) for workload {:?}: {:?} → {:?}",
            workload.label, workload.example_command, scrubbed,
        );
    }
}

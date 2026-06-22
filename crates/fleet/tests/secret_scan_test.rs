use std::path::Path;

/// Verifies that scripts/secret-scan.sh exits 0 on the clean repo tree AND
/// that the companion integration test (scripts/secret-scan.test.sh) — which
/// plants a staged fake secret and asserts detection — passes end-to-end.
///
/// Requires: bash, git available in PATH (always true on macOS CI).
#[test]
fn secret_scan_clean_tree_exits_zero() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap() // crates/
        .parent()
        .unwrap(); // repo root

    let scan = repo_root.join("scripts/secret-scan.sh");
    let status = std::process::Command::new("bash")
        .arg(&scan)
        .current_dir(repo_root)
        .status()
        .expect("failed to run secret-scan.sh");

    assert!(
        status.success(),
        "scripts/secret-scan.sh exited non-zero on the clean tree — check for leaked secrets"
    );
}

#[test]
fn secret_scan_detects_and_clean_integration() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap() // crates/
        .parent()
        .unwrap(); // repo root

    let test_script = repo_root.join("scripts/secret-scan.test.sh");
    let status = std::process::Command::new("bash")
        .arg(&test_script)
        .current_dir(repo_root)
        .status()
        .expect("failed to run secret-scan.test.sh");

    assert!(
        status.success(),
        "scripts/secret-scan.test.sh failed — detection or clean-tree test failed"
    );
}

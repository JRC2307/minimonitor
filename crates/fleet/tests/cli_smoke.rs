use assert_cmd::Command;

#[test]
fn version_flag_prints_semver() {
    let mut cmd = Command::cargo_bin("fleet").unwrap();
    cmd.arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::is_match(r"^fleet 0\.2\.0").unwrap());
}

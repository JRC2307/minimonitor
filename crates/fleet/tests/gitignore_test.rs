#[test]
fn ignores_deploy_secrets() {
    let content = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap() // crates/
            .parent()
            .unwrap() // repo root
            .join(".gitignore"),
    )
    .expect("read .gitignore");
    assert!(content.contains("deploy/.env"), "missing deploy/.env");
    assert!(content.contains("deploy/*_data/"), "missing deploy/*_data/");
    assert!(content.contains("deploy/ntfy/"), "missing deploy/ntfy/");
}

//! Integration tests for `fleet::export` JSON builders (Task 7).
//!
//! Tests cover:
//! - `build_fleet_json` / `build_fleet_json_at` shape + `online` derivation
//! - Schema-lock golden test (byte-match against fixtures/export/fleet_export.golden.json)
//! - `build_cf_json` empty-but-valid
//! - `build_path_health_json` empty-but-valid
//! - `write_fleet_yaml` still works (no regression from Task 5)

use chrono::DateTime;
use fleet::export::{
    build_cf_json, build_fleet_json, build_fleet_json_at, build_path_health_json, write_fleet_yaml,
};
use fleet::model::{DedupeKind, Node, Tags, TailnetRef, Tier};

/// Build a deterministic test node.
///
/// `fleet_id` is caller-supplied for flexibility. `online` maps to the bool
/// stored on `Node` (derived from `last_seen` at sync time; we set it directly
/// here since these are in-memory fixtures, not DB rows).
fn fixture_node(online: bool) -> Node {
    // A fixed UTC timestamp used both for last_seen and first_seen/updated_at.
    let ts: DateTime<chrono::Utc> = "2026-01-01T00:00:00Z".parse().unwrap();
    Node {
        fleet_id: "mk:abc123".to_owned(),
        hostname: "nas-01".to_owned(),
        fqdn: "nas-01.ts.net".to_owned(),
        seen_in: vec![TailnetRef {
            account: "personal".to_owned(),
            device_id: "999".to_owned(),
        }],
        addresses: vec!["100.64.0.1".to_owned()],
        os: "linux".to_owned(),
        online,
        last_seen: ts,
        tags: Tags {
            role: Some("nas".to_owned()),
            owner: Some("self".to_owned()),
            site: Some("local".to_owned()),
            gpu: None,
            raw: vec![
                "tag:role-nas".to_owned(),
                "tag:owner-self".to_owned(),
                "tag:site-local".to_owned(),
            ],
        },
        tier: Tier::Agent,
        dedupe_key_kind: DedupeKind::Machinekey,
        notes: None,
        first_seen: ts,
        updated_at: ts,
        fuzzy_hint: None,
    }
}

// ── shape test ────────────────────────────────────────────────────────────────

#[test]
fn build_fleet_json_shape() {
    let node = fixture_node(true);
    let export = build_fleet_json(&[node]);

    // generated_at is present (non-empty, roughly ISO 8601 — dynamic value)
    assert!(
        !export.generated_at.is_empty(),
        "generated_at must not be empty"
    );
    assert!(
        export.generated_at.contains('T'),
        "generated_at should be ISO 8601, got: {}",
        export.generated_at
    );

    assert_eq!(export.nodes.len(), 1);
    let n = &export.nodes[0];
    assert_eq!(n.id, "mk:abc123");
    assert_eq!(n.hostname, "nas-01");
    assert_eq!(n.tier, "agent");
    assert_eq!(n.online, 1u8, "online node must export online=1");
    assert_eq!(n.site.as_deref(), Some("local"));
    assert_eq!(n.role.as_deref(), Some("nas"));
    assert_eq!(n.owner.as_deref(), Some("self"));
    assert_eq!(n.last_seen, "2026-01-01T00:00:00Z");
}

#[test]
fn online_derivation_offline_node() {
    let node = fixture_node(false);
    let export = build_fleet_json(&[node]);
    assert_eq!(
        export.nodes[0].online, 0u8,
        "offline node must export online=0"
    );
}

// ── schema-lock golden test ───────────────────────────────────────────────────

#[test]
fn schema_lock_golden() {
    // Build the export with a fixed generated_at so output is deterministic.
    let node = fixture_node(true);
    let export = build_fleet_json_at(&[node], "2026-01-01T00:00:00Z".to_owned());

    // Pretty-print exactly as the golden file is formatted.
    let actual = serde_json::to_string_pretty(&export).unwrap();

    let golden_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/export/fleet_export.golden.json"
    );
    let expected = std::fs::read_to_string(golden_path)
        .unwrap_or_else(|e| panic!("Could not read golden file {golden_path}: {e}"));

    // Normalise trailing newline so the comparison is OS-independent.
    assert_eq!(
        actual.trim_end(),
        expected.trim_end(),
        "JSON export does not match golden file — a field rename or reorder broke the schema lock.\n\
         To update: regenerate {golden_path} with the new output and review the diff in git."
    );
}

// ── cf builder ────────────────────────────────────────────────────────────────

#[test]
fn cf_builder_empty_valid() {
    let cf = build_cf_json(&[]);
    let json = serde_json::to_string(&cf).unwrap();
    assert_eq!(json, r#"{"zones":[]}"#);
}

#[test]
fn cf_builder_passthrough() {
    let zone = serde_json::json!({"name": "example.com", "healthy": true});
    let cf = build_cf_json(std::slice::from_ref(&zone));
    assert_eq!(cf.zones.len(), 1);
    assert_eq!(cf.zones[0], zone);
}

// ── path-health builder ───────────────────────────────────────────────────────

#[test]
fn path_health_builder_empty_valid() {
    let ph = build_path_health_json(&[]);
    let json = serde_json::to_string(&ph).unwrap();
    assert_eq!(json, r#"{"hops":[]}"#);
}

#[test]
fn path_health_builder_passthrough() {
    let hop = serde_json::json!({"ttl": 1, "host": "192.168.1.1", "loss_pct": 0.0});
    let ph = build_path_health_json(std::slice::from_ref(&hop));
    assert_eq!(ph.hops.len(), 1);
    assert_eq!(ph.hops[0], hop);
}

// ── YAML snapshot regression (Task 5 must not be broken) ─────────────────────

#[test]
fn write_fleet_yaml_not_regressed() {
    let node = fixture_node(true);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fleet.yaml");
    write_fleet_yaml(&[node], &path).unwrap();
    let yaml = std::fs::read_to_string(&path).unwrap();
    assert!(yaml.contains("fleet_id"), "fleet_id must appear in YAML");
    assert!(
        !yaml.contains("online"),
        "volatile field 'online' must not appear in YAML"
    );
    assert!(
        !yaml.contains("last_seen"),
        "volatile field 'last_seen' must not appear in YAML"
    );
    assert!(
        !yaml.contains("updated_at"),
        "volatile field 'updated_at' must not appear in YAML"
    );
}

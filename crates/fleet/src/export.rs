//! Export builders and git-tracked YAML snapshot (spec §3.6, §3.7, §3.8).
//!
//! ## YAML snapshot
//! The snapshot must be **stable across syncs that change only volatile fields**
//! so the git diff is meaningful. We therefore serialize a [`FleetYamlNode`]
//! projection that **excludes** `last_seen`, `online`, and `updated_at`. Nodes
//! are sorted by `fleet_id` for a deterministic byte output.
//!
//! ## JSON builders
//! [`build_fleet_json`] / [`build_fleet_json_at`], [`build_cf_json`], and
//! [`build_path_health_json`] are the single source of truth for the JSON shapes
//! consumed by both the CLI `--json` output and `fleet serve`'s `/api/*` endpoints
//! (Task 16). These structs are **public** so `serve` reuses them directly.

use crate::model::{DedupeKind, Node, Tier};
use anyhow::Context;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The stable, exported projection of a [`Node`]. Volatile fields
/// (`last_seen`, `online`, `updated_at`) are intentionally omitted; `notes`,
/// `first_seen`, and empty tag facets are omitted when absent for a tidy diff.
#[derive(Debug, Serialize)]
pub struct FleetYamlNode {
    pub fleet_id: String,
    pub hostname: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub fqdn: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub os: String,
    pub tier: Tier,
    pub dedupe_key_kind: DedupeKind,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub raw_tags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub seen_in: Vec<SeenInYaml>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// A `(account, device_id)` provenance pair in the export.
#[derive(Debug, Serialize)]
pub struct SeenInYaml {
    pub account: String,
    pub device_id: String,
}

impl FleetYamlNode {
    fn from_node(n: &Node) -> Self {
        Self {
            fleet_id: n.fleet_id.clone(),
            hostname: n.hostname.clone(),
            fqdn: n.fqdn.clone(),
            os: n.os.clone(),
            tier: n.tier,
            dedupe_key_kind: n.dedupe_key_kind,
            addresses: n.addresses.clone(),
            role: n.tags.role.clone(),
            owner: n.tags.owner.clone(),
            site: n.tags.site.clone(),
            gpu: n.tags.gpu.clone(),
            raw_tags: n.tags.raw.clone(),
            seen_in: n
                .seen_in
                .iter()
                .map(|s| SeenInYaml {
                    account: s.account.clone(),
                    device_id: s.device_id.clone(),
                })
                .collect(),
            notes: n.notes.clone(),
        }
    }
}

/// Top-level YAML document: `nodes: [...]`.
#[derive(Debug, Serialize)]
struct FleetYaml {
    nodes: Vec<FleetYamlNode>,
}

/// Render the stable YAML snapshot to bytes (the testable core).
pub fn render_fleet_yaml(nodes: &[Node]) -> anyhow::Result<String> {
    let mut sorted: Vec<&Node> = nodes.iter().collect();
    sorted.sort_by(|a, b| a.fleet_id.cmp(&b.fleet_id));
    let doc = FleetYaml {
        nodes: sorted.iter().map(|n| FleetYamlNode::from_node(n)).collect(),
    };
    serde_yaml_ng::to_string(&doc).context("serializing fleet.yaml")
}

/// Write the stable YAML snapshot for `nodes` to `path` (sorted by `fleet_id`).
pub fn write_fleet_yaml(nodes: &[Node], path: &Path) -> anyhow::Result<()> {
    let yaml = render_fleet_yaml(nodes)?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating dir {}", parent.display()))?;
    }
    std::fs::write(path, yaml).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

// ── JSON export builders (spec §3.7 / §3.8) ─────────────────────────────────

/// Top-level fleet JSON export (`/api/fleet`). Public so `fleet serve` reuses it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetExport {
    pub generated_at: String,
    pub nodes: Vec<NodeExport>,
}

/// Per-node JSON projection. `online` is `u8` (1 = online, 0 = offline) so
/// the dashboard can render status without a string comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeExport {
    pub id: String,
    pub hostname: String,
    pub tier: String,
    /// 1 = online, 0 = offline (derived from `Node::online`).
    pub online: u8,
    pub site: Option<String>,
    pub role: Option<String>,
    pub owner: Option<String>,
    pub last_seen: String,
}

/// Cloudflare zone export (`/api/cf`). Zones are `serde_json::Value` stubs until
/// `fleet cf-sync` lands. Struct is public and reused by `fleet serve`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CfExport {
    pub zones: Vec<serde_json::Value>,
}

/// MTR path-health export (`/api/path-health`). Hops are `serde_json::Value`
/// stubs until `fleet probe` lands. Public, reused by `fleet serve`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathHealthExport {
    pub hops: Vec<serde_json::Value>,
}

/// Build the fleet JSON export, stamping `generated_at` with [`Utc::now()`].
pub fn build_fleet_json(nodes: &[Node]) -> FleetExport {
    build_fleet_json_at(nodes, Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// Build the fleet JSON export with a **caller-supplied** `generated_at` string.
///
/// Use this variant in tests to produce deterministic output.
pub fn build_fleet_json_at(nodes: &[Node], generated_at: String) -> FleetExport {
    let exports = nodes
        .iter()
        .map(|n| NodeExport {
            id: n.fleet_id.clone(),
            hostname: n.hostname.clone(),
            tier: match n.tier {
                Tier::Agent => "agent".to_owned(),
                Tier::Agentless => "agentless".to_owned(),
            },
            online: u8::from(n.online),
            site: n.tags.site.clone(),
            role: n.tags.role.clone(),
            owner: n.tags.owner.clone(),
            last_seen: n.last_seen.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        })
        .collect();
    FleetExport {
        generated_at,
        nodes: exports,
    }
}

/// Build an empty-but-valid Cloudflare export. Populated by `fleet cf-sync` (Task 9).
pub fn build_cf_json(zones: &[serde_json::Value]) -> CfExport {
    CfExport {
        zones: zones.to_vec(),
    }
}

/// Build an empty-but-valid path-health export. Populated by `fleet probe` (Task 10).
pub fn build_path_health_json(hops: &[serde_json::Value]) -> PathHealthExport {
    PathHealthExport {
        hops: hops.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DedupeKind, Tags, TailnetRef};
    use chrono::{Duration, Utc};

    fn node(fleet_id: &str) -> Node {
        let now = Utc::now();
        Node {
            fleet_id: fleet_id.to_owned(),
            hostname: "host".to_owned(),
            fqdn: "host.ts.net".to_owned(),
            seen_in: vec![TailnetRef {
                account: "personal".to_owned(),
                device_id: "1".to_owned(),
            }],
            addresses: vec!["100.64.0.1".to_owned()],
            os: "linux".to_owned(),
            online: true,
            last_seen: now,
            tags: Tags {
                role: Some("worker".to_owned()),
                owner: Some("self".to_owned()),
                site: None,
                gpu: None,
                raw: vec!["tag:role-worker".to_owned()],
            },
            tier: Tier::Agent,
            dedupe_key_kind: DedupeKind::Machinekey,
            notes: None,
            first_seen: now,
            updated_at: now,
            fuzzy_hint: None,
        }
    }

    #[test]
    fn excludes_volatile_fields() {
        let yaml = render_fleet_yaml(&[node("a")]).unwrap();
        assert!(!yaml.contains("last_seen"), "last_seen leaked:\n{yaml}");
        assert!(!yaml.contains("online"), "online leaked:\n{yaml}");
        assert!(!yaml.contains("updated_at"), "updated_at leaked:\n{yaml}");
        assert!(yaml.contains("fleet_id"));
        assert!(yaml.contains("role: worker"));
    }

    #[test]
    fn stable_across_volatile_change() {
        let a = node("x");
        let mut b = node("x");
        b.last_seen = a.last_seen + Duration::hours(1);
        b.updated_at = a.updated_at + Duration::hours(2);
        b.online = !a.online;
        assert_eq!(
            render_fleet_yaml(&[a]).unwrap(),
            render_fleet_yaml(&[b]).unwrap(),
            "only volatile fields differ → identical YAML"
        );
    }

    #[test]
    fn sorted_by_fleet_id() {
        let yaml = render_fleet_yaml(&[node("zeta"), node("alpha")]).unwrap();
        let a = yaml.find("alpha").unwrap();
        let z = yaml.find("zeta").unwrap();
        assert!(a < z, "alpha must precede zeta");
    }
}

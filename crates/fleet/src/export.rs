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
//!
//! [`build_ports_json`] / [`build_ports_json_at`] and
//! [`build_workloads_json`] / [`build_workloads_json_at`] are the schema-locked
//! builders for `/api/ports` and `/api/workloads` (spec §6.4 / C9). Per-row
//! `stale: bool` is computed via `model::is_stale`. Port numbers are `u16`
//! (a JSON number) and `stale` is a boolean — both are golden-locked by tests.

use crate::db::host::{FleetPortRow, FleetWorkloadRow};
use crate::model::{DedupeKind, Node, Tier, is_stale};
use anyhow::Context;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

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

// ── Ports JSON export (spec §6.4 / C9) ──────────────────────────────────────

/// Top-level response body for `GET /api/ports`. Schema-locked — a field rename
/// or type change breaks the golden test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortsExport {
    pub generated_at: String,
    pub rows: Vec<PortRowExport>,
}

/// Per-row projection for `/api/ports`. Field names, types, and ordering are
/// schema-locked: `port` is `u16` (JSON number), `stale` is `bool` (JSON boolean).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortRowExport {
    pub hostname: String,
    pub fleet_id: String,
    /// Port number as a JSON **number** (u16). A rename or type change breaks
    /// the schema-lock golden test.
    pub port: u16,
    pub proto: String,
    pub process: String,
    pub pid: i64,
    pub bind: String,
    pub collected_at: String,
    /// `true` when the snapshot is older than the stale threshold.
    /// JSON **boolean** — not a string. Schema-locked.
    pub stale: bool,
}

/// Build the ports JSON export, stamping `generated_at` with [`Utc::now()`].
pub fn build_ports_json(rows: &[FleetPortRow], stale_threshold: Duration) -> PortsExport {
    build_ports_json_at(
        rows,
        Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        stale_threshold,
    )
}

/// Build the ports JSON export with a **caller-supplied** `generated_at` string.
/// Use this variant in tests to produce deterministic output.
pub fn build_ports_json_at(
    rows: &[FleetPortRow],
    generated_at: String,
    stale_threshold: Duration,
) -> PortsExport {
    PortsExport {
        generated_at,
        rows: rows
            .iter()
            .map(|r| PortRowExport {
                hostname: r.hostname.clone(),
                fleet_id: r.node_id.clone(),
                port: r.port,
                proto: r.proto.clone(),
                process: r.process.clone(),
                pid: r.pid,
                bind: r.bind.clone(),
                collected_at: r.collected_at.clone(),
                stale: is_stale(&r.collected_at, stale_threshold),
            })
            .collect(),
    }
}

// ── Workloads JSON export (spec §6.4 / C9) ───────────────────────────────────

/// Top-level response body for `GET /api/workloads`. Schema-locked.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadsExport {
    pub generated_at: String,
    pub rows: Vec<WorkloadRowExport>,
}

/// Per-row projection for `/api/workloads`. Field names and types are
/// schema-locked: `stale` is `bool` (JSON boolean).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadRowExport {
    pub hostname: String,
    pub fleet_id: String,
    pub label: String,
    pub category: String,
    pub process_count: i64,
    pub total_cpu_percent: f64,
    pub total_memory_bytes: i64,
    pub example_command: String,
    pub collected_at: String,
    /// `true` when the snapshot is older than the stale threshold.
    /// JSON **boolean** — not a string. Schema-locked.
    pub stale: bool,
}

/// Build the workloads JSON export, stamping `generated_at` with [`Utc::now()`].
pub fn build_workloads_json(
    rows: &[FleetWorkloadRow],
    stale_threshold: Duration,
) -> WorkloadsExport {
    build_workloads_json_at(
        rows,
        Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        stale_threshold,
    )
}

/// Build the workloads JSON export with a **caller-supplied** `generated_at` string.
/// Use this variant in tests to produce deterministic output.
pub fn build_workloads_json_at(
    rows: &[FleetWorkloadRow],
    generated_at: String,
    stale_threshold: Duration,
) -> WorkloadsExport {
    WorkloadsExport {
        generated_at,
        rows: rows
            .iter()
            .map(|r| WorkloadRowExport {
                hostname: r.hostname.clone(),
                fleet_id: r.node_id.clone(),
                label: r.label.clone(),
                category: r.category.clone(),
                process_count: r.process_count,
                total_cpu_percent: r.total_cpu_percent,
                total_memory_bytes: r.total_memory_bytes,
                example_command: r.example_command.clone(),
                collected_at: r.collected_at.clone(),
                stale: is_stale(&r.collected_at, stale_threshold),
            })
            .collect(),
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

    // ── C9 export builder tests ──────────────────────────────────────────────

    fn fresh_port_row() -> crate::db::host::FleetPortRow {
        crate::db::host::FleetPortRow {
            node_id: "fleet-01".to_owned(),
            hostname: "host-a".to_owned(),
            collected_at: (Utc::now() - Duration::minutes(1)).to_rfc3339(),
            port: 8080,
            proto: "TCP".to_owned(),
            process: "nginx".to_owned(),
            pid: 1234,
            bind: "0.0.0.0".to_owned(),
        }
    }

    fn stale_port_row() -> crate::db::host::FleetPortRow {
        crate::db::host::FleetPortRow {
            node_id: "fleet-02".to_owned(),
            hostname: "host-b".to_owned(),
            collected_at: (Utc::now() - Duration::hours(4)).to_rfc3339(),
            port: 443,
            proto: "TCP".to_owned(),
            process: "caddy".to_owned(),
            pid: 5678,
            bind: "127.0.0.1".to_owned(),
        }
    }

    fn fresh_workload_row() -> crate::db::host::FleetWorkloadRow {
        crate::db::host::FleetWorkloadRow {
            node_id: "fleet-01".to_owned(),
            hostname: "host-a".to_owned(),
            collected_at: (Utc::now() - Duration::minutes(1)).to_rfc3339(),
            label: "llama.cpp".to_owned(),
            category: "inference".to_owned(),
            process_count: 2,
            total_cpu_percent: 42.5,
            total_memory_bytes: 4_000_000_000,
            example_command: "/usr/bin/llama-run model.gguf".to_owned(),
            workload_count: 2,
        }
    }

    fn stale_workload_row() -> crate::db::host::FleetWorkloadRow {
        crate::db::host::FleetWorkloadRow {
            node_id: "fleet-02".to_owned(),
            hostname: "host-b".to_owned(),
            collected_at: (Utc::now() - Duration::hours(4)).to_rfc3339(),
            label: "ollama".to_owned(),
            category: "llm".to_owned(),
            process_count: 1,
            total_cpu_percent: 10.0,
            total_memory_bytes: 2_000_000_000,
            example_command: "/usr/bin/ollama serve".to_owned(),
            workload_count: 1,
        }
    }

    const THREE_HOURS: std::time::Duration = std::time::Duration::from_secs(3 * 3600);

    /// build_ports_json_at produces deterministic output and correct stale flags
    #[test]
    fn build_ports_json_at_stale_flag() {
        let rows = vec![fresh_port_row(), stale_port_row()];
        let export = build_ports_json_at(
            rows.as_slice(),
            "2026-06-22T00:00:00Z".to_owned(),
            THREE_HOURS,
        );
        assert_eq!(export.generated_at, "2026-06-22T00:00:00Z");
        assert_eq!(export.rows.len(), 2);
        assert!(!export.rows[0].stale, "fresh row must not be stale");
        assert!(export.rows[1].stale, "4h-old row must be stale");
    }

    /// port is a u16 (number) and stale is a bool — schema-lock type check
    #[test]
    fn build_ports_json_at_port_is_number_stale_is_bool() {
        let rows = vec![fresh_port_row()];
        let export = build_ports_json_at(
            rows.as_slice(),
            "2026-06-22T00:00:00Z".to_owned(),
            THREE_HOURS,
        );
        let json = serde_json::to_value(&export).unwrap();
        let row = &json["rows"][0];
        assert!(
            row["port"].is_number(),
            "port must be a JSON number, got: {}",
            row["port"]
        );
        assert!(
            row["stale"].is_boolean(),
            "stale must be a JSON boolean, got: {}",
            row["stale"]
        );
        assert_eq!(row["port"].as_u64(), Some(8080), "port value mismatch");
    }

    /// build_ports_json_at field names are locked — rename breaks this test
    #[test]
    fn build_ports_json_at_golden_key_paths() {
        let rows = vec![fresh_port_row()];
        let export = build_ports_json_at(
            rows.as_slice(),
            "2026-06-22T00:00:00Z".to_owned(),
            THREE_HOURS,
        );
        let json = serde_json::to_value(&export).unwrap();
        let row = &json["rows"][0];
        for key in &[
            "hostname",
            "fleet_id",
            "port",
            "proto",
            "process",
            "pid",
            "bind",
            "collected_at",
            "stale",
        ] {
            assert!(
                row.get(*key).is_some(),
                "missing port row key: {key} — field rename breaks golden contract"
            );
        }
    }

    /// build_workloads_json_at produces correct stale flags
    #[test]
    fn build_workloads_json_at_stale_flag() {
        let rows = vec![fresh_workload_row(), stale_workload_row()];
        let export = build_workloads_json_at(
            rows.as_slice(),
            "2026-06-22T00:00:00Z".to_owned(),
            THREE_HOURS,
        );
        assert_eq!(export.generated_at, "2026-06-22T00:00:00Z");
        assert_eq!(export.rows.len(), 2);
        assert!(!export.rows[0].stale, "fresh workload must not be stale");
        assert!(export.rows[1].stale, "4h-old workload must be stale");
    }

    /// build_workloads_json_at stale is bool — schema-lock type check
    #[test]
    fn build_workloads_json_at_stale_is_bool() {
        let rows = vec![fresh_workload_row()];
        let export = build_workloads_json_at(
            rows.as_slice(),
            "2026-06-22T00:00:00Z".to_owned(),
            THREE_HOURS,
        );
        let json = serde_json::to_value(&export).unwrap();
        let row = &json["rows"][0];
        assert!(
            row["stale"].is_boolean(),
            "stale must be a JSON boolean, got: {}",
            row["stale"]
        );
    }

    /// build_workloads_json_at field names are locked — rename breaks this test
    #[test]
    fn build_workloads_json_at_golden_key_paths() {
        let rows = vec![fresh_workload_row()];
        let export = build_workloads_json_at(
            rows.as_slice(),
            "2026-06-22T00:00:00Z".to_owned(),
            THREE_HOURS,
        );
        let json = serde_json::to_value(&export).unwrap();
        let row = &json["rows"][0];
        for key in &[
            "hostname",
            "fleet_id",
            "label",
            "category",
            "process_count",
            "total_cpu_percent",
            "total_memory_bytes",
            "example_command",
            "collected_at",
            "stale",
        ] {
            assert!(
                row.get(*key).is_some(),
                "missing workload row key: {key} — field rename breaks golden contract"
            );
        }
    }
}

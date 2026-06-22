//! `fleet-overrides.yaml` loader + per-node attribute layering (spec §3.2, §3.5).
//!
//! Two layers live here:
//!   - **Aliases** collapse multiple `(account, device_id)` pairs under one
//!     canonical `fleet_id` *before* the pure merge groups devices. The merge
//!     consumes the minimal [`crate::merge::Overrides`] (an alias `BTreeMap`);
//!     [`FullOverrides::to_merge_overrides`] flattens the alias members into it.
//!   - **Per-node overrides** layer `tags` / `tier` / `notes` over a folded
//!     [`Node`] (precedence: override > parsed tag > default). [`apply`] runs
//!     this and then derives the real [`Tier`] when no explicit tier was set.
//!
//! **Cross-owner guard (R-overrides / spec §3.5):** an alias whose members span
//! two different *inferred* `owner` facets is a load-time error unless
//! `ack_cross_owner: true`. Owner is inferred from the account name: an account
//! starting with `client-` → `client-<suffix>`, anything else → `self`. A
//! `nodes[fleet_id].tags.owner = "self"` override on an alias that has a
//! `client-*` member flips client→self and emits a stderr warning.

use crate::merge::Overrides as MergeOverrides;
use crate::model::{Node, Tier, parse_tags};
use anyhow::Context;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

// ─── YAML shape ──────────────────────────────────────────────────────────────

/// A single `(account, device_id)` member of an alias.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AliasMember {
    pub account: String,
    pub device_id: String,
}

/// One alias entry: a canonical `fleet_id` and the members that collapse into it.
#[derive(Debug, Clone, Deserialize)]
pub struct AliasEntry {
    pub fleet_id: String,
    #[serde(default)]
    pub members: Vec<AliasMember>,
    /// Acknowledge an intentional cross-owner alias (bypasses the guard).
    #[serde(default)]
    pub ack_cross_owner: bool,
}

/// Per-node attribute override layered over a folded [`Node`].
#[derive(Debug, Clone, Deserialize, Default)]
pub struct NodeOverride {
    #[serde(default)]
    pub tags: NodeTagsOverride,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// The `tags:` sub-map of a per-node override (any subset of the four facets).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct NodeTagsOverride {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub gpu: Option<String>,
}

/// The fully-parsed `fleet-overrides.yaml`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct FullOverrides {
    #[serde(default)]
    pub aliases: Vec<AliasEntry>,
    #[serde(default)]
    pub nodes: BTreeMap<String, NodeOverride>,
}

// ─── Owner inference ─────────────────────────────────────────────────────────

/// Infer the `owner` facet from an account name (spec §3.5 guard).
/// `client-<x>` → `client-<x>`; anything else → `self`.
fn infer_owner(account: &str) -> String {
    if account.starts_with("client-") {
        account.to_owned()
    } else {
        "self".to_owned()
    }
}

// ─── Loader ──────────────────────────────────────────────────────────────────

/// Load and validate `fleet-overrides.yaml`. A missing file is **not** an error
/// (overrides are optional) — it yields an empty [`FullOverrides`]. The
/// cross-owner guard runs at load time.
pub fn load(path: &Path) -> anyhow::Result<FullOverrides> {
    if !path.exists() {
        return Ok(FullOverrides::default());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading overrides from {}", path.display()))?;
    load_str(&raw)
}

/// Parse + validate overrides from a YAML string (the testable core of [`load`]).
pub fn load_str(raw: &str) -> anyhow::Result<FullOverrides> {
    let parsed: FullOverrides =
        serde_yaml_ng::from_str(raw).context("parsing fleet-overrides.yaml")?;
    validate(&parsed)?;
    Ok(parsed)
}

/// Cross-owner guard + owner-flip warning (spec §3.5).
fn validate(ov: &FullOverrides) -> anyhow::Result<()> {
    for alias in &ov.aliases {
        // Distinct inferred owners across this alias's members.
        let mut owners: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for m in &alias.members {
            owners.insert(infer_owner(&m.account));
        }
        if owners.len() > 1 && !alias.ack_cross_owner {
            anyhow::bail!(
                "cross-owner alias `{}` spans owners {:?} — set `ack_cross_owner: true` to allow",
                alias.fleet_id,
                owners
            );
        }

        // Owner-flip warning: override forces owner=self on an alias whose
        // members include a client-* account.
        if let Some(node_ov) = ov.nodes.get(&alias.fleet_id)
            && node_ov.tags.owner.as_deref() == Some("self")
        {
            let has_client = alias
                .members
                .iter()
                .any(|m| m.account.starts_with("client-"));
            if has_client {
                eprintln!(
                    "warning: override flips owner→self for alias `{}` which has a client-* member",
                    alias.fleet_id
                );
            }
        }
    }
    Ok(())
}

// ─── Alias lookup + merge bridge ─────────────────────────────────────────────

impl FullOverrides {
    /// Return the alias `fleet_id` for a device, if declared.
    pub fn alias_for(&self, account: &str, device_id: &str) -> Option<&str> {
        for alias in &self.aliases {
            if alias
                .members
                .iter()
                .any(|m| m.account == account && m.device_id == device_id)
            {
                return Some(&alias.fleet_id);
            }
        }
        None
    }

    /// Flatten the alias members into the minimal [`MergeOverrides`] the pure
    /// merge consumes.
    pub fn to_merge_overrides(&self) -> MergeOverrides {
        let mut aliases = BTreeMap::new();
        for alias in &self.aliases {
            for m in &alias.members {
                aliases.insert(
                    (m.account.clone(), m.device_id.clone()),
                    alias.fleet_id.clone(),
                );
            }
        }
        MergeOverrides { aliases }
    }
}

/// Free-function alias lookup mirroring [`FullOverrides::alias_for`] for the
/// spec's named surface.
pub fn alias_for<'a>(ov: &'a FullOverrides, account: &str, device_id: &str) -> Option<&'a str> {
    ov.alias_for(account, device_id)
}

// ─── Per-node layering + tier derivation ─────────────────────────────────────

/// Layer per-node overrides over a folded [`Node`] and derive its real tier
/// (spec §3.5 step 7). Precedence: override > parsed tag > default. The merge
/// fold has already parsed tags into facets; this overwrites present override
/// facets, applies `notes`, and resolves `tier`.
pub fn apply(node: &mut Node, ov: &FullOverrides) {
    // Re-parse from raw to guarantee a clean facet baseline (idempotent).
    let parsed = parse_tags(&node.tags.raw);
    node.tags.role = parsed.role;
    node.tags.owner = parsed.owner;
    node.tags.site = parsed.site;
    node.tags.gpu = parsed.gpu;

    let mut explicit_tier: Option<Tier> = None;

    if let Some(no) = ov.nodes.get(&node.fleet_id) {
        if no.tags.role.is_some() {
            node.tags.role = no.tags.role.clone();
        }
        if no.tags.owner.is_some() {
            node.tags.owner = no.tags.owner.clone();
        }
        if no.tags.site.is_some() {
            node.tags.site = no.tags.site.clone();
        }
        if no.tags.gpu.is_some() {
            node.tags.gpu = no.tags.gpu.clone();
        }
        if let Some(n) = &no.notes {
            node.notes = Some(n.clone());
        }
        if let Some(t) = &no.tier {
            explicit_tier = match t.to_lowercase().as_str() {
                "agent" => Some(Tier::Agent),
                "agentless" => Some(Tier::Agentless),
                _ => None,
            };
        }
    }

    node.tier = explicit_tier.unwrap_or_else(|| derive_tier(node));
}

/// Derive the tier from owner/os/role (spec §3.2): **agent** when
/// `owner == self` AND `os ∈ {macOS, linux}` AND
/// `role ∈ {host, worker, nas, inference, hub}`; else **agentless**.
fn derive_tier(node: &Node) -> Tier {
    let owner_self = node.tags.owner.as_deref() == Some("self");
    let os = node.os.to_lowercase();
    let os_ok = os == "macos" || os == "linux";
    let role_ok = matches!(
        node.tags.role.as_deref(),
        Some("host" | "worker" | "nas" | "inference" | "hub")
    );
    if owner_self && os_ok && role_ok {
        Tier::Agent
    } else {
        Tier::Agentless
    }
}

// ─── Test helper (shared with commands::sync tests) ──────────────────────────

/// Build a minimal [`Node`] for cross-module tests.
#[cfg(test)]
pub fn tests_helper_node(fleet_id: &str, os: &str) -> Node {
    use crate::model::{DedupeKind, Tags};
    let now = chrono::Utc::now();
    Node {
        fleet_id: fleet_id.to_owned(),
        hostname: "h".to_owned(),
        fqdn: "h.ts.net".to_owned(),
        seen_in: vec![],
        addresses: vec![],
        os: os.to_owned(),
        online: true,
        last_seen: now,
        tags: Tags::default(),
        tier: Tier::Agentless,
        dedupe_key_kind: DedupeKind::Machinekey,
        notes: None,
        first_seen: now,
        updated_at: now,
        fuzzy_hint: None,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DedupeKind;
    use chrono::Utc;

    fn node(fleet_id: &str, os: &str, raw: Vec<&str>) -> Node {
        let now = Utc::now();
        Node {
            fleet_id: fleet_id.to_owned(),
            hostname: "h".to_owned(),
            fqdn: "h.ts.net".to_owned(),
            seen_in: vec![],
            addresses: vec![],
            os: os.to_owned(),
            online: true,
            last_seen: now,
            tags: parse_tags(&raw.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
            tier: Tier::Agentless,
            dedupe_key_kind: DedupeKind::Fuzzy,
            notes: None,
            first_seen: now,
            updated_at: now,
            fuzzy_hint: None,
        }
    }

    #[test]
    fn alias_collapse_before_grouping() {
        let yaml = r#"
aliases:
  - fleet_id: nas-01
    members:
      - { account: personal, device_id: "111" }
      - { account: client-acme, device_id: "222" }
    ack_cross_owner: true
"#;
        let ov = load_str(yaml).unwrap();
        assert_eq!(alias_for(&ov, "personal", "111"), Some("nas-01"));
        assert_eq!(alias_for(&ov, "client-acme", "222"), Some("nas-01"));
        // And the merge bridge flattens both members.
        let m = ov.to_merge_overrides();
        assert_eq!(m.alias_for("personal", "111"), Some("nas-01"));
        assert_eq!(m.alias_for("client-acme", "222"), Some("nas-01"));
    }

    #[test]
    fn per_node_layering_precedence() {
        let yaml = r#"
nodes:
  nas-01:
    tags:
      role: host
      site: local
    tier: agent
    notes: "My NAS"
"#;
        let ov = load_str(yaml).unwrap();
        // Parsed tags say role=worker, owner=self; override forces role=host.
        let mut n = node("nas-01", "linux", vec!["tag:role-worker", "tag:owner-self"]);
        apply(&mut n, &ov);
        assert_eq!(n.tags.role.as_deref(), Some("host"), "override wins");
        assert_eq!(
            n.tags.owner.as_deref(),
            Some("self"),
            "absent → falls through"
        );
        assert_eq!(n.tags.site.as_deref(), Some("local"));
        assert_eq!(n.tier, Tier::Agent, "explicit override tier wins");
        assert_eq!(n.notes.as_deref(), Some("My NAS"));
    }

    #[test]
    fn tier_derived_when_not_overridden() {
        let ov = FullOverrides::default();
        // owner=self, os=linux, role=worker → agent
        let mut a = node("a", "linux", vec!["tag:owner-self", "tag:role-worker"]);
        apply(&mut a, &ov);
        assert_eq!(a.tier, Tier::Agent);
        // client owner → agentless
        let mut b = node("b", "linux", vec!["tag:owner-client-x", "tag:role-worker"]);
        apply(&mut b, &ov);
        assert_eq!(b.tier, Tier::Agentless);
        // mobile os → agentless
        let mut c = node("c", "iOS", vec!["tag:owner-self", "tag:role-worker"]);
        apply(&mut c, &ov);
        assert_eq!(c.tier, Tier::Agentless);
        // router role → agentless
        let mut d = node("d", "linux", vec!["tag:owner-self", "tag:role-router"]);
        apply(&mut d, &ov);
        assert_eq!(d.tier, Tier::Agentless);
    }

    #[test]
    fn cross_owner_guard_error() {
        let yaml = r#"
aliases:
  - fleet_id: nas-01
    members:
      - { account: personal, device_id: "111" }
      - { account: client-acme, device_id: "222" }
"#;
        let err = load_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("cross-owner"),
            "expected cross-owner error, got: {err}"
        );
    }

    #[test]
    fn cross_owner_guard_ack_bypass() {
        let yaml = r#"
aliases:
  - fleet_id: nas-01
    members:
      - { account: personal, device_id: "111" }
      - { account: client-acme, device_id: "222" }
    ack_cross_owner: true
"#;
        let ov = load_str(yaml).expect("ack bypasses the guard");
        assert_eq!(ov.aliases.len(), 1);
    }

    #[test]
    fn owner_flip_warning() {
        // Override forces owner=self on an alias with a client-* member.
        // Must NOT error (only warns to stderr). All members same inferred owner
        // would dodge the cross-owner guard, so use ack to focus on the warning.
        let yaml = r#"
aliases:
  - fleet_id: nas-01
    members:
      - { account: client-acme, device_id: "222" }
nodes:
  nas-01:
    tags:
      owner: self
"#;
        let ov = load_str(yaml).expect("owner flip warns but does not error");
        assert_eq!(
            ov.nodes.get("nas-01").unwrap().tags.owner.as_deref(),
            Some("self")
        );
    }
}

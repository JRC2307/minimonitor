//! `fleet list [--tag <facet:value>] [--tier <t>] [--online] [--json]`
//!
//! Pure SQLite read with optional filters. `--online` **recomputes** freshness
//! at query time and never trusts the stored `online` flag.

use crate::db::nodes::{ListFilter, list_filtered};
use crate::model::{DedupeKind, Node, Tier};
use anyhow::Result;
use chrono::Utc;
use std::time::Duration;

/// Run `fleet list` with the given filter options. Returns the matching nodes.
/// Callers decide whether to print a table or JSON.
pub fn run(
    conn: &rusqlite::Connection,
    tag: Option<&str>,
    tier: Option<&str>,
    online_only: bool,
    json: bool,
    online_threshold: Duration,
) -> Result<()> {
    let filter = build_filter(tag, tier)?;
    let mut nodes = list_filtered(conn, &filter)?;

    if online_only {
        nodes.retain(|n| crate::model::is_online(n.last_seen, online_threshold));
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&nodes)?);
    } else {
        print_table(&nodes, online_threshold);
    }

    Ok(())
}

pub struct ListOptions<'a> {
    pub tag: Option<&'a str>,
    pub tier: Option<&'a str>,
    pub online_only: bool,
    pub online_threshold: Duration,
}

/// Build a [`ListFilter`] from CLI flag strings.
fn build_filter(tag: Option<&str>, tier: Option<&str>) -> Result<ListFilter> {
    let mut filter = ListFilter::default();

    if let Some(t) = tag {
        let (facet, value) = t
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("--tag must be <facet>:<value>, got {:?}", t))?;
        filter.tag_facet = Some(facet.to_owned());
        filter.tag_value = Some(value.to_owned());
    }

    if let Some(tier_str) = tier {
        filter.tier = Some(match tier_str {
            "agent" => Tier::Agent,
            "agentless" => Tier::Agentless,
            other => anyhow::bail!("unknown tier {:?} (expected agent|agentless)", other),
        });
    }

    Ok(filter)
}

/// Print a compact aligned table.
///
/// Columns: hostname | tier | online | site | role | owner | last_seen | fuzzy
pub fn print_table(nodes: &[Node], threshold: Duration) {
    if nodes.is_empty() {
        println!("(no nodes)");
        return;
    }

    // Header
    println!(
        "{:<20} {:<10} {:<3} {:<12} {:<12} {:<14} {:<16} ",
        "HOSTNAME", "TIER", "ON", "SITE", "ROLE", "OWNER", "LAST_SEEN"
    );
    println!("{}", "-".repeat(95));

    for n in nodes {
        let online_sym = if is_online_now(n, threshold) {
            "●"
        } else {
            "○"
        };
        let tier_str = match n.tier {
            Tier::Agent => "agent",
            Tier::Agentless => "agentless",
        };
        let site = n.tags.site.as_deref().unwrap_or("-");
        let role = n.tags.role.as_deref().unwrap_or("-");
        let owner = n.tags.owner.as_deref().unwrap_or("-");
        let last_seen = format_relative(n.last_seen);
        let fuzzy = if n.dedupe_key_kind == DedupeKind::Fuzzy {
            "~"
        } else {
            ""
        };

        println!(
            "{:<20} {:<10} {:<3} {:<12} {:<12} {:<14} {:<16} {}",
            truncate(&n.hostname, 20),
            tier_str,
            online_sym,
            truncate(site, 12),
            truncate(role, 12),
            truncate(owner, 14),
            last_seen,
            fuzzy
        );
    }
}

fn is_online_now(n: &Node, threshold: Duration) -> bool {
    crate::model::is_online(n.last_seen, threshold)
}

fn format_relative(dt: chrono::DateTime<Utc>) -> String {
    let secs = Utc::now().signed_duration_since(dt).num_seconds().max(0) as u64;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max { s } else { &s[..max] }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::nodes::upsert_node;
    use crate::db::open;
    use crate::model::{DedupeKind, Node, Tags, Tier};
    use chrono::{Duration as CDuration, Utc};
    use tempfile::NamedTempFile;

    fn make_node(id: &str) -> Node {
        let now = Utc::now();
        Node {
            fleet_id: id.to_owned(),
            hostname: format!("host-{id}"),
            fqdn: format!("host-{id}.local"),
            seen_in: vec![],
            addresses: vec!["100.1.2.3".to_owned()],
            os: "linux".to_owned(),
            online: true,
            last_seen: now,
            tags: Tags {
                role: Some("host".to_owned()),
                owner: Some("self".to_owned()),
                site: Some("local".to_owned()),
                gpu: None,
                raw: vec![],
            },
            tier: Tier::Agent,
            dedupe_key_kind: DedupeKind::Machinekey,
            notes: None,
            first_seen: now,
            updated_at: now,
            fuzzy_hint: None,
        }
    }

    fn open_with_nodes(nodes: Vec<Node>) -> (NamedTempFile, rusqlite::Connection) {
        let f = NamedTempFile::new().unwrap();
        let conn = open(f.path()).unwrap();
        for n in nodes {
            upsert_node(&conn, &n).unwrap();
        }
        (f, conn)
    }

    #[test]
    fn filter_by_tag_role() {
        let mut worker = make_node("w1");
        worker.tags.role = Some("worker".to_owned());

        let mut host = make_node("h1");
        host.tags.role = Some("host".to_owned());

        let (_f, conn) = open_with_nodes(vec![worker, host]);

        let filter = build_filter(Some("role:host"), None).unwrap();
        let nodes = list_filtered(&conn, &filter).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].tags.role.as_deref(), Some("host"));
    }

    #[test]
    fn filter_by_tier_agent() {
        let mut agentless = make_node("ag1");
        agentless.tier = Tier::Agentless;

        let agent = make_node("ag2");

        let (_f, conn) = open_with_nodes(vec![agentless, agent]);

        let filter = build_filter(None, Some("agent")).unwrap();
        let nodes = list_filtered(&conn, &filter).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].tier, Tier::Agent);
    }

    #[test]
    fn online_recompute_marks_stale_node_offline() {
        // Node with stored online=true but last_seen 1 hour ago → must render ○
        let mut stale = make_node("stale1");
        stale.last_seen = Utc::now() - CDuration::hours(1);
        stale.online = true; // stored flag is "on" — must NOT be trusted

        let (_f, conn) = open_with_nodes(vec![stale]);

        let filter = ListFilter::default();
        let nodes = list_filtered(&conn, &filter).unwrap();
        assert_eq!(nodes.len(), 1);

        // With 15 min threshold, 1 hour ago is stale
        let threshold = Duration::from_secs(900);
        assert!(
            !is_online_now(&nodes[0], threshold),
            "stale node must render ○"
        );
    }

    #[test]
    fn online_filter_excludes_stale() {
        let mut stale = make_node("stale2");
        stale.last_seen = Utc::now() - CDuration::hours(1);
        stale.online = true;

        let fresh = make_node("fresh1");

        let (_f, conn) = open_with_nodes(vec![stale, fresh]);

        let filter = ListFilter::default();
        let mut nodes = list_filtered(&conn, &filter).unwrap();

        let threshold = Duration::from_secs(900);
        nodes.retain(|n| crate::model::is_online(n.last_seen, threshold));

        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].fleet_id, "fresh1");
    }

    #[test]
    fn json_flag_emits_vec_node() {
        let node = make_node("j1");
        let (_f, conn) = open_with_nodes(vec![node.clone()]);

        let filter = ListFilter::default();
        let nodes = list_filtered(&conn, &filter).unwrap();

        // Verify serde round-trip produces a JSON array with fleet_id
        let json_str = serde_json::to_string(&nodes).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["fleet_id"], "j1");
    }

    #[test]
    fn fuzzy_rows_get_tilde_marker() {
        let mut fuzzy_node = make_node("fz1");
        fuzzy_node.dedupe_key_kind = DedupeKind::Fuzzy;

        let exact_node = make_node("mk1");

        let (_f, conn) = open_with_nodes(vec![fuzzy_node, exact_node]);

        let filter = ListFilter::default();
        let nodes = list_filtered(&conn, &filter).unwrap();

        for n in &nodes {
            if n.fleet_id == "fz1" {
                assert_eq!(
                    n.dedupe_key_kind,
                    DedupeKind::Fuzzy,
                    "fz1 must be Fuzzy kind"
                );
            }
        }
    }

    #[test]
    fn tag_filter_rejects_missing_colon() {
        let err = build_filter(Some("rolehost"), None);
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("facet"));
    }

    #[test]
    fn tier_filter_rejects_unknown() {
        let err = build_filter(None, Some("superagent"));
        assert!(err.is_err());
    }

    #[test]
    fn filter_by_owner_via_tag() {
        let mut self_node = make_node("own1");
        self_node.tags.owner = Some("self".to_owned());

        let mut client_node = make_node("own2");
        client_node.tags.owner = Some("client-acme".to_owned());

        let (_f, conn) = open_with_nodes(vec![self_node, client_node]);

        let filter = build_filter(Some("owner:self"), None).unwrap();
        let nodes = list_filtered(&conn, &filter).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].tags.owner.as_deref(), Some("self"));
    }
}

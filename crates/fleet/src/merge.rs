//! Pure multi-tailnet merge + dedupe (spec §3.4) and the `online` derivation (§3.3).
//!
//! NO I/O happens here. The merge takes already-fetched device lists, an alias
//! map, and the previously-minted fuzzy ids, and folds them into one [`Node`]
//! per physical box.
//!
//! ## Merge key precedence ladder (§3.4 step 2)
//!   1. **alias** — an operator-declared `(account, device_id) → fleet_id`.
//!   2. **`mk:<machineKey>`** — the robust same-physical-box signal.
//!   3. **`fz:<slug(hostname)>|<os>`** — fuzzy last resort.
//!
//! ## Colliding-hostname decision (brief point 3)
//! Two *different-machineKey* boxes that share `hostname` + `os` and have no
//! alias would collapse under the same `fz:` key and be wrongly merged. To keep
//! them SEPARATE we detect that collision: when more than one distinct
//! `(account, device_id)` with **distinct** identity hashes into the same `fz:`
//! key, we disambiguate each by appending the per-account device id to its fuzzy
//! key (`fz:<slug>|<os>#<account>:<device_id>`). The result: same-box re-sightings
//! (same hostname/os, picked up again across syncs) still re-link via the prior
//! fuzzy-hint map, but two genuinely different `worker` boxes stay as two Nodes,
//! each flagged `DedupeKind::Fuzzy`.

use crate::model::{DedupeKind, Node, Tags, TailnetRef, Tier, TsDevice, slugify};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::time::Duration;

/// Default freshness window: 15 minutes (spec §3.3).
pub const DEFAULT_ONLINE_THRESHOLD: Duration = Duration::from_secs(900);

/// Derive presence from `last_seen` freshness (§3.3). There is no `online`
/// boolean in the Tailscale API. A `last_seen` in the future or otherwise
/// un-subtractable yields `false` (offline) rather than panicking.
pub fn is_online(last_seen: DateTime<Utc>, max_age: Duration) -> bool {
    Utc::now()
        .signed_duration_since(last_seen)
        .to_std()
        .map(|age| age < max_age)
        .unwrap_or(false)
}

/// Operator-declared alias map: `(account, device_id) → fleet_id`.
///
/// Minimal Task-4 surface. The full loader + cross-owner guard live in
/// `overrides.rs` (Task 5); this struct is what the pure merge consumes.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    /// Keyed by `(account, device_id)`, value is the canonical `fleet_id`.
    pub aliases: BTreeMap<(String, String), String>,
}

impl Overrides {
    /// Return the alias `fleet_id` for a device, if one is declared.
    pub fn alias_for(&self, account: &str, device_id: &str) -> Option<&str> {
        self.aliases
            .get(&(account.to_owned(), device_id.to_owned()))
            .map(String::as_str)
    }
}

/// Previously-minted fuzzy ids, so a renamed fuzzy box re-links to its existing
/// minted `n-<8hex>` id instead of forking a new identity (§3.4 step 6).
///
/// Keyed by `fuzzy_hint` (the `fz:...` string that was current when the id was
/// first minted).
#[derive(Debug, Clone, Default)]
pub struct PriorIds {
    /// `fuzzy_hint` → previously-minted `fleet_id`.
    pub by_fuzzy_hint: BTreeMap<String, String>,
}

impl PriorIds {
    /// Look up a previously-minted id for a fuzzy hint.
    pub fn get(&self, fuzzy_hint: &str) -> Option<&str> {
        self.by_fuzzy_hint.get(fuzzy_hint).map(String::as_str)
    }
}

/// The merge key for a single device, with its confidence kind.
fn merge_key(d: &TsDevice, ov: &Overrides) -> (String, DedupeKind) {
    if let Some(id) = ov.alias_for(&d.account, &d.id) {
        return (id.to_owned(), DedupeKind::Alias);
    }
    if !d.machine_key.is_empty() {
        return (format!("mk:{}", d.machine_key), DedupeKind::Machinekey);
    }
    (
        format!("fz:{}|{}", slugify(&d.hostname), d.os.to_lowercase()),
        DedupeKind::Fuzzy,
    )
}

/// Deterministically mint `n-<8hex>` from a fuzzy key using FNV-1a (no RNG, no
/// extra crates). Same key → same id on every run, which is what lets a re-link
/// fall back to recomputation when the prior map is empty.
fn mint_fuzzy_id(fuzzy_key: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for b in fuzzy_key.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("n-{:08x}", (hash & 0xffff_ffff) as u32)
}

/// Merge per-account device lists into one [`Node`] per physical box (§3.4).
///
/// - `per_account`: `(account_name, devices)` as fetched from each tailnet.
/// - `overrides`: alias map (and, in Task 5, attribute layering).
/// - `prior`: previously-minted fuzzy ids for re-link stability.
/// - `threshold`: freshness window for the derived `online` flag.
/// - `include_unauthorized`: keep `authorized == false` devices when true.
pub fn merge(
    per_account: Vec<(String, Vec<TsDevice>)>,
    overrides: &Overrides,
    prior: &PriorIds,
    threshold: Duration,
    include_unauthorized: bool,
) -> Vec<Node> {
    // ── Step 1: collect & filter ────────────────────────────────────────────
    let mut devices: Vec<TsDevice> = Vec::new();
    for (account, list) in per_account {
        for mut d in list {
            d.account = account.clone();
            if d.is_external {
                continue; // shared-in devices pollute inventory
            }
            if !d.authorized && !include_unauthorized {
                continue;
            }
            devices.push(d);
        }
    }

    // ── Step 2: compute merge keys ──────────────────────────────────────────
    // Pre-pass for the colliding-hostname guard: count distinct device
    // identities per fuzzy key. If >1 distinct identity collides on the same
    // `fz:` key, those devices are genuinely different boxes and must NOT merge.
    let mut fz_identities: BTreeMap<String, std::collections::BTreeSet<(String, String)>> =
        BTreeMap::new();
    for d in &devices {
        if let (key, DedupeKind::Fuzzy) = merge_key(d, overrides) {
            fz_identities
                .entry(key)
                .or_default()
                .insert((d.account.clone(), d.id.clone()));
        }
    }

    // ── Step 3: group by (possibly disambiguated) merge key ─────────────────
    struct Group {
        key: String,
        kind: DedupeKind,
        devices: Vec<TsDevice>,
    }
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();

    for d in devices {
        let (mut key, kind) = merge_key(&d, overrides);
        if kind == DedupeKind::Fuzzy {
            // Colliding-hostname guard: if this fuzzy key has >1 distinct
            // identity, append the per-account device id so each box gets its
            // own group (stays separate). A lone identity keeps the bare `fz:`
            // key so re-sightings across syncs re-link via the prior map.
            if fz_identities.get(&key).map(|s| s.len()).unwrap_or(0) > 1 {
                key = format!("{key}#{}:{}", d.account, d.id);
            }
        }
        groups
            .entry(key.clone())
            .or_insert_with(|| Group {
                key,
                kind,
                devices: Vec::new(),
            })
            .devices
            .push(d);
    }

    // ── Steps 4–6: fold each group to a Node ────────────────────────────────
    let now = Utc::now();
    let mut nodes: Vec<Node> = Vec::new();
    for (_k, group) in groups {
        // Step 4: canonical row = freshest lastSeen.
        let canonical = group
            .devices
            .iter()
            .max_by_key(|d| d.last_seen)
            .expect("group is non-empty");

        // Step 5: fold.
        let mut seen_in: Vec<TailnetRef> = group
            .devices
            .iter()
            .map(|d| TailnetRef {
                account: d.account.clone(),
                device_id: d.id.clone(),
            })
            .collect();
        seen_in.sort_by(|a, b| {
            (a.account.as_str(), a.device_id.as_str())
                .cmp(&(b.account.as_str(), b.device_id.as_str()))
        });

        let mut addresses: Vec<String> = group
            .devices
            .iter()
            .flat_map(|d| d.addresses.iter().cloned())
            .collect();
        addresses.sort();
        addresses.dedup();

        let last_seen = group
            .devices
            .iter()
            .map(|d| d.last_seen)
            .max()
            .unwrap_or(canonical.last_seen);

        let raw_tags: Vec<String> = {
            let mut t: Vec<String> = group
                .devices
                .iter()
                .flat_map(|d| d.tags.iter().cloned())
                .collect();
            t.sort();
            t.dedup();
            t
        };

        // Step 6: id minting.
        let (fleet_id, fuzzy_hint) = match group.kind {
            DedupeKind::Machinekey | DedupeKind::Alias => (group.key.clone(), None),
            DedupeKind::Fuzzy => {
                // The fuzzy hint is the bare (un-disambiguated) part used for
                // re-link; but when disambiguated we keep the full key so the
                // hint is per-box stable.
                let hint = group.key.clone();
                let id = prior
                    .get(&hint)
                    .map(str::to_owned)
                    .unwrap_or_else(|| mint_fuzzy_id(&hint));
                (id, Some(hint))
            }
        };

        nodes.push(Node {
            fleet_id,
            hostname: canonical.hostname.clone(),
            fqdn: canonical.name.clone(),
            seen_in,
            addresses,
            os: canonical.os.clone(),
            online: is_online(last_seen, threshold),
            last_seen,
            tags: Tags {
                raw: raw_tags,
                ..Tags::default()
            },
            tier: Tier::Agentless, // §3.5 step 7 (Task 5) layers the real tier
            dedupe_key_kind: group.kind,
            notes: fuzzy_hint, // carried as a re-link hint; Task 5 overwrites notes
            first_seen: last_seen,
            updated_at: now,
        });
    }

    nodes.sort_by(|a, b| a.fleet_id.cmp(&b.fleet_id));
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn dev(account: &str, id: &str, hostname: &str, mk: &str, os: &str, ts: &str) -> TsDevice {
        TsDevice {
            id: id.to_owned(),
            hostname: hostname.to_owned(),
            name: format!("{hostname}.example.ts.net"),
            machine_key: mk.to_owned(),
            os: os.to_owned(),
            addresses: vec![],
            tags: vec![],
            is_external: false,
            authorized: true,
            last_seen: DateTime::parse_from_rfc3339(ts)
                .unwrap()
                .with_timezone(&Utc),
            account: account.to_owned(),
            ..Default::default()
        }
    }

    fn fresh() -> String {
        Utc::now().to_rfc3339()
    }

    #[test]
    fn is_online_true_within_window_false_outside() {
        let recent = Utc::now() - chrono::Duration::seconds(60);
        assert!(is_online(recent, DEFAULT_ONLINE_THRESHOLD));
        let stale = Utc::now() - chrono::Duration::seconds(2000);
        assert!(!is_online(stale, DEFAULT_ONLINE_THRESHOLD));
        // Future timestamp -> offline, not a panic.
        let future = Utc::now() + chrono::Duration::seconds(120);
        assert!(!is_online(future, DEFAULT_ONLINE_THRESHOLD));
    }

    #[test]
    fn clean_machinekey_merge_across_accounts() {
        let mut a = dev(
            "personal",
            "111",
            "nas",
            "mkey:same",
            "linux",
            "2026-06-20T10:00:00Z",
        );
        a.addresses = vec!["100.64.0.1".into()];
        let mut b = dev(
            "client-acme",
            "999",
            "nas",
            "mkey:same",
            "linux",
            "2026-06-20T12:00:00Z",
        );
        b.addresses = vec!["100.99.0.5".into(), "100.64.0.1".into()];

        let nodes = merge(
            vec![
                ("personal".into(), vec![a]),
                ("client-acme".into(), vec![b]),
            ],
            &Overrides::default(),
            &PriorIds::default(),
            DEFAULT_ONLINE_THRESHOLD,
            false,
        );

        assert_eq!(nodes.len(), 1, "same machineKey must collapse to one node");
        let n = &nodes[0];
        assert_eq!(n.dedupe_key_kind, DedupeKind::Machinekey);
        assert_eq!(n.fleet_id, "mk:mkey:same");
        assert_eq!(n.seen_in.len(), 2);
        // sorted union of addresses
        assert_eq!(n.addresses, vec!["100.64.0.1", "100.99.0.5"]);
        // last_seen = max
        assert_eq!(
            n.last_seen,
            Utc.with_ymd_and_hms(2026, 6, 20, 12, 0, 0).unwrap()
        );
    }

    #[test]
    fn canonical_row_is_freshest() {
        let old = dev(
            "personal",
            "1",
            "oldname",
            "mkey:x",
            "linux",
            "2026-06-20T08:00:00Z",
        );
        let new = dev(
            "client",
            "2",
            "newname",
            "mkey:x",
            "macOS",
            "2026-06-20T20:00:00Z",
        );
        let nodes = merge(
            vec![("personal".into(), vec![old]), ("client".into(), vec![new])],
            &Overrides::default(),
            &PriorIds::default(),
            DEFAULT_ONLINE_THRESHOLD,
            false,
        );
        assert_eq!(nodes.len(), 1);
        // hostname/fqdn/os come from the freshest (new) row
        assert_eq!(nodes[0].hostname, "newname");
        assert_eq!(nodes[0].os, "macOS");
        assert_eq!(nodes[0].fqdn, "newname.example.ts.net");
    }

    #[test]
    fn wiped_state_collapses_via_alias() {
        // Two different machineKeys (box was wiped) — alias map says they're the same.
        let a = dev(
            "personal",
            "111",
            "nas",
            "mkey:old",
            "linux",
            "2026-06-20T10:00:00Z",
        );
        let b = dev(
            "client-acme",
            "222",
            "nas",
            "mkey:new",
            "linux",
            "2026-06-20T11:00:00Z",
        );

        let mut overrides = Overrides::default();
        overrides
            .aliases
            .insert(("personal".into(), "111".into()), "nas-01".into());
        overrides
            .aliases
            .insert(("client-acme".into(), "222".into()), "nas-01".into());

        let nodes = merge(
            vec![
                ("personal".into(), vec![a]),
                ("client-acme".into(), vec![b]),
            ],
            &overrides,
            &PriorIds::default(),
            DEFAULT_ONLINE_THRESHOLD,
            false,
        );
        assert_eq!(nodes.len(), 1, "alias must collapse two machineKeys");
        assert_eq!(nodes[0].fleet_id, "nas-01");
        assert_eq!(nodes[0].dedupe_key_kind, DedupeKind::Alias);
        assert_eq!(nodes[0].seen_in.len(), 2);
    }

    #[test]
    fn colliding_hostnames_stay_separate() {
        // Two unrelated `worker` boxes, different machineKeys, same hostname+os,
        // NO alias, NO machineKey (empty) → fuzzy. They must NOT merge.
        let a = dev(
            "client-a",
            "1",
            "worker",
            "",
            "linux",
            "2026-06-20T10:00:00Z",
        );
        let b = dev(
            "client-b",
            "2",
            "worker",
            "",
            "linux",
            "2026-06-20T11:00:00Z",
        );

        let nodes = merge(
            vec![("client-a".into(), vec![a]), ("client-b".into(), vec![b])],
            &Overrides::default(),
            &PriorIds::default(),
            DEFAULT_ONLINE_THRESHOLD,
            false,
        );
        assert_eq!(nodes.len(), 2, "colliding hostnames must stay separate");
        for n in &nodes {
            assert_eq!(n.dedupe_key_kind, DedupeKind::Fuzzy);
        }
        assert_ne!(nodes[0].fleet_id, nodes[1].fleet_id);
    }

    #[test]
    fn external_and_unauthorized_filtered() {
        let mut ext = dev("personal", "1", "shared", "", "linux", &fresh());
        ext.is_external = true;
        let mut unauth = dev("personal", "2", "pending", "mkey:p", "linux", &fresh());
        unauth.authorized = false;
        let ok = dev("personal", "3", "good", "mkey:g", "linux", &fresh());

        let nodes = merge(
            vec![("personal".into(), vec![ext, unauth, ok])],
            &Overrides::default(),
            &PriorIds::default(),
            DEFAULT_ONLINE_THRESHOLD,
            false,
        );
        assert_eq!(nodes.len(), 1, "external + unauthorized dropped");
        assert_eq!(nodes[0].hostname, "good");
    }

    #[test]
    fn unauthorized_kept_when_flag_set() {
        let mut unauth = dev("personal", "2", "pending", "mkey:p", "linux", &fresh());
        unauth.authorized = false;
        let nodes = merge(
            vec![("personal".into(), vec![unauth])],
            &Overrides::default(),
            &PriorIds::default(),
            DEFAULT_ONLINE_THRESHOLD,
            true,
        );
        assert_eq!(nodes.len(), 1, "include_unauthorized keeps the device");
    }

    #[test]
    fn fuzzy_mint_then_relink_on_rename() {
        // First sync: a lone fuzzy box mints n-<8hex>.
        let first = dev(
            "personal",
            "7",
            "buildbox",
            "",
            "linux",
            "2026-06-20T10:00:00Z",
        );
        let nodes1 = merge(
            vec![("personal".into(), vec![first])],
            &Overrides::default(),
            &PriorIds::default(),
            DEFAULT_ONLINE_THRESHOLD,
            false,
        );
        assert_eq!(nodes1.len(), 1);
        let minted = nodes1[0].fleet_id.clone();
        assert!(minted.starts_with("n-"), "fuzzy id minted: {minted}");
        let hint = nodes1[0].notes.clone().expect("fuzzy hint carried");

        // Build the prior map from the first sync.
        let mut prior = PriorIds::default();
        prior.by_fuzzy_hint.insert(hint.clone(), minted.clone());

        // Second sync: SAME box (same hostname+os → same fuzzy hint) but renamed
        // device id. It must re-link to the SAME minted id, not fork.
        let renamed = dev(
            "personal",
            "8",
            "buildbox",
            "",
            "linux",
            "2026-06-21T10:00:00Z",
        );
        let nodes2 = merge(
            vec![("personal".into(), vec![renamed])],
            &Overrides::default(),
            &prior,
            DEFAULT_ONLINE_THRESHOLD,
            false,
        );
        assert_eq!(nodes2.len(), 1);
        assert_eq!(
            nodes2[0].fleet_id, minted,
            "renamed fuzzy box re-links to same id"
        );
    }
}

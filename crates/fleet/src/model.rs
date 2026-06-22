use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct Node {
    pub fleet_id: String,
    pub hostname: String,
    pub fqdn: String,
    pub seen_in: Vec<TailnetRef>,
    pub addresses: Vec<String>,
    pub os: String,
    pub online: bool,
    pub last_seen: DateTime<Utc>,
    pub tags: Tags,
    pub tier: Tier,
    pub dedupe_key_kind: DedupeKind,
    pub notes: Option<String>,
    pub first_seen: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// The `fz:...` re-link hint for fuzzy-merged boxes, persisted to
    /// `node_seen.fuzzy_hint` and reloaded into [`crate::merge::PriorIds`] at the
    /// next sync. `None` for machinekey/alias nodes. Never exported to YAML.
    #[serde(skip)]
    pub fuzzy_hint: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct TailnetRef {
    pub account: String,
    pub device_id: String,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug, Default)]
pub struct Tags {
    pub role: Option<String>,
    pub owner: Option<String>,
    pub site: Option<String>,
    pub gpu: Option<String>,
    pub raw: Vec<String>,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Agent,
    Agentless,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum DedupeKind {
    Machinekey,
    Alias,
    Fuzzy,
}

/// A device as returned by the Tailscale REST API
/// (`GET /api/v2/tailnet/{tailnet}/devices?fields=default`).
///
/// Field names are camelCase on the wire (`#[serde(rename_all = "camelCase")]`).
/// `account` is NOT part of the API payload — it is injected by the client after
/// deserialization so the pure merge layer knows which tailnet each row came from.
///
/// `last_seen` is parsed from an RFC3339 string that carries a **non-UTC offset**
/// (e.g. `-05:00`) and normalized to UTC via `parse_from_rfc3339().with_timezone(&Utc)`.
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct TsDevice {
    /// Per-tailnet DeviceID (stable within one tailnet, NOT across accounts).
    pub id: String,
    /// MagicDNS hostname (short name).
    #[serde(default)]
    pub hostname: String,
    /// Fully-qualified MagicDNS name.
    #[serde(default)]
    pub name: String,
    /// Robust same-physical-box signal; stable across re-auth within a node state dir.
    /// Empty for shared-in (external) devices.
    #[serde(default)]
    pub machine_key: String,
    /// Ephemeral node key; changes on re-registration.
    #[serde(default)]
    pub node_key: String,
    /// macOS|linux|windows|iOS|android
    #[serde(default)]
    pub os: String,
    /// Tailnet IPs (100.x).
    #[serde(default)]
    pub addresses: Vec<String>,
    /// ACL tags (flat strings).
    #[serde(default)]
    pub tags: Vec<String>,
    /// True for shared-in devices that would pollute inventory; dropped in merge.
    #[serde(default)]
    pub is_external: bool,
    /// False until an admin approves the device; dropped unless include_unauthorized.
    #[serde(default = "default_authorized")]
    pub authorized: bool,
    /// RFC3339 timestamp with a possibly non-UTC offset; normalized to UTC on read.
    #[serde(default = "epoch_utc", with = "ts_rfc3339")]
    pub last_seen: DateTime<Utc>,
    /// Injected by the client (the configured tailnet account name), NOT from the API.
    #[serde(default)]
    pub account: String,
}

fn default_authorized() -> bool {
    true
}

fn epoch_utc() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(0, 0).unwrap()
}

/// Serde adapter that parses an RFC3339 string with any offset and normalizes
/// to UTC. Empty strings deserialize to the Unix epoch (offline).
mod ts_rfc3339 {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(dt: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&dt.to_rfc3339())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        let raw = String::deserialize(d)?;
        if raw.is_empty() {
            return Ok(super::epoch_utc());
        }
        DateTime::parse_from_rfc3339(&raw)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(serde::de::Error::custom)
    }
}

// FleetId newtype
use regex::Regex;
use std::sync::OnceLock;

static FLEET_ID_RE: OnceLock<Regex> = OnceLock::new();

fn fleet_id_regex() -> &'static Regex {
    FLEET_ID_RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9._:-]+$").unwrap())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetId(String);

impl FleetId {
    pub fn new(s: &str) -> anyhow::Result<Self> {
        if s.is_empty() {
            anyhow::bail!("fleet_id must not be empty");
        }
        if !fleet_id_regex().is_match(s) {
            anyhow::bail!(
                "fleet_id {:?} contains invalid characters (allowed: A-Za-z0-9._:-)",
                s
            );
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for FleetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Derive online status from `last_seen` freshness.
///
/// Never trusts a stored `online` flag — recomputes at call time.
/// Returns `false` if `last_seen` is in the future (clock skew / unparseable).
pub fn is_online(last_seen: chrono::DateTime<Utc>, max_age: std::time::Duration) -> bool {
    Utc::now()
        .signed_duration_since(last_seen)
        .to_std()
        .map(|age| age < max_age)
        .unwrap_or(false)
}

/// Slugify a hostname: lowercase, map chars outside [A-Za-z0-9._:-] to '-'.
/// A leading '-' is neutralized by prepending 'n' so it can't be an ssh option.
pub fn slugify(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            let lc = c.to_ascii_lowercase();
            if lc.is_ascii_alphanumeric() || lc == '.' || lc == '_' || lc == ':' || lc == '-' {
                lc
            } else {
                '-'
            }
        })
        .collect();
    if out.starts_with('-') {
        out.insert(0, 'n');
    }
    out
}

/// Parse Tailscale's flat `tag:<facet>-<value>` strings into the four
/// [`Tags`] facets (spec §3.2). Known facet prefixes are `role-`, `owner-`,
/// `site-`, `gpu-`; the substring after the prefix is the value. A `tag:`
/// prefix on the raw string is stripped first. Unmatched tags are dropped from
/// the facets (they remain in `raw` via the caller). Last-write-wins per facet.
pub fn parse_tags(raw: &[String]) -> Tags {
    let mut tags = Tags {
        raw: raw.to_vec(),
        ..Tags::default()
    };
    for t in raw {
        let body = t.strip_prefix("tag:").unwrap_or(t);
        if let Some(v) = body.strip_prefix("role-") {
            tags.role = Some(v.to_owned());
        } else if let Some(v) = body.strip_prefix("owner-") {
            tags.owner = Some(v.to_owned());
        } else if let Some(v) = body.strip_prefix("site-") {
            tags.site = Some(v.to_owned());
        } else if let Some(v) = body.strip_prefix("gpu-") {
            tags.gpu = Some(v.to_owned());
        }
    }
    tags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tags_extracts_facets() {
        let raw = vec![
            "tag:role-worker".to_owned(),
            "tag:owner-client-acme".to_owned(),
            "tag:site-local".to_owned(),
            "tag:gpu-none".to_owned(),
            "tag:unrelated".to_owned(),
        ];
        let t = parse_tags(&raw);
        assert_eq!(t.role.as_deref(), Some("worker"));
        assert_eq!(t.owner.as_deref(), Some("client-acme"));
        assert_eq!(t.site.as_deref(), Some("local"));
        assert_eq!(t.gpu.as_deref(), Some("none"));
        assert_eq!(t.raw.len(), 5, "raw retains all tags");
    }

    #[test]
    fn fleet_id_accepts_valid() {
        assert!(FleetId::new("nas-01").is_ok());
        assert!(FleetId::new("mk:abc.def").is_ok());
        assert!(FleetId::new("n-1a2b3c4d").is_ok());
    }

    #[test]
    fn fleet_id_rejects_invalid() {
        assert!(FleetId::new("foo;rm -rf").is_err());
        assert!(FleetId::new("-oProxyCommand=x").is_err());
        assert!(FleetId::new("a\"b").is_err());
        assert!(FleetId::new("a`b").is_err());
        assert!(FleetId::new("").is_err());
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Worker-01.local"), "worker-01.local");
    }

    #[test]
    fn slugify_strips_bad_chars() {
        assert_eq!(slugify("foo;bar"), "foo-bar");
        assert_eq!(slugify("a\"b"), "a-b");
        assert_eq!(slugify("a`b"), "a-b");
    }

    #[test]
    fn slugify_leading_dash_neutralized() {
        let result = slugify("-oProxyCommand=x");
        assert!(
            !result.starts_with('-'),
            "leading dash not neutralized: {result}"
        );
    }
}

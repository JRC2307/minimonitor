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

#[cfg(test)]
mod tests {
    use super::*;

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

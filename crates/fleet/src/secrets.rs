//! Secret resolution and redaction helpers.
//!
//! Resolution order (spec §7 / R-8):
//!   1. `FLEET_<env_var>` environment variable (non-empty)
//!   2. macOS Keychain via `security find-generic-password -s <svc> -a fleet -w`
//!   3. Hard error naming BOTH the env var AND the keychain service
//!
//! The Keychain shell-out is injectable (a function pointer) so tests can
//! supply a stub without touching the real `security` binary.
//!
//! Redaction strips Authorization headers and credential-bearing URLs from
//! `anyhow::Error` chains before they reach logs or stderr.

use anyhow::Context;
use regex::Regex;
use std::sync::OnceLock;

// ─── Resolution ──────────────────────────────────────────────────────────────

/// Public type alias for the injectable Keychain resolver function.
///
/// Must be a plain function pointer (no captures) so it can be used as a
/// `const`-compatible function reference in production code.
/// Tests that need a closure can use `resolve_with_keychain_fn` instead.
pub type KeychainFn = fn(&str) -> anyhow::Result<String>;

/// Resolve a secret, accepting any callable (fn pointer OR closure) as the
/// Keychain backend. This is the primary entry point for tests.
///
/// 1. If `env_var` is set to a non-empty value, return it immediately — no
///    Keychain call is made.
/// 2. Otherwise call `keychain` with `keychain_service`.
/// 3. If both fail, return an error that names both sources.
pub fn resolve_with_keychain<F>(
    env_var: &str,
    keychain_service: &str,
    keychain: F,
) -> anyhow::Result<String>
where
    F: Fn(&str) -> anyhow::Result<String>,
{
    // 1. Env first.
    if let Some(v) = std::env::var(env_var).ok().filter(|v| !v.is_empty()) {
        return Ok(v);
    }
    // 2. Keychain.
    match keychain(keychain_service) {
        Ok(v) if !v.is_empty() => return Ok(v),
        Ok(_) => {}  // empty value from keychain → fall through to error
        Err(_) => {} // keychain failed → fall through to error
    }
    // 3. Hard error naming both sources.
    anyhow::bail!(
        "secret unresolved: env var `{env_var}` not set (or empty), \
         and keychain service `{keychain_service}` returned nothing"
    )
}

/// Convenience wrapper that uses the real macOS Keychain shell-out.
///
/// For use in production code. Tests inject `keychain_absent_fn` or a custom stub.
pub fn resolve(env_var: &str, keychain_service: &str) -> anyhow::Result<String> {
    resolve_with_keychain(env_var, keychain_service, keychain_shell)
}

/// The real macOS Keychain resolver: shells to `security find-generic-password`.
fn keychain_shell(service: &str) -> anyhow::Result<String> {
    let out = std::process::Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", "fleet", "-w"])
        .output()
        .context("failed to run `security` — is this macOS?")?;
    anyhow::ensure!(
        out.status.success(),
        "security find-generic-password failed for service `{}`",
        service
    );
    let value = String::from_utf8(out.stdout)
        .context("keychain value is not valid UTF-8")?
        .trim()
        .to_owned();
    Ok(value)
}

/// A `KeychainFn` stub that always fails — use in tests to avoid real Keychain.
pub fn keychain_absent_fn(_svc: &str) -> anyhow::Result<String> {
    anyhow::bail!("keychain not available in tests (stub)")
}

// ─── Redaction ───────────────────────────────────────────────────────────────

/// Regex patterns compiled once.
fn bearer_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)Bearer\s+\S+").unwrap())
}

/// Matches the secret key segment in hc-ping.com URLs:
/// `https://hc-ping.com/<KEY>/<slug>` — captures and removes `<KEY>`.
fn hc_ping_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Replace the key (path segment after hc-ping.com/) with [REDACTED]
        Regex::new(r"(hc-ping\.com/)([A-Za-z0-9_-]+)(/[^\s?]*)?").unwrap()
    })
}

/// Matches credential-bearing ntfy token URLs:
/// `https://<token>@<host>/` or `Authorization: Bearer <token>`
/// (Bearer is already handled above; this covers any other tokenized URL segments.)
fn ntfy_token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(https?://)([^@\s]+@)").unwrap())
}

/// Apply all redaction patterns to a string.
fn redact_str(s: &str) -> String {
    let s = bearer_re().replace_all(s, "Bearer [REDACTED]");
    let s = hc_ping_re().replace_all(&s, "$1[REDACTED]$3");
    let s = ntfy_token_re().replace_all(&s, "$1[REDACTED]@");
    s.into_owned()
}

/// Redact an `anyhow::Error` chain, stripping `Bearer` tokens and
/// credential-bearing URLs before returning a displayable wrapper.
///
/// The returned type implements `Display` and `std::error::Error`.
pub fn redact(err: anyhow::Error) -> anyhow::Error {
    let chain = format!("{err:#}");
    let clean = redact_str(&chain);
    anyhow::anyhow!("{}", clean)
}

/// Redact an error that may contain the hc-ping `ping_key`.
///
/// Applies all standard redaction patterns first, then does a **literal**
/// replacement of the raw `ping_key` value (R-8 — must never appear in error
/// output, regardless of which base URL was used, including wiremock hosts in
/// tests). The `slug` is **not** treated as a secret and is not scrubbed.
/// Any hc-ping URL token already removed by the standard `redact_str` patterns
/// is also covered.
pub fn redact_ping(err: anyhow::Error, ping_key: &str) -> anyhow::Error {
    let chain = format!("{err:#}");
    let clean = redact_str(&chain);
    // Literal replacement of the raw ping_key value — catches any URL shape
    // (hc-ping.com, wiremock 127.0.0.1, proxies, etc.).  Must come after
    // redact_str so that Bearer/ntfy patterns are handled first.
    let clean = if !ping_key.is_empty() {
        clean.replace(ping_key, "[REDACTED]")
    } else {
        clean
    };
    anyhow::anyhow!("{}", clean)
}

// ─── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_redacted() {
        let s = "Authorization: Bearer tk_abc123xyz";
        let out = redact_str(s);
        assert!(!out.contains("tk_abc123xyz"), "token leaked: {out}");
        assert!(out.contains("Bearer"), "Bearer keyword gone: {out}");
    }

    #[test]
    fn hc_ping_key_redacted() {
        let s = "GET https://hc-ping.com/SECRETKEY/mini-heartbeat";
        let out = redact_str(s);
        assert!(!out.contains("SECRETKEY"), "key leaked: {out}");
    }

    #[test]
    fn ntfy_token_in_url_redacted() {
        let s = "https://mytoken@192.168.1.1:8082/fleet";
        let out = redact_str(s);
        assert!(!out.contains("mytoken"), "token leaked: {out}");
    }

    #[test]
    fn non_secret_unchanged() {
        let s = "connection refused to 100.64.0.1:8082";
        assert_eq!(redact_str(s), s);
    }

    #[test]
    fn test_bare_hc_ping_and_lowercase_bearer_redacted() {
        // Bare hc-ping URL (no trailing slug) and lowercase bearer token
        let s = "ping failed: https://hc-ping.com/SUPERSECRETKEY; auth: bearer tk_abc123";
        let out = redact_str(s);
        assert!(
            !out.contains("SUPERSECRETKEY"),
            "bare hc-ping key leaked: {out}"
        );
        assert!(
            !out.contains("tk_abc123"),
            "lowercase bearer token leaked: {out}"
        );
    }
}

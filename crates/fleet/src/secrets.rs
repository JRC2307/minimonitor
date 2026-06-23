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

/// Matches credential-bearing URLs of the form `scheme://user:pass@host`.
///
/// NOTE: This is intentionally restricted to patterns that include a colon
/// (i.e. `user:pass@`) to avoid matching bare-token URLs like
/// `https://mytoken@host`, which are covered by `ntfy_token_re`.
/// Order matters: this runs BEFORE `ntfy_token_re` so the `:pass@` portion
/// is already redacted before the ntfy pattern sees the string.
fn userpass_url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Matches `scheme://anything:anything@` — strips user:pass, keeps scheme.
        // Captures: (1) scheme://, (2) user, (3) :pass@.
        // Replacement: `$1[REDACTED]@`
        Regex::new(r"([a-zA-Z][a-zA-Z0-9+\-.]*://)([^:@/\s]+:[^@/\s]+@)").unwrap()
    })
}

/// Matches credential-bearing ntfy token URLs:
/// `https://<token>@<host>/` or `Authorization: Bearer <token>`
/// (Bearer is already handled above; this covers any other tokenized URL segments.)
fn ntfy_token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(https?://)([^@\s]+@)").unwrap())
}

/// Matches key=value secret patterns in argv and query strings (case-insensitive).
///
/// Covered keys: `password`, `token`, `secret`, `api_key`, `apikey`.
///
/// DELIBERATELY EXCLUDED: bare `key=` — far too broad; would clobber innocent
/// flags like `--sort-key=name`, `--ssh-key=id_rsa`, `cache-key=abc`.
/// Only the specific names above are sensitive enough to warrant redaction.
///
/// The `(?:--|[?&])?` prefix matches optional `--` (CLI long-opts) or `?`/`&`
/// (URL query) before the key name, preserving it while replacing only the value.
fn kv_secret_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)((?:--|[?&])?(?:password|token|secret|api_key|apikey))=(\S+)").unwrap()
    })
}

/// Apply all redaction patterns to a string.
///
/// Patterns applied (in order):
/// 1. `Bearer <token>` → `Bearer [REDACTED]`
/// 2. `hc-ping.com/<KEY>/` → `hc-ping.com/[REDACTED]/`
/// 3. `scheme://user:pass@host` → `scheme://[REDACTED]@host`  (must run before ntfy)
/// 4. `https://token@host` → `https://[REDACTED]@host`  (ntfy bare-token URLs)
/// 5. `password=X`, `token=X`, `secret=X`, `api_key=X`, `apikey=X` (case-insensitive) → `key=[REDACTED]`
fn redact_str(s: &str) -> String {
    let s = bearer_re().replace_all(s, "Bearer [REDACTED]");
    let s = hc_ping_re().replace_all(&s, "$1[REDACTED]$3");
    // user:pass@host must run before the bare ntfy_token pattern
    let s = userpass_url_re().replace_all(&s, "$1[REDACTED]@");
    let s = ntfy_token_re().replace_all(&s, "$1[REDACTED]@");
    // key=value secrets (argv flags and query-string params)
    let s = kv_secret_re().replace_all(&s, "$1=[REDACTED]");
    s.into_owned()
}

/// Scrub a command-line string of any secret-shaped substrings.
///
/// Applies the same patterns as `redact_str` to a full argv string.
/// Safe, non-secret commands (e.g. `/usr/bin/ollama serve --model llama3`)
/// are returned unchanged.
///
/// Call this on every `ProcessRow.command` and `AiWorkload.example_command`
/// before persisting to SQLite (§4.5).
pub fn scrub_command(s: &str) -> String {
    redact_str(s)
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

    // ── New patterns: C2 ─────────────────────────────────────────────────────

    #[test]
    fn password_flag_redacted() {
        let s = "--password=hunter2";
        let out = scrub_command(s);
        assert!(!out.contains("hunter2"), "password leaked: {out}");
        assert!(out.contains("--password="), "key name gone: {out}");
    }

    #[test]
    fn password_flag_uppercase_redacted() {
        // Case-insensitive match
        let s = "--PASSWORD=hunter2";
        let out = scrub_command(s);
        assert!(
            !out.contains("hunter2"),
            "password (uppercase) leaked: {out}"
        );
    }

    #[test]
    fn token_kv_redacted() {
        let s = "token=ghp_abc123";
        let out = scrub_command(s);
        assert!(!out.contains("ghp_abc123"), "token leaked: {out}");
        assert!(out.contains("token="), "key name gone: {out}");
    }

    #[test]
    fn secret_kv_redacted() {
        let s = "secret=supersecretvalue";
        let out = scrub_command(s);
        assert!(!out.contains("supersecretvalue"), "secret leaked: {out}");
    }

    #[test]
    fn api_key_underscore_redacted() {
        let s = "?api_key=AKIA123";
        let out = scrub_command(s);
        assert!(!out.contains("AKIA123"), "api_key leaked: {out}");
        assert!(out.contains("api_key="), "key name gone: {out}");
    }

    #[test]
    fn apikey_no_underscore_redacted() {
        let s = "apikey=AKIA456";
        let out = scrub_command(s);
        assert!(!out.contains("AKIA456"), "apikey leaked: {out}");
    }

    #[test]
    fn userpass_url_redacted() {
        let s = "https://user:pass@host.example.com/path";
        let out = scrub_command(s);
        assert!(!out.contains("user:pass"), "user:pass leaked: {out}");
        assert!(out.contains("https://"), "scheme gone: {out}");
        assert!(out.contains("host.example.com"), "host gone: {out}");
    }

    #[test]
    fn userpass_url_various_schemes_redacted() {
        let s = "postgres://admin:s3cr3t@db.internal:5432/mydb";
        let out = scrub_command(s);
        assert!(!out.contains("s3cr3t"), "DB password leaked: {out}");
        assert!(out.contains("postgres://"), "scheme gone: {out}");
    }

    // ── Non-secret argv must pass unchanged ──────────────────────────────────

    #[test]
    fn ollama_serve_unchanged() {
        let s = "/usr/bin/ollama serve --model llama3";
        assert_eq!(scrub_command(s), s, "benign command was altered");
    }

    #[test]
    fn sort_key_flag_unchanged() {
        // `key=` alone must NOT be redacted — too broad
        let s = "/usr/bin/ls --sort-key=name";
        assert_eq!(
            scrub_command(s),
            s,
            "--sort-key=name was altered (false positive)"
        );
    }

    #[test]
    fn ssh_key_flag_unchanged() {
        let s = "ssh-keygen --ssh-key=id_rsa";
        assert_eq!(
            scrub_command(s),
            s,
            "--ssh-key=id_rsa was altered (false positive)"
        );
    }

    #[test]
    fn cache_key_unchanged() {
        let s = "myapp --cache-key=abc123";
        assert_eq!(
            scrub_command(s),
            s,
            "--cache-key was altered (false positive)"
        );
    }
}

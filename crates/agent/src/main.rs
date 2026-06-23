mod push;

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use minimonitor_core::snapshot::{Sampler, SortMode};

// ─── Routing ─────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum Route {
    Healthz,
    Snapshot,
    NotFound,
}

/// Normalize path: strip query at first `?`, strip one trailing `/`, then match.
fn route(method: &str, path: &str) -> Route {
    let path = path.split('?').next().unwrap_or(path);
    let path = path.strip_suffix('/').unwrap_or(path);
    match (method, path) {
        ("GET", "/healthz") => Route::Healthz,
        ("GET", "/snapshot") => Route::Snapshot,
        _ => Route::NotFound,
    }
}

// ─── Constant-time auth ───────────────────────────────────────────────────────

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Pure auth decision. `token == None` ⇒ always authorized.
/// Wrong/missing/non-Bearer ⇒ false.
fn authorized(headers: &[tiny_http::Header], token: Option<&str>) -> bool {
    let Some(tok) = token else {
        return true;
    };
    let expected = format!("Bearer {tok}");
    headers.iter().any(|h| {
        h.field.equiv("Authorization") && ct_eq(h.value.as_str().as_bytes(), expected.as_bytes())
    })
}

// ─── Bind resolution ──────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum BindDecision {
    /// Proceed and bind to this address.
    Bind(String),
    /// Fail-closed loopback fallback (with warning).
    LoopbackFallback(String),
    /// Refuse: non-tailnet bind without --allow-non-tailnet.
    RefuseNonTailnet(String),
    /// Refuse: tailnet bind without token and without --allow-untokened-tailnet.
    RefuseUntokenedTailnet(String),
}

struct BindArgs<'a> {
    /// Value of --bind flag, if provided.
    bind_flag: Option<String>,
    /// Whether --allow-non-tailnet was passed.
    allow_non_tailnet: bool,
    /// Whether --allow-untokened-tailnet was passed.
    allow_untokened_tailnet: bool,
    /// The resolved token (None means no token configured).
    token: Option<&'a str>,
}

fn is_loopback_bind(addr: &str) -> bool {
    // Extract host part from host:port
    if let Some((host_raw, _)) = addr.rsplit_once(':') {
        let host = host_raw.trim_start_matches('[').trim_end_matches(']');
        host == "127.0.0.1" || host.starts_with("127.") || host == "::1" || host == "[::1]"
    } else {
        false
    }
}

fn resolve_bind_with(
    args: &BindArgs<'_>,
    env_lookup: impl Fn(&str) -> Option<String>,
    tailnet_ip: Option<String>,
) -> BindDecision {
    // Precedence 1: --bind flag
    let candidate = if let Some(ref flag) = args.bind_flag {
        flag.clone()
    } else if let Some(env_val) = env_lookup("MINIMONITOR_AGENT_BIND").filter(|v| !v.is_empty()) {
        // Precedence 2: MINIMONITOR_AGENT_BIND env (non-empty)
        env_val
    } else if let Some(ts_ip) = tailnet_ip {
        // Precedence 3: auto-detected tailnet IP
        format!("{ts_ip}:9909")
    } else {
        // Precedence 4: fail-closed loopback fallback
        return BindDecision::LoopbackFallback("127.0.0.1:9909".to_owned());
    };

    // Loopback is always allowed unconditionally.
    if is_loopback_bind(&candidate) {
        return BindDecision::Bind(candidate);
    }

    // Run the tailnet validator (self-guard).
    if let Err(_err) = minimonitor_core::net::validate_tailnet_bind(&candidate) {
        // Non-tailnet bind: gate on --allow-non-tailnet.
        if args.allow_non_tailnet {
            return BindDecision::Bind(candidate);
        } else {
            return BindDecision::RefuseNonTailnet(candidate);
        }
    }

    // Tailnet bind: check token requirement.
    if args.token.is_none() && !args.allow_untokened_tailnet {
        return BindDecision::RefuseUntokenedTailnet(candidate);
    }

    BindDecision::Bind(candidate)
}

// ─── CLI argument parsing ─────────────────────────────────────────────────────

struct ParsedArgs {
    once: bool,
    bind_flag: Option<String>,
    allow_non_tailnet: bool,
    allow_untokened_tailnet: bool,
}

fn parse_args(args: impl Iterator<Item = String>) -> ParsedArgs {
    let args: Vec<String> = args.collect();
    let once = args.iter().any(|a| a == "--once");
    let allow_non_tailnet = args.iter().any(|a| a == "--allow-non-tailnet");
    let allow_untokened_tailnet = args.iter().any(|a| a == "--allow-untokened-tailnet");

    let bind_flag = args
        .windows(2)
        .find(|w| w[0] == "--bind")
        .map(|w| w[1].clone());

    ParsedArgs {
        once,
        bind_flag,
        allow_non_tailnet,
        allow_untokened_tailnet,
    }
}

// ─── main ─────────────────────────────────────────────────────────────────────

fn main() {
    let parsed = parse_args(std::env::args());

    if parsed.once {
        let mut sampler = Sampler::new();
        let snap = sampler.sample(SortMode::Cpu);
        println!("{}", serde_json::to_string_pretty(&snap).unwrap());
        return;
    }

    // Resolve token (empty/unset ⇒ None). Token is never logged.
    let token_string = std::env::var("MINIMONITOR_AGENT_TOKEN")
        .ok()
        .filter(|v| !v.is_empty());
    let token: Option<&str> = token_string.as_deref();

    // Resolve bind address.
    let hostname = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "localhost".to_owned());
    let identity = minimonitor_core::net::network_identity(hostname);
    let tailnet_ip = identity.tailnet_ip;

    let bind_args = BindArgs {
        bind_flag: parsed.bind_flag,
        allow_non_tailnet: parsed.allow_non_tailnet,
        allow_untokened_tailnet: parsed.allow_untokened_tailnet,
        token,
    };

    let addr = match resolve_bind_with(&bind_args, |k| std::env::var(k).ok(), tailnet_ip) {
        BindDecision::Bind(a) => a,
        BindDecision::LoopbackFallback(a) => {
            eprintln!(
                "minimonitor-agent: no tailnet IP detected — falling back to loopback {a} (agent unreachable from fleet)"
            );
            a
        }
        BindDecision::RefuseNonTailnet(a) => {
            eprintln!(
                "minimonitor-agent: refusing to bind non-tailnet address `{a}` — pass --allow-non-tailnet to override (dev only)"
            );
            std::process::exit(2);
        }
        BindDecision::RefuseUntokenedTailnet(a) => {
            eprintln!(
                "minimonitor-agent: refusing to bind tailnet address `{a}` without a token — set MINIMONITOR_AGENT_TOKEN or pass --allow-untokened-tailnet"
            );
            std::process::exit(2);
        }
    };

    let mut sampler = Sampler::new();
    let first = serde_json::to_string(&sampler.sample(SortMode::Cpu)).unwrap();
    let latest = Arc::new(Mutex::new(first));

    {
        let latest = latest.clone();
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(1));
                let snap = sampler.sample(SortMode::Cpu);
                if let Ok(json) = serde_json::to_string(&snap) {
                    *latest.lock().unwrap() = json;
                }
            }
        });
    }

    let server = tiny_http::Server::http(&addr)
        .unwrap_or_else(|e| panic!("agent failed to bind {addr}: {e}"));
    // Token is never logged — echo only the address.
    eprintln!("minimonitor-agent serving http://{addr}/snapshot");

    for request in server.incoming_requests() {
        let method = request.method().as_str();
        let url = request.url().to_owned();

        match route(method, &url) {
            Route::Healthz => {
                let header =
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/plain"[..])
                        .unwrap();
                let _ = request.respond(tiny_http::Response::from_string("ok").with_header(header));
            }
            Route::Snapshot => {
                if !authorized(request.headers(), token) {
                    let header =
                        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/plain"[..])
                            .unwrap();
                    let _ = request.respond(
                        tiny_http::Response::from_string("unauthorized")
                            .with_status_code(401)
                            .with_header(header),
                    );
                } else {
                    let body = latest.lock().unwrap().clone();
                    let header = tiny_http::Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"application/json"[..],
                    )
                    .unwrap();
                    let _ =
                        request.respond(tiny_http::Response::from_string(body).with_header(header));
                }
            }
            Route::NotFound => {
                let header =
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/plain"[..])
                        .unwrap();
                let _ = request.respond(
                    tiny_http::Response::from_string("not found")
                        .with_status_code(404)
                        .with_header(header),
                );
            }
        }
    }
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ct_eq ─────────────────────────────────────────────────────────────────

    #[test]
    fn ct_eq_equal_slices() {
        assert!(ct_eq(b"hello", b"hello"));
    }

    #[test]
    fn ct_eq_same_length_different() {
        assert!(!ct_eq(b"hello", b"world"));
    }

    #[test]
    fn ct_eq_different_length() {
        assert!(!ct_eq(b"hi", b"hello"));
    }

    #[test]
    fn ct_eq_both_empty() {
        assert!(ct_eq(b"", b""));
    }

    // ── authorized ────────────────────────────────────────────────────────────

    fn make_header(name: &str, value: &str) -> tiny_http::Header {
        tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap()
    }

    #[test]
    fn authorized_none_token_always_true() {
        // No headers, no token ⇒ true.
        assert!(authorized(&[], None));
        // Even with a random Authorization header, None token ⇒ true.
        let h = make_header("Authorization", "Bearer anything");
        assert!(authorized(&[h], None));
    }

    #[test]
    fn authorized_correct_bearer() {
        let h = make_header("Authorization", "Bearer secret123");
        assert!(authorized(&[h], Some("secret123")));
    }

    #[test]
    fn authorized_wrong_same_length_token() {
        // "secret123" and "secret456" are same length → constant-time compare must still return false.
        let h = make_header("Authorization", "Bearer secret456");
        assert!(!authorized(&[h], Some("secret123")));
    }

    #[test]
    fn authorized_wrong_length_token() {
        let h = make_header("Authorization", "Bearer short");
        assert!(!authorized(&[h], Some("secret123")));
    }

    #[test]
    fn authorized_missing_authorization_header() {
        let h = make_header("Content-Type", "application/json");
        assert!(!authorized(&[h], Some("secret123")));
    }

    #[test]
    fn authorized_non_bearer_scheme() {
        let h = make_header("Authorization", "Basic dXNlcjpwYXNz");
        assert!(!authorized(&[h], Some("secret123")));
    }

    // ── route ─────────────────────────────────────────────────────────────────

    #[test]
    fn route_healthz() {
        assert_eq!(route("GET", "/healthz"), Route::Healthz);
    }

    #[test]
    fn route_snapshot() {
        assert_eq!(route("GET", "/snapshot"), Route::Snapshot);
    }

    #[test]
    fn route_snapshot_with_query() {
        assert_eq!(route("GET", "/snapshot?x=1"), Route::Snapshot);
    }

    #[test]
    fn route_snapshot_trailing_slash() {
        assert_eq!(route("GET", "/snapshot/"), Route::Snapshot);
    }

    #[test]
    fn route_post_snapshot_is_404() {
        assert_eq!(route("POST", "/snapshot"), Route::NotFound);
    }

    #[test]
    fn route_anything_else_is_404() {
        assert_eq!(route("GET", "/anything-else"), Route::NotFound);
        assert_eq!(route("GET", "/"), Route::NotFound);
        assert_eq!(route("DELETE", "/healthz"), Route::NotFound);
    }

    // ── resolve_bind_with ─────────────────────────────────────────────────────

    fn no_env(_k: &str) -> Option<String> {
        None
    }

    fn env_with_bind(val: &'static str) -> impl Fn(&str) -> Option<String> {
        move |k| {
            if k == "MINIMONITOR_AGENT_BIND" {
                Some(val.to_owned())
            } else {
                None
            }
        }
    }

    fn base_args<'a>(token: Option<&'a str>) -> BindArgs<'a> {
        BindArgs {
            bind_flag: None,
            allow_non_tailnet: false,
            allow_untokened_tailnet: false,
            token,
        }
    }

    #[test]
    fn bind_flag_wins_over_env_and_auto() {
        let args = BindArgs {
            bind_flag: Some("100.96.1.2:9909".to_owned()),
            allow_non_tailnet: false,
            allow_untokened_tailnet: false,
            token: Some("tok"),
        };
        let decision = resolve_bind_with(
            &args,
            env_with_bind("100.96.9.9:9909"),
            Some("100.96.5.5".to_owned()),
        );
        assert_eq!(decision, BindDecision::Bind("100.96.1.2:9909".to_owned()));
    }

    #[test]
    fn env_wins_over_auto_when_no_flag() {
        let args = BindArgs {
            bind_flag: None,
            allow_non_tailnet: false,
            allow_untokened_tailnet: false,
            token: Some("tok"),
        };
        let decision = resolve_bind_with(
            &args,
            env_with_bind("100.96.1.2:9909"),
            Some("100.96.5.5".to_owned()),
        );
        assert_eq!(decision, BindDecision::Bind("100.96.1.2:9909".to_owned()));
    }

    #[test]
    fn empty_env_falls_through_to_auto() {
        let args = BindArgs {
            bind_flag: None,
            allow_non_tailnet: false,
            allow_untokened_tailnet: false,
            token: Some("tok"),
        };
        // Empty env value is ignored.
        let env = |k: &str| {
            if k == "MINIMONITOR_AGENT_BIND" {
                Some("".to_owned())
            } else {
                None
            }
        };
        let decision = resolve_bind_with(&args, env, Some("100.96.1.2".to_owned()));
        assert_eq!(decision, BindDecision::Bind("100.96.1.2:9909".to_owned()));
    }

    #[test]
    fn auto_tailnet_ip_used() {
        let args = BindArgs {
            bind_flag: None,
            allow_non_tailnet: false,
            allow_untokened_tailnet: false,
            token: Some("tok"),
        };
        let decision = resolve_bind_with(&args, no_env, Some("100.96.1.2".to_owned()));
        assert_eq!(decision, BindDecision::Bind("100.96.1.2:9909".to_owned()));
    }

    #[test]
    fn auto_no_tailnet_ip_loopback_fallback() {
        let args = base_args(Some("tok"));
        let decision = resolve_bind_with(&args, no_env, None);
        assert_eq!(
            decision,
            BindDecision::LoopbackFallback("127.0.0.1:9909".to_owned())
        );
    }

    #[test]
    fn self_guard_wildcard_without_flag_refuses() {
        let args = BindArgs {
            bind_flag: Some("0.0.0.0:9909".to_owned()),
            allow_non_tailnet: false,
            allow_untokened_tailnet: false,
            token: Some("tok"),
        };
        let decision = resolve_bind_with(&args, no_env, None);
        assert_eq!(
            decision,
            BindDecision::RefuseNonTailnet("0.0.0.0:9909".to_owned())
        );
    }

    #[test]
    fn self_guard_wildcard_with_flag_allowed() {
        let args = BindArgs {
            bind_flag: Some("0.0.0.0:9909".to_owned()),
            allow_non_tailnet: true,
            allow_untokened_tailnet: false,
            token: Some("tok"),
        };
        let decision = resolve_bind_with(&args, no_env, None);
        assert_eq!(decision, BindDecision::Bind("0.0.0.0:9909".to_owned()));
    }

    #[test]
    fn self_guard_ipv6_wildcard_without_flag_refuses() {
        let args = BindArgs {
            bind_flag: Some("[::]:9909".to_owned()),
            allow_non_tailnet: false,
            allow_untokened_tailnet: false,
            token: Some("tok"),
        };
        let decision = resolve_bind_with(&args, no_env, None);
        assert_eq!(
            decision,
            BindDecision::RefuseNonTailnet("[::]:9909".to_owned())
        );
    }

    #[test]
    fn loopback_always_allowed_unconditionally() {
        // Even without token and without any flags, loopback is allowed.
        let args = base_args(None);
        let args2 = BindArgs {
            bind_flag: Some("127.0.0.1:9909".to_owned()),
            ..args
        };
        let decision = resolve_bind_with(&args2, no_env, None);
        assert_eq!(decision, BindDecision::Bind("127.0.0.1:9909".to_owned()));
    }

    #[test]
    fn untokened_tailnet_without_flag_refuses() {
        let args = BindArgs {
            bind_flag: Some("100.96.1.2:9909".to_owned()),
            allow_non_tailnet: false,
            allow_untokened_tailnet: false,
            token: None, // no token
        };
        let decision = resolve_bind_with(&args, no_env, None);
        assert_eq!(
            decision,
            BindDecision::RefuseUntokenedTailnet("100.96.1.2:9909".to_owned())
        );
    }

    #[test]
    fn untokened_tailnet_with_flag_allowed() {
        let args = BindArgs {
            bind_flag: Some("100.96.1.2:9909".to_owned()),
            allow_non_tailnet: false,
            allow_untokened_tailnet: true,
            token: None, // no token, but flag set
        };
        let decision = resolve_bind_with(&args, no_env, None);
        assert_eq!(decision, BindDecision::Bind("100.96.1.2:9909".to_owned()));
    }

    #[test]
    fn loopback_untokened_is_allowed() {
        // Loopback + no token ⇒ allowed (trivially safe).
        let args = BindArgs {
            bind_flag: Some("127.0.0.1:9909".to_owned()),
            allow_non_tailnet: false,
            allow_untokened_tailnet: false,
            token: None,
        };
        let decision = resolve_bind_with(&args, no_env, None);
        assert_eq!(decision, BindDecision::Bind("127.0.0.1:9909".to_owned()));
    }
}

use clap::{Parser, Subcommand};

/// Fleet — multi-tailnet inventory registry + observability CLI.
#[derive(Parser)]
#[command(name = "fleet", version)]
struct Cli {
    /// Path to fleet.toml (default: ~/.config/fleet/fleet.toml)
    #[arg(long, global = true)]
    config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run preflight checks: compose bind-address + secret resolvability.
    Doctor {
        /// Path to the docker-compose file to check.
        #[arg(long, default_value = "deploy/docker-compose.yml")]
        compose: std::path::PathBuf,
    },
    /// Pull every configured tailnet, merge/dedupe, persist, export fleet.yaml.
    Sync {
        /// Path to fleet-overrides.yaml (default: alongside the config).
        #[arg(long)]
        overrides: Option<std::path::PathBuf>,
    },
    /// List nodes with optional filters.
    List {
        /// Filter by facet:value (e.g. role:host, owner:self, site:local, gpu:none).
        #[arg(long)]
        tag: Option<String>,
        /// Filter by tier: agent | agentless.
        #[arg(long)]
        tier: Option<String>,
        /// Show only nodes that are online right now (recomputes freshness).
        #[arg(long)]
        online: bool,
        /// Emit JSON (Vec<Node>) instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Show full detail for a single node (fleet_id | hostname | fqdn).
    Show {
        /// Node reference: fleet_id, hostname, or fqdn.
        node: String,
    },
    /// Pull Cloudflare zones + cert-packs, upsert to cf_zone, ntfy on breach.
    CfSync,
    /// Run the MTR path prober against configured targets; ntfy on breach.
    Probe,
    /// Pull host snapshots from all tier:agent nodes (resilient, retention-first).
    Collect,
    /// Ping the hc-ping.com dead-man's-switch endpoint (external liveness check).
    ///
    /// URL: {base}/{ping_key}/{slug}?create=1  (auto-provisions the check).
    /// Run every minute from a LaunchAgent / cron slot.
    Heartbeat,
    /// Idempotent reconcile of agent-tier nodes into Beszel (PocketBase REST).
    Enroll {
        /// Print the plan without making any API calls or DB writes.
        #[arg(long)]
        dry_run: bool,
    },
    /// Open an SSH session to a node via its validated Tailscale IP.
    Ssh {
        /// Node reference: fleet_id, hostname, or fqdn.
        target: String,
        /// SSH user (default: from config → "root").
        #[arg(long, short = 'u')]
        user: Option<String>,
        /// Use `tailscale ssh` instead of plain `ssh`.
        #[arg(long)]
        ts: bool,
        /// Remote command to run (passed after --).
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Start the read-only JSON API server (spec §3.8).
    ///
    /// Binds to the address in `[serve] bind` from fleet.toml.
    Serve,
}

const TS_API_BASE: &str = "https://api.tailscale.com";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        None => {
            // No subcommand — print help.
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
        }
        Some(Commands::Doctor { compose }) => {
            let config_path = cli.config.clone().unwrap_or_else(default_config_path);
            run_doctor(&compose, &config_path)?;
        }
        Some(Commands::CfSync) => {
            let config_path = cli.config.clone().unwrap_or_else(default_config_path);
            let cfg = fleet::config::load_config(&config_path)?;
            let db_path = std::path::PathBuf::from(&cfg.db_path);
            let cf_cfg = cfg.cloudflare.as_ref().ok_or_else(|| {
                anyhow::anyhow!("fleet cf-sync: [cloudflare] section missing from config")
            })?;
            fleet::commands::cf_sync::run(cf_cfg, cfg.ntfy.as_ref(), &db_path).await?;
            eprintln!("fleet cf-sync: done");
        }
        Some(Commands::Heartbeat) => {
            let config_path = cli.config.clone().unwrap_or_else(default_config_path);
            let cfg = fleet::config::load_config(&config_path)?;
            let hc_cfg = cfg.healthchecks.as_ref().ok_or_else(|| {
                anyhow::anyhow!("fleet heartbeat: [healthchecks] section missing from config")
            })?;
            fleet::commands::heartbeat::run(hc_cfg).await?;
            eprintln!("fleet heartbeat: ok");
        }
        Some(Commands::Probe) => {
            let config_path = cli.config.clone().unwrap_or_else(default_config_path);
            let cfg = fleet::config::load_config(&config_path)?;
            let db_path = std::path::PathBuf::from(&cfg.db_path);
            fleet::commands::probe::run(&cfg, &db_path).await?;
            eprintln!("fleet probe: done");
        }
        Some(Commands::Collect) => {
            let config_path = cli.config.clone().unwrap_or_else(default_config_path);
            let cfg = fleet::config::load_config(&config_path)?;
            let db_path = cfg.db_path.clone();
            fleet::commands::collect::run(&cfg, &db_path).await?;
            eprintln!("fleet collect: done");
        }
        Some(Commands::Sync { overrides }) => {
            let config_path = cli.config.clone().unwrap_or_else(default_config_path);
            let cfg = fleet::config::load_config(&config_path)?;
            let overrides_path = overrides.unwrap_or_else(|| {
                config_path
                    .parent()
                    .map(|p| p.join("fleet-overrides.yaml"))
                    .unwrap_or_else(|| std::path::PathBuf::from("fleet-overrides.yaml"))
            });
            let db_path = std::path::PathBuf::from(&cfg.db_path);
            fleet::commands::sync::run(&cfg, &overrides_path, &db_path, TS_API_BASE).await?;
            eprintln!("fleet sync: done");
        }
        Some(Commands::List {
            tag,
            tier,
            online,
            json,
        }) => {
            let config_path = cli.config.clone().unwrap_or_else(default_config_path);
            let cfg = fleet::config::load_config(&config_path)?;
            let db_path = std::path::PathBuf::from(&cfg.db_path);
            let conn = fleet::db::open(&db_path)?;
            let threshold = std::time::Duration::from_secs(cfg.online_threshold_secs);
            fleet::commands::list::run(
                &conn,
                tag.as_deref(),
                tier.as_deref(),
                online,
                json,
                threshold,
            )?;
        }
        Some(Commands::Show { node }) => {
            let config_path = cli.config.clone().unwrap_or_else(default_config_path);
            let cfg = fleet::config::load_config(&config_path)?;
            let db_path = std::path::PathBuf::from(&cfg.db_path);
            let conn = fleet::db::open(&db_path)?;
            let threshold = std::time::Duration::from_secs(cfg.online_threshold_secs);
            let result = fleet::commands::show::run(&conn, &node, threshold)?;
            if matches!(result, fleet::commands::show::ShowResult::Ambiguous) {
                std::process::exit(1);
            }
        }
        Some(Commands::Enroll { dry_run }) => {
            let config_path = cli.config.clone().unwrap_or_else(default_config_path);
            let cfg = fleet::config::load_config(&config_path)?;
            let db_path = std::path::PathBuf::from(&cfg.db_path);
            let beszel_url = cfg
                .beszel
                .as_ref()
                .map(|b| b.url.clone())
                .unwrap_or_default();
            fleet::commands::enroll::run_all(&cfg, &db_path, &beszel_url, dry_run).await?;
        }
        Some(Commands::Ssh {
            target,
            user,
            ts,
            cmd,
        }) => {
            let config_path = cli.config.clone().unwrap_or_else(default_config_path);
            let cfg = fleet::config::load_config(&config_path)?;
            let db_path = std::path::PathBuf::from(&cfg.db_path);
            let conn = fleet::db::open(&db_path)?;
            let user = user.unwrap_or(cfg.ssh_user);
            fleet::commands::ssh::run(&conn, &target, &user, ts, &cmd)?;
        }
        Some(Commands::Serve) => {
            let config_path = cli.config.clone().unwrap_or_else(default_config_path);
            let cfg = fleet::config::load_config(&config_path)?;
            let db_path = std::path::PathBuf::from(&cfg.db_path);
            fleet::commands::serve::run(&cfg, &db_path).await?;
        }
    }

    Ok(())
}

fn default_config_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::PathBuf::from(home).join(".config/fleet/fleet.toml")
}

fn run_doctor(compose_path: &std::path::Path, config_path: &std::path::Path) -> anyhow::Result<()> {
    eprintln!(
        "fleet doctor: checking bind addresses in {}",
        compose_path.display()
    );

    let mut errors: Vec<String> = Vec::new();

    // ── 1. Compose bind-address check ────────────────────────────────────────
    if compose_path.exists() {
        let yaml = std::fs::read_to_string(compose_path)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", compose_path.display()))?;
        match fleet::doctor::check_compose_binds(&yaml) {
            Ok(()) => eprintln!("fleet doctor: compose bind-address check PASSED"),
            Err(e) => {
                eprintln!("fleet doctor: ERROR compose bind: {e}");
                errors.push(e.to_string());
            }
        }
    } else {
        eprintln!(
            "fleet doctor: {} not found, skipping compose bind check",
            compose_path.display()
        );
    }

    // Load config (best-effort; some checks below require it).
    let cfg_opt = if config_path.exists() {
        match fleet::config::load_config(config_path) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("fleet doctor: WARN could not load config: {e}");
                None
            }
        }
    } else {
        None
    };

    // ── 2. `fleet serve` bind check (R-5, spec §3.8) ─────────────────────────
    if let Some(cfg) = cfg_opt.as_ref()
        && let Some(serve) = cfg.serve.as_ref()
    {
        match fleet::doctor::check_serve_bind(&serve.bind) {
            Ok(()) => eprintln!("fleet doctor: serve bind-address check PASSED"),
            Err(e) => {
                eprintln!("fleet doctor: ERROR serve.bind: {e}");
                errors.push(e.to_string());
            }
        }
    }

    // ── 3. Agent live-bind check (spec §3.4) ─────────────────────────────────
    // Scan the local host's listening ports for :9909 bound to a non-safe address.
    match fleet::doctor::check_agent_live_bind() {
        Ok(()) => eprintln!("fleet doctor: agent live bind check PASSED"),
        Err(e) => {
            eprintln!("fleet doctor: ERROR agent live bind: {e}");
            errors.push(e.to_string());
        }
    }

    // ── 4. Token resolvability + untokened-tailnet check (spec §3.4) ─────────
    if let Some(cfg) = cfg_opt.as_ref() {
        let token_env = cfg.collect.token_env.as_deref();
        if let Some(env_var) = token_env {
            // WARN if the configured token env var cannot be resolved.
            let unresolved = fleet::doctor::check_secret_resolvability(
                &[(env_var, env_var)],
                fleet::secrets::keychain_absent_fn,
            );
            if !unresolved.is_empty() {
                eprintln!(
                    "fleet doctor: WARN token env var `{env_var}` is not set or unresolvable \
                     (set it before running `fleet collect`)"
                );
            } else {
                eprintln!("fleet doctor: token env var `{env_var}` resolves OK");
            }
        } else {
            // No token configured — check whether a tailnet-bound agent is running.
            // A locally-detected tailnet-bound agent with no token is an ERROR (spec §3.3 / §3.4).
            use minimonitor_core::net::{is_cgnat, listening_ports};
            let has_tailnet_agent = listening_ports()
                .into_iter()
                .filter(|r| r.port == 9909)
                .any(|r| {
                    // Tailnet bind = CGNAT address (100.64.0.0/10, RFC 6598).
                    let addr = r.bind.trim_start_matches('[').trim_end_matches(']');
                    addr.parse::<std::net::Ipv4Addr>()
                        .map(is_cgnat)
                        .unwrap_or(false)
                });
            if has_tailnet_agent {
                let msg = "tailnet-bound agent detected on :9909 with no [collect].token_env \
                           configured — untokened tailnet agent is a security ERROR";
                eprintln!("fleet doctor: ERROR {msg}");
                errors.push(msg.to_owned());
            }
        }
    }

    // ── 5. DB: list nodes with open last_error (read-only; skip if no DB) ────
    if let Some(cfg) = cfg_opt.as_ref() {
        let db_path = std::path::PathBuf::from(&cfg.db_path);
        if db_path.exists() {
            match fleet::db::open(&db_path) {
                Err(e) => {
                    eprintln!("fleet doctor: WARN cannot open DB for status check: {e}");
                }
                Ok(conn) => {
                    let mut stmt = conn
                        .prepare(
                            "SELECT node_id, last_error FROM host_collect_status \
                             WHERE last_error IS NOT NULL",
                        )
                        .unwrap_or_else(|_| unreachable!("static SQL is valid"));
                    let rows: Vec<(String, String)> = stmt
                        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                        .map(|iter| iter.filter_map(|r| r.ok()).collect())
                        .unwrap_or_default();
                    if rows.is_empty() {
                        eprintln!("fleet doctor: no nodes with collect errors in DB");
                    } else {
                        for (node_id, err) in &rows {
                            eprintln!(
                                "fleet doctor: WARN node `{node_id}` has collect error: {err}"
                            );
                        }
                    }
                }
            }
        } else {
            eprintln!(
                "fleet doctor: no DB found at {}, skipping node error check",
                db_path.display()
            );
        }
    }

    if errors.is_empty() {
        eprintln!("fleet doctor: all checks passed");
        Ok(())
    } else {
        anyhow::bail!(
            "fleet doctor: {} error(s) found:\n{}",
            errors.len(),
            errors.join("\n")
        )
    }
}

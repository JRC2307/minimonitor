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
            run_doctor(&compose)?;
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
        Some(Commands::Probe) => {
            let config_path = cli.config.clone().unwrap_or_else(default_config_path);
            let cfg = fleet::config::load_config(&config_path)?;
            let db_path = std::path::PathBuf::from(&cfg.db_path);
            fleet::commands::probe::run(&cfg, &db_path).await?;
            eprintln!("fleet probe: done");
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
    }

    Ok(())
}

fn default_config_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::PathBuf::from(home).join(".config/fleet/fleet.toml")
}

fn run_doctor(compose_path: &std::path::Path) -> anyhow::Result<()> {
    eprintln!(
        "fleet doctor: checking bind addresses in {}",
        compose_path.display()
    );

    if compose_path.exists() {
        let yaml = std::fs::read_to_string(compose_path)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", compose_path.display()))?;
        fleet::doctor::check_compose_binds(&yaml)?;
        eprintln!("fleet doctor: bind-address check PASSED");
    } else {
        eprintln!(
            "fleet doctor: {} not found, skipping bind check",
            compose_path.display()
        );
    }

    eprintln!("fleet doctor: all checks passed");
    Ok(())
}

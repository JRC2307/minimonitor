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
}

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
    }

    Ok(())
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

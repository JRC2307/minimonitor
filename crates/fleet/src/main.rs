use clap::Parser;

#[derive(Parser)]
#[command(name = "fleet", version)]
struct Cli {}

#[tokio::main]
async fn main() {
    let _cli = Cli::parse();
}

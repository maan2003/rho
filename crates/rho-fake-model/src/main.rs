use std::net::SocketAddr;

use clap::Parser;
use rho_fake_model::{FakeModel, FakeModelConfig, TimingMode};

#[derive(Parser)]
#[command(
    name = "rho-fake-model",
    about = "Deterministic OpenAI and Anthropic protocol server for Rho QA"
)]
struct Args {
    #[arg(long, default_value_t = 0)]
    seed: u64,
    #[arg(long, default_value = "127.0.0.1:0")]
    bind: SocketAddr,
    #[arg(long)]
    wall_clock_timing: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut config = FakeModelConfig::seeded(args.seed);
    config.bind = args.bind;
    if args.wall_clock_timing {
        config.timing.mode = TimingMode::Timed;
    }
    let model = FakeModel::start(config).await?;
    println!(
        "{}",
        serde_json::json!({
            "ready": true,
            "openai_base_url": model.openai_base_url(),
            "anthropic_base_url": model.anthropic_base_url(),
            "pid": std::process::id(),
        })
    );
    tokio::signal::ctrl_c().await?;
    model.shutdown().await
}

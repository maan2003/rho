use std::net::SocketAddr;

use clap::Parser;
use rho_fake_model::{FakeModel, FakeModelConfig, Scenario, TimingMode};

#[derive(Parser)]
#[command(
    name = "rho-fake-model",
    about = "Deterministic OpenAI and Anthropic protocol server for Rho QA"
)]
struct Args {
    #[arg(long, default_value_t = 0)]
    seed: u64,
    #[arg(long, value_enum, default_value_t)]
    scenario: Scenario,
    #[arg(long, default_value = "127.0.0.1:0")]
    bind: SocketAddr,
    #[arg(long)]
    wall_clock_timing: bool,
    /// Disable injected terminal faults for throughput/proof runs.
    #[arg(long)]
    no_faults: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut config = FakeModelConfig::seeded(args.seed);
    config.bind = args.bind;
    config.scenario = args.scenario;
    if args.wall_clock_timing || args.scenario == Scenario::SlowTrickle {
        config.timing.mode = TimingMode::Timed;
    }
    if args.no_faults {
        config.distribution.rate_limit_bps = 0;
        config.distribution.usage_limit_bps = 0;
        config.distribution.overload_bps = 0;
        config.distribution.disconnect_bps = 0;
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

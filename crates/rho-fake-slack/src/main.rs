//! The fake as a program: a Slack the rig can point rho at.
//!
//! ```text
//! rho-fake-slack --port 7300 --rate 5
//! RHO_SLACK_API_BASE=http://127.0.0.1:7300/api rho
//! ```
//!
//! It is the same server the in-process form starts, with the same store,
//! the same seed and the same schedule. What the binary adds is a port that
//! is known in advance and a control surface: `POST /control` takes the same
//! typed action the in-process handle takes, and `GET /control` hands back
//! what the server saw every connected client be told.

use clap::Parser;
use rho_fake_slack::{FakeSlack, Seed, world};

/// A Slack that is real enough to live in.
#[derive(Parser)]
#[command(name = "rho-fake-slack")]
struct Arguments {
    /// The port to serve on. Zero takes whatever is free and says which.
    #[arg(long, default_value_t = 7300)]
    port: u16,
    /// The number the whole workspace comes out of.
    #[arg(long, default_value_t = 1)]
    seed: u64,
    #[arg(long, default_value_t = 300)]
    conversations: usize,
    #[arg(long, default_value_t = 450_000)]
    messages: usize,
    #[arg(long, default_value_t = 120)]
    people: usize,
    /// How many things happen a second while clients are connected. Zero
    /// leaves the workspace still until something drives it over `/control`.
    #[arg(long, default_value_t = 1.0)]
    rate: f64,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let arguments = Arguments::parse();
    let seed = Seed {
        seed: arguments.seed,
        conversations: arguments.conversations,
        messages: arguments.messages,
        people: arguments.people,
        ..Seed::default()
    };
    let built_at = std::time::Instant::now();
    let store = world::build(seed);
    let conversations = store.conversation_count();
    let messages = store.message_count();
    let built = built_at.elapsed();

    let slack = FakeSlack::serve_on(&format!("127.0.0.1:{}", arguments.port), store).await?;
    slack.live(arguments.rate);

    // Said in the form the rig pastes: the environment variable rho reads,
    // not a sentence about it.
    println!(
        "seed              {} ({conversations} conversations, {messages} messages, {built:?})",
        arguments.seed
    );
    println!("RHO_SLACK_API_BASE={}", slack.api_base());
    println!("socket            {}", slack.socket_base());
    println!(
        "control           {}/control",
        slack.api_base().trim_end_matches("/api")
    );
    println!("rate              {} happenings/s", arguments.rate);

    // Nothing to do but hold the server open. Ctrl-C ends it, and the store
    // goes with it, because nothing here is written down.
    tokio::signal::ctrl_c().await?;
    println!(
        "\n{} happenings, {} frames published to {} clients",
        slack.happenings(),
        slack.observations().published,
        slack.connected(),
    );
    Ok(())
}

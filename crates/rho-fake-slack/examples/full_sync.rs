//! What the server costs, and what a client's first sync costs against it.
//!
//! ```text
//! cargo run --release --example full_sync
//! cargo run --release --example full_sync -- 300 450000
//! ```
//!
//! Four numbers, because they are four different claims: how long the world
//! takes to build, what it weighs once built, how many requests a second the
//! server sustains, and what one `rho-slack` client pays to learn the whole
//! workspace from cold. The last one is the one a reader feels, since it is
//! what happens between starting rho and seeing their list.

use std::time::Instant;

use rho_fake_slack::api::Method;
use rho_fake_slack::{FakeSlack, Seed, world};
use rho_slack::api::Client;
use rho_slack::config::Credentials;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let mut seed = Seed::default();
    if let Some(conversations) = arguments.next() {
        seed.conversations = conversations.parse()?;
    }
    if let Some(messages) = arguments.next() {
        seed.messages = messages.parse()?;
    }

    let built_at = Instant::now();
    let store = world::build(seed);
    let built = built_at.elapsed();
    println!("conversations             {}", store.conversation_count());
    println!("messages                  {}", store.message_count());
    println!("people                    {}", store.user_count());
    println!("seed                      {built:?}");
    println!("resident, world in hand   {}", resident());

    let slack = FakeSlack::serve(store).await?;
    let credentials = Credentials {
        workspace: rho_slack::config::WorkspaceName("acme".to_owned()),
        token: "xoxc-fake".to_owned(),
        cookie: "d=fake".to_owned(),
    };
    let client = Client::with_base(credentials, slack.api_base())?;

    // A first full sync as the client does it from cold: the list, the badge
    // counts, the mutes, the custom emoji, the followed threads, and then the
    // first page of every conversation, which is what fills the rows.
    let synced_at = Instant::now();
    let conversations = client.conversations().await?;
    let list = synced_at.elapsed();
    let counts = client.counts().await?;
    let _ = client.muted_channels().await?;
    let _ = client.custom_emoji().await?;
    let followed = client.followed_threads().await?;
    let overview = synced_at.elapsed();
    let mut messages = 0;
    for conversation in &conversations {
        messages += client
            .conversations_history(&conversation.id, None)
            .await?
            .messages
            .len();
    }
    let sync = synced_at.elapsed();
    println!();
    println!(
        "first sync, the list      {list:?}   ({} conversations)",
        conversations.len()
    );
    println!(
        "first sync, the overview  {overview:?}   ({} counts, {} followed threads)",
        counts.conversations.len(),
        followed.len()
    );
    println!(
        "first sync, whole         {sync:?}   ({messages} messages over {} requests)",
        slack.served()
    );
    println!(
        "  of which history        {}",
        slack.served_method(Method::ConversationsHistory)
    );
    println!("resident, after the sync  {}", resident());

    // How much the server takes while a client is asking for pages of history
    // as fast as it can: one conversation, the same window, repeatedly, so the
    // number is the server's and not the world's.
    let busiest = conversations
        .iter()
        .max_by_key(|conversation| conversation.name.len())
        .map(|conversation| conversation.id.clone())
        .expect("a conversation");
    let hammer_at = Instant::now();
    let mut served = 0u64;
    while hammer_at.elapsed().as_secs_f64() < 2.0 {
        client.conversations_history(&busiest, None).await?;
        served += 1;
    }
    let alone = served as f64 / hammer_at.elapsed().as_secs_f64();

    // And with eight clients asking at once, which is what "sustains" means
    // for a server several sessions and a GUI are pointed at.
    let together_at = Instant::now();
    let mut askers = Vec::new();
    for _ in 0..8 {
        let client = Client::with_base(
            Credentials {
                workspace: rho_slack::config::WorkspaceName("acme".to_owned()),
                token: "xoxc-fake".to_owned(),
                cookie: "d=fake".to_owned(),
            },
            slack.api_base(),
        )?;
        let channel = busiest.clone();
        askers.push(tokio::spawn(async move {
            let mut served = 0u64;
            let started = Instant::now();
            while started.elapsed().as_secs_f64() < 2.0 {
                if client.conversations_history(&channel, None).await.is_err() {
                    break;
                }
                served += 1;
            }
            served
        }));
    }
    let mut together = 0u64;
    for asker in askers {
        together += asker.await?;
    }
    let rate = together as f64 / together_at.elapsed().as_secs_f64();
    println!();
    println!("history, one client       {alone:.0} requests/s");
    println!("history, eight clients    {rate:.0} requests/s");
    Ok(())
}

/// Resident memory, read from the kernel rather than guessed at from the
/// sizes of the structures.
fn resident() -> String {
    let Ok(statm) = std::fs::read_to_string("/proc/self/statm") else {
        return "unknown".to_owned();
    };
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|pages| pages.parse().ok())
        .unwrap_or_default();
    format!("{} MiB", pages * 4 / 1024)
}

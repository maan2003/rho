//! What a crowd costs, and what the server can say about it.
//!
//! ```text
//! cargo run --release --example crowd
//! cargo run --release --example crowd -- 32 20000
//! ```
//!
//! Several `rho-slack` clients on one workspace, all reading their sockets,
//! while the schedule runs. The claims measured here: a frame is rendered
//! once however many clients there are, every client is handed the same
//! frames, the server can say so, and saying so costs a pass over the
//! conversations something happened in rather than over the workspace.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use rho_fake_slack::{FakeSlack, Seed, world};
use rho_slack::api::Client;
use rho_slack::config::{Credentials, WorkspaceName};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let clients: usize = arguments.next().map_or(Ok(32), |it| it.parse())?;
    let happenings: usize = arguments.next().map_or(Ok(20_000), |it| it.parse())?;

    let slack = FakeSlack::serve(world::build(Seed::default())).await?;
    println!(
        "conversations             {}",
        slack.store().conversation_count()
    );
    println!(
        "messages                  {}",
        slack.store().message_count()
    );
    println!("resident, nobody watching {}", resident());

    let seen = Arc::new(AtomicU64::new(0));
    let mut readers = Vec::new();
    let joined_at = Instant::now();
    for _ in 0..clients {
        let client = Client::with_base(credentials(), slack.api_base())?;
        let rtm = client.rtm_connect().await?;
        let (mut socket, _) =
            tokio_tungstenite::connect_async(client.socket_request(&rtm.url)?).await?;
        let seen = seen.clone();
        readers.push(tokio::spawn(async move {
            while let Some(Ok(_)) = socket.next().await {
                seen.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    while slack.connected() < clients {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    println!(
        "{clients} clients connected     {:?} ({:?} each)",
        joined_at.elapsed(),
        joined_at.elapsed() / clients as u32
    );
    println!("resident, {clients} watching     {}", resident());

    // The workspace lives while they all watch.
    let at = Instant::now();
    slack.advance(happenings);
    let published = at.elapsed();
    let settled_at = Instant::now();
    let settled = slack.settled(Duration::from_secs(30)).await;
    println!(
        "one happening, {clients} clients   {:?}",
        published / happenings as u32
    );
    println!(
        "everyone caught up        {}  after {:?}",
        settled,
        settled_at.elapsed()
    );

    // And the server says whether they agree, which is the whole point of
    // having several of them.
    let checked_at = Instant::now();
    let observations = slack.observations();
    let checked = checked_at.elapsed();
    // Again, warm: the first check pays for touching every client's account
    // of every conversation for the first time.
    let again_at = Instant::now();
    let again = slack.observations();
    let warm = again_at.elapsed();
    println!(
        "the check                 {checked:?} cold, {warm:?} warm   ({} clients, {} frames published)",
        observations.clients.len(),
        observations.published
    );
    assert_eq!(again.disagreements, observations.disagreements);
    println!(
        "they agree                {}{}",
        observations.agree(),
        match observations.disagreements.first() {
            Some(first) => format!("  ({first:?})"),
            None => String::new(),
        }
    );
    let delivered: Vec<u64> = observations
        .clients
        .iter()
        .map(|client| client.delivered)
        .collect();
    println!(
        "frames each               {} (min {}, max {})",
        observations.published,
        delivered.iter().min().copied().unwrap_or_default(),
        delivered.iter().max().copied().unwrap_or_default(),
    );
    println!("frames delivered in all   {}", seen.load(Ordering::Relaxed));
    // The burst above is not a workspace, it is a stress: 20,000 happenings
    // as fast as the server can apply them. A rate is what a workspace
    // actually does, so the last number is the crowd keeping up with one.
    let before = seen.load(Ordering::Relaxed);
    let at = Instant::now();
    slack.live(2_000.0);
    tokio::time::sleep(Duration::from_secs(3)).await;
    slack.still();
    let caught_up = slack.settled(Duration::from_secs(10)).await;
    let living = slack.observations();
    println!(
        "at 2,000/s for 3s         {} frames to {clients} clients in {:?}, caught up {caught_up}",
        seen.load(Ordering::Relaxed) - before,
        at.elapsed(),
    );
    println!(
        "they agree                {}{}",
        living.agree(),
        match living.disagreements.first() {
            Some(first) => format!("  ({first:?})"),
            None => String::new(),
        }
    );
    println!("resident, after living    {}", resident());
    for reader in readers {
        reader.abort();
    }
    Ok(())
}

fn credentials() -> Credentials {
    Credentials {
        workspace: WorkspaceName("acme".to_owned()),
        token: "xoxc-fake".to_owned(),
        cookie: "d=fake".to_owned(),
    }
}

fn resident() -> String {
    let Ok(statm) = std::fs::read_to_string("/proc/self/statm") else {
        return "unknown".to_owned();
    };
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|pages| pages.parse().ok())
        .unwrap_or_default();
    format!("{} MiB", pages * 4096 / (1024 * 1024))
}

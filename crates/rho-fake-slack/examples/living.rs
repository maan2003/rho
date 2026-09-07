//! What time costs: how fast the workspace can live, and how long a
//! happening takes to reach a connected client.
//!
//! ```text
//! cargo run --release --example living
//! cargo run --release --example living -- 8 2000
//! ```
//!
//! Three claims. One happening is a draw, a binary search and an append, so
//! its cost does not grow with the workspace — measured here at the default
//! 450,000 messages against a small world, which is the only honest way to
//! say "does not grow". A frame is rendered once and handed to every socket,
//! so a second client costs a pointer and not a serialisation. And what a
//! client waits between something happening and hearing about it, which is
//! the number a reader actually feels.

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
    let clients: usize = arguments.next().map_or(Ok(8), |it| it.parse())?;
    let rate: f64 = arguments.next().map_or(Ok(2_000.0), |it| it.parse())?;

    // A happening against a full workspace, and against a small one. If the
    // cost were a pass over a conversation or over the world, these two
    // would not be the same number.
    for seed in [
        Seed {
            conversations: 8,
            messages: 2_000,
            ..Seed::default()
        },
        Seed::default(),
    ] {
        let slack = FakeSlack::serve(world::build(seed)).await?;
        slack.advance(1_000);
        let at = Instant::now();
        let happenings = 20_000;
        slack.advance(happenings);
        let each = at.elapsed() / happenings as u32;
        println!(
            "one happening             {each:?}   ({} conversations, {} messages)",
            slack.store().conversation_count(),
            slack.store().message_count(),
        );
    }

    let slack = Arc::new(FakeSlack::serve(world::build(Seed::default())).await?);
    println!("resident, world in hand   {}", resident());

    // Every client connects the way rho does: the URL from `rtm.connect`,
    // then the handshake `rho-slack` builds.
    let seen = Arc::new(AtomicU64::new(0));
    let mut readers = Vec::new();
    for _ in 0..clients {
        let client = Client::with_base(credentials(), slack.api_base())?;
        let rtm = client.rtm_connect().await?;
        let request = client.socket_request(&rtm.url)?;
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await?;
        let seen = seen.clone();
        readers.push(tokio::spawn(async move {
            while let Some(Ok(frame)) = socket.next().await {
                let tokio_tungstenite::tungstenite::Message::Text(text) = frame else {
                    continue;
                };
                if text.contains("\"hello\"") {
                    continue;
                }
                seen.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    while slack.connected() < clients {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    println!("clients on the socket     {}", slack.connected());

    // What a client waits: one happening at a time, timed from the call
    // that made it to the frame arriving at every socket.
    let mut waits = Vec::new();
    for _ in 0..200 {
        let before = seen.load(Ordering::Relaxed);
        let at = Instant::now();
        slack.advance(1);
        // A read cursor for somebody else is not a frame, so a draw that
        // makes no frame is not a wait to measure.
        let mut waited = None;
        while at.elapsed() < Duration::from_millis(200) {
            if seen.load(Ordering::Relaxed) >= before + slack.connected() as u64 {
                waited = Some(at.elapsed());
                break;
            }
            tokio::time::sleep(Duration::from_micros(50)).await;
        }
        if let Some(waited) = waited {
            waits.push(waited);
        }
    }
    waits.sort();
    if !waits.is_empty() {
        println!(
            "a happening reaches {clients}     {:?} median, {:?} at the 99th",
            waits[waits.len() / 2],
            waits[waits.len() * 99 / 100],
        );
    }
    // What one happening costs to announce, measured from the server: the
    // frame is rendered once and every socket gets the same pointer.
    let at = Instant::now();
    let announced = 2_000;
    slack.advance(announced);
    let published = at.elapsed();
    println!(
        "one happening, {clients} sockets  {:?}",
        published / announced as u32
    );

    // And the rate, sustained: time running on the clock rather than in a
    // loop, with every client keeping up or being told it did not.
    let before = seen.load(Ordering::Relaxed);
    let happenings_before = slack.happenings();
    slack.live(rate);
    tokio::time::sleep(Duration::from_secs(3)).await;
    slack.still();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let happened = slack.happenings() - happenings_before;
    let delivered = seen.load(Ordering::Relaxed) - before;
    println!(
        "asked for                 {rate}/s for 3s\nhappened                  {}/s\ndelivered                 {delivered} frames to {} clients still connected",
        happened / 3,
        slack.connected(),
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

/// Resident memory, from the kernel rather than from a guess.
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

//! The responsiveness half of slice B's backfill proof: while the
//! background job builds stories on a copy of the user's store, a client
//! connects and asks for the agent list, and this prints how long the
//! daemon took to answer. Point `RHO_PROBE_SOCKET` at the copy's daemon.
//! Deleted with the migration.

use std::time::{Duration, Instant};

use rho_ui_proto::client::Client;
use rho_ui_proto::{ClientMessage, ServerMessage};

#[tokio::test]
#[ignore = "needs a daemon on RHO_PROBE_SOCKET serving a copy of the store"]
async fn the_daemon_answers_while_the_backfill_runs() {
    let socket = std::env::var("RHO_PROBE_SOCKET").expect("RHO_PROBE_SOCKET must name a socket");
    let probes: usize = std::env::var("RHO_PROBES")
        .ok()
        .and_then(|probes| probes.parse().ok())
        .unwrap_or(20);
    let mut slowest = Duration::ZERO;
    for probe in 0..probes {
        let started = Instant::now();
        // Every client speaks first; `Ready` comes back unasked after it.
        let mut client = Client::connect(&socket).await.expect("connect");
        client.send(&ClientMessage::Subscribe).await.expect("send");
        let agents = loop {
            match client.recv().await.expect("ready") {
                ServerMessage::Ready { agents, .. } => break agents.len(),
                _ => continue,
            }
        };
        let elapsed = started.elapsed();
        slowest = slowest.max(elapsed);
        println!(
            "probe {probe}: {agents} agents listed in {:.0} ms",
            elapsed.as_secs_f64() * 1000.0
        );
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
    println!("probe: slowest {:.0} ms", slowest.as_secs_f64() * 1000.0);
}

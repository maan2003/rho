//! The dev shell cache daemon, run by `rho-agent-host`. It keeps the
//! entries in `shells.redb` in the cache directory
//! ([`rho_devshell::devshell_dir`]) and serves its socket until it is killed.
//!
//!     rho-devshell-daemon                 serve
//!     rho-devshell-daemon stats [HOURS]   summarise the logged events, then
//!                                         list each that ran an evaluation
//!     rho-devshell-daemon events          list every logged event

use std::sync::Arc;

use anyhow::{Context as _, Result};
use rho_db::RhoDb;
use rho_devshell::{Event, Record, Stats};
use rho_devshell_daemon::Store;

const USAGE: &str = "usage: rho-devshell-daemon [stats [HOURS] | events]";

fn main() -> Result<()> {
    let dir = rho_devshell::devshell_dir()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let records = || async { rho_devshell::Client::new(&dir).records().await };
        match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
            [] => {
                let store =
                    Arc::new(Store::open(RhoDb::open(dir.join("shells.redb")), dir.clone()).await);
                store.serve().context("serve the dev shell cache")?.await;
            }
            ["stats", ref hours @ ..] if hours.len() <= 1 => {
                let hours: Option<f64> = hours
                    .first()
                    .map(|h| h.parse())
                    .transpose()
                    .context(USAGE)?;
                let now = chrono::Utc::now().timestamp_millis() as u64;
                let records: Vec<Record> = records()
                    .await?
                    .into_iter()
                    .filter(|r| {
                        hours.is_none_or(|h| now.saturating_sub(r.at_ms) as f64 <= h * 3_600_000.0)
                    })
                    .collect();
                println!("{}\n", Stats::of(&records));
                for record in records
                    .iter()
                    .filter(|r| !matches!(r.event, Event::Hit { .. } | Event::Kept { .. }))
                {
                    println!("{}", line(record));
                }
            }
            ["events"] => {
                for record in records().await? {
                    println!("{}", line(&record));
                }
            }
            _ => anyhow::bail!(USAGE),
        }
        Ok(())
    })
}

/// One event: local time, what happened and how long it took, and the flake.
fn line(record: &Record) -> String {
    let at = chrono::DateTime::from_timestamp_millis(record.at_ms as i64)
        .unwrap_or_default()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S");
    let what = match record.event {
        Event::Hit { ms } => format!("{ms:>8} ms  hit"),
        Event::Miss { ms, stale: false } => format!("{ms:>8} ms  miss, new key"),
        Event::Miss { ms, stale: true } => format!("{ms:>8} ms  miss, inputs changed"),
        Event::Uncached { ms } => format!("{ms:>8} ms  evaluated, not cacheable"),
        Event::Failed { ms } => format!("{ms:>8} ms  failed"),
        Event::Kept { uses } => format!("{uses:>8} x   kept"),
    };
    format!("{at}  {what:<36}  {}", record.flake)
}

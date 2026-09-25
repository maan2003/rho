//! The dev shell cache daemon, run by `rho-agent-host`. It keeps the
//! entries in `shells.redb` in the cache directory
//! ([`rho_devshell::devshell_dir`]) and serves its socket until it is killed.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use rho_db::RhoDb;
use rho_devshell_daemon::Store;

fn main() -> Result<()> {
    let dir = rho_devshell::devshell_dir()?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async {
        let store = Arc::new(Store::open(RhoDb::open(dir.join("shells.redb")), dir).await);
        store.serve().context("serve the dev shell cache")?.await;
        Ok(())
    })
}

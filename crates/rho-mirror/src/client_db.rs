//! Opening the client's one database, on the model thread.
//!
//! `main` names the state directory and nothing else; the file is opened
//! here, where no frame waits on it. What comes back is handed to the
//! agent mirror and the desk replica directly, and to everything else —
//! the journal, the Slack mirror, the inbox — through
//! [`rho_db::client::on_open`], which `main` registers before this runs.

use rho_db::RhoDb;

/// Opens the database in the state directory `main` named, if it named
/// one. `None` is a session with no caches: every write below is a no-op
/// and every reader asks the daemon, which is what a test wants and what
/// a client that cannot open its file falls back to.
pub fn open_stated() -> Option<RhoDb> {
    let state_dir = crate::mirror::state_dir()?;
    match rho_db::client::open_shared(state_dir) {
        Ok(db) => Some(db),
        Err(error) => {
            tracing::warn!(%error, "the client database is unavailable; this session keeps nothing of its own");
            None
        }
    }
}

//! The journal observer: it tells the agent host about every append after it
//! commits. What a client is told of it is the agent host's business; this
//! only says what happened, in the runtime's own words.

use rho_agent_types::{AgentId, AgentPos, Seq};
use rho_db::RhoDb;

/// One row, the moment it became durable. The row is read from the
/// journal; this only says where it landed.
#[derive(Clone, Copy, Debug)]
pub struct LogAppended {
    pub seq: Seq,
    pub agent_id: AgentId,
    pub pos: AgentPos,
}

/// The database's journal observer. Held in the database's own observer
/// slot rather than passed down, because rows are appended from a dozen
/// places and none of them should have to carry a channel.
pub struct Journal {
    feed: tokio::sync::broadcast::Sender<LogAppended>,
}

/// Every row appended to this database from now on. A slow reader lags
/// rather than blocking the writer; a lagged reader catches up from the
/// journal table, which is the truth.
pub fn feed(db: &RhoDb) -> tokio::sync::broadcast::Receiver<LogAppended> {
    db.observer(Journal::new).feed.subscribe()
}

impl Journal {
    fn new() -> Self {
        Self {
            feed: tokio::sync::broadcast::Sender::new(4096),
        }
    }

    pub(crate) fn sender(&self) -> tokio::sync::broadcast::Sender<LogAppended> {
        self.feed.clone()
    }
}

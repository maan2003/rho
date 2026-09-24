//! The journal observer: it tells the agent host about every append after it
//! commits, and carries each loop's status in the same feed. What a client is
//! told of either is the agent host's business; this only says what happened,
//! in the runtime's own words.

use std::sync::Arc;

use rho_agent_types::{AgentId, AgentPos, Seq};
use rho_db::RhoDb;

use crate::{AgentStatus, QueuedInput};

/// One row, the moment it became durable. The row is read from the
/// journal; this only says where it landed.
#[derive(Clone, Copy, Debug)]
pub struct LogAppended {
    pub seq: Seq,
    pub agent_id: AgentId,
    pub pos: AgentPos,
}

/// One thing a connection forwards, in the order it happened: a row
/// became durable, or a loop's status changed. One feed for both is what
/// makes the ordering rule hold: a loop writes its row, the commit hook
/// sends `Appended`, and only then does the same task send its `Status`.
#[derive(Clone, Debug)]
pub enum Feed {
    Appended(LogAppended),
    Status {
        agent_id: AgentId,
        status: Arc<AgentStatus>,
        /// The live queue, for a loop whose queue is not rows (Claude).
        queue: Option<Arc<[QueuedInput]>>,
        /// Whatever was told of this loop's tail before is void: tell it
        /// whole.
        reset: bool,
    },
}

/// The database's journal observer. Held in the database's own observer
/// slot rather than passed down, because rows are appended from a dozen
/// places and none of them should have to carry a channel.
pub struct Journal {
    feed: tokio::sync::broadcast::Sender<Feed>,
}

/// Every row appended to this database from now on, and every status
/// any loop reports. A slow reader lags rather than blocking the writer;
/// a lagged reader catches up from the journal table, which is the
/// truth, and asks the loops to tell their tails again.
pub fn feed(db: &RhoDb) -> tokio::sync::broadcast::Receiver<Feed> {
    db.observer(Journal::new).feed.subscribe()
}

/// A loop saying what its status is now.
pub fn tell_status(
    db: &RhoDb,
    agent_id: AgentId,
    status: Arc<AgentStatus>,
    queue: Option<Arc<[QueuedInput]>>,
    reset: bool,
) {
    let _ = db.observer(Journal::new).feed.send(Feed::Status {
        agent_id,
        status,
        queue,
        reset,
    });
}

impl Journal {
    fn new() -> Self {
        Self {
            feed: tokio::sync::broadcast::Sender::new(4096),
        }
    }

    pub(crate) fn sender(&self) -> tokio::sync::broadcast::Sender<Feed> {
        self.feed.clone()
    }
}

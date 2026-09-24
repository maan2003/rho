//! One host's agents, as the client reaches them: calls and
//! visualizations, each on a stream of its own. The session beside them is
//! [`crate::stream`].

use std::future::Future;

use rho_agent_hosts::{Dialer, Link};
use rho_rpc::protocol::Call;

use crate::protocol::{self, VisualizationContent};

/// One host's agents, as a client reaches them. Cheap to clone; valid
/// across reconnects, since each use dials whatever connection is up.
#[derive(Clone)]
pub struct AgentsLink {
    link: Link,
}

impl AgentsLink {
    pub fn new(link: Link) -> Self {
        Self { link }
    }

    /// Agents with no host behind them, for an agent whose host has been
    /// detached: its retained transcript still renders, and asking for
    /// anything reports the same "not connected" as a dropped connection.
    pub fn detached() -> Self {
        Self::new(Link::detached())
    }

    /// Makes one call on a stream of its own. The answer needs no
    /// particular executor; a refusal is an error.
    pub fn call<C: Call>(
        &self,
        call: C,
    ) -> impl Future<Output = anyhow::Result<C::Reply>> + Send + 'static {
        self.link.run(|dialer| dial_call(dialer, call))
    }

    /// A recorded visualization.
    pub fn visualization(
        &self,
        id: String,
    ) -> impl Future<Output = anyhow::Result<VisualizationContent>> + Send + 'static {
        self.call(protocol::Visualization { id })
    }
}

/// One call on a stream of its own. A refusal is an error.
async fn dial_call<C: Call>(dialer: Dialer, call: C) -> anyhow::Result<C::Reply> {
    let mut stream = dialer.open(C::PRIORITY).await?;
    rho_rpc::protocol::call(&mut stream, call).await
}

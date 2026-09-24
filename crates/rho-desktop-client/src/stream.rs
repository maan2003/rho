//! Each host's desktops stream: the desktops the host's agents run,
//! whole, each time they change.

use std::sync::Arc;

use futures::channel::mpsc as futures_mpsc;
use futures::future::BoxFuture;
use rho_agent_host_proto::host::Open as HostOpen;
use rho_agent_host_proto::{DesktopSession, read_frame, write_open};
use rho_hosts::{Dialer, HostId, HostStream};

/// A host's desktops, as it now has them.
pub struct DesktopsEvent {
    pub host: HostId,
    pub sessions: Vec<DesktopSession>,
}

/// Every attached host's desktops stream, to one channel.
pub struct DesktopStreams {
    events: futures_mpsc::UnboundedSender<DesktopsEvent>,
}

impl DesktopStreams {
    /// No host yet; the receiver hears every host's desktops.
    pub fn new() -> (Self, futures_mpsc::UnboundedReceiver<DesktopsEvent>) {
        let (events, events_rx) = futures_mpsc::unbounded();
        (Self { events }, events_rx)
    }

    /// The desktops stream for a host just attached, for the host to open
    /// on every connection.
    pub fn stream(&self, host: HostId) -> Arc<dyn HostStream> {
        Arc::new(DesktopsStream {
            host,
            events: self.events.clone(),
        })
    }
}

struct DesktopsStream {
    host: HostId,
    events: futures_mpsc::UnboundedSender<DesktopsEvent>,
}

impl HostStream for DesktopsStream {
    fn name(&self) -> &'static str {
        "desktops"
    }

    fn run(&self, dialer: Dialer) -> BoxFuture<'static, anyhow::Result<()>> {
        let host = self.host;
        let events = self.events.clone();
        Box::pin(async move {
            let mut stream = dialer.open(None).await?;
            write_open(&mut stream, &HostOpen::Desktops).await?;
            loop {
                let sessions: Vec<DesktopSession> = read_frame(&mut stream).await?;
                if events
                    .unbounded_send(DesktopsEvent { host, sessions })
                    .is_err()
                {
                    return Ok(());
                }
            }
        })
    }
}

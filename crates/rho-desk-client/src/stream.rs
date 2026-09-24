//! Each host's desk stream: the host's copy of the desk to the window, the
//! window's syncs and writes back.
//!
//! The host opens it on every connection
//! ([`rho_hosts::HostStream`]); what is said on it and where its frames go
//! are this crate's.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use futures::channel::mpsc as futures_mpsc;
use futures::future::BoxFuture;
use rho_agent_host_proto::desk::stream::{ClientFrame, ServerFrame};
use rho_agent_host_proto::{Open, read_frame, write_frame};
use rho_hosts::{Dialer, HostId, HostStream};

/// What a host says on its desk stream.
pub enum DeskFrame {
    /// The stream is open. Every (re)opened stream starts here, and the
    /// client's `Sync` goes after it: a stream that has not synced may not
    /// write.
    Opened,
    /// The answer to `Sync`.
    Synced {
        store: rho_agent_host_proto::desk::cells::DeviceId,
        node_namespace: u16,
        delta: rho_agent_host_proto::desk::cells::Snapshot,
        bodies: Vec<rho_agent_host_proto::desk::cells::BodySnapshot>,
    },
    /// The host's copy moved; sync if `frontier` is past what is held.
    CellsAvailable {
        frontier: rho_agent_host_proto::desk::cells::Version,
    },
    /// A body edit, from whichever device made it.
    TextApplied {
        id: rho_agent_host_proto::desk::cells::Id,
        operation: rho_agent_host_proto::desk::TextOperation,
    },
    /// The stream missed some of the host's pokes; sync again.
    ResyncRequired,
}

/// A desk-stream frame tagged with the host it came from.
pub struct DeskEvent {
    pub host: HostId,
    pub frame: DeskFrame,
}

/// Every attached host's desk stream: their frames to one channel, in the
/// order they arrive, and the way to speak on each.
pub struct DeskStreams {
    events: futures_mpsc::UnboundedSender<DeskEvent>,
    hosts: Mutex<HashMap<HostId, futures_mpsc::UnboundedSender<ClientFrame>>>,
    #[cfg(feature = "test-support")]
    sent: Mutex<HashMap<HostId, Vec<ClientFrame>>>,
}

impl DeskStreams {
    /// No host yet; the receiver hears every host's frames.
    pub fn new() -> (Self, futures_mpsc::UnboundedReceiver<DeskEvent>) {
        let (events, events_rx) = futures_mpsc::unbounded();
        let streams = Self {
            events,
            hosts: Mutex::default(),
            #[cfg(feature = "test-support")]
            sent: Mutex::default(),
        };
        (streams, events_rx)
    }

    /// The desk stream for a host just attached, for the host to open on
    /// every connection.
    pub fn stream(&self, host: HostId) -> Arc<dyn HostStream> {
        let (commands, commands_rx) = futures_mpsc::unbounded();
        self.hosts.lock().unwrap().insert(host, commands);
        Arc::new(DeskStream {
            host,
            events: self.events.clone(),
            commands: Arc::new(tokio::sync::Mutex::new(commands_rx)),
        })
    }

    /// Says `frame` on the host's desk stream. What is said while the
    /// stream is down is dropped when it opens again; the handshake after
    /// `Opened` carries whatever it held.
    pub fn send(&self, host: HostId, frame: ClientFrame) {
        #[cfg(feature = "test-support")]
        self.sent
            .lock()
            .unwrap()
            .entry(host)
            .or_default()
            .push(frame.clone());
        if let Some(commands) = self.hosts.lock().unwrap().get(&host) {
            let _ = commands.unbounded_send(frame);
        }
    }

    pub fn detach(&self, host: HostId) {
        self.hosts.lock().unwrap().remove(&host);
    }

    /// What was said to a host since the last call.
    #[cfg(feature = "test-support")]
    pub fn take_sent_for_test(&self, host: HostId) -> Vec<ClientFrame> {
        self.sent.lock().unwrap().remove(&host).unwrap_or_default()
    }
}

/// One host's desk stream, as the host keeps it across reconnects.
struct DeskStream {
    host: HostId,
    events: futures_mpsc::UnboundedSender<DeskEvent>,
    commands: Arc<tokio::sync::Mutex<futures_mpsc::UnboundedReceiver<ClientFrame>>>,
}

impl HostStream for DeskStream {
    fn name(&self) -> &'static str {
        "desk"
    }

    /// For as long as the connection lasts. Ends with an error when either
    /// direction does, or when a newer window takes this device.
    fn run(&self, dialer: Dialer) -> BoxFuture<'static, anyhow::Result<()>> {
        let host = self.host;
        let events = self.events.clone();
        let commands = self.commands.clone();
        Box::pin(async move {
            // Interactive streams outrank calls and sessions (priority 1 and below).
            let mut socket = dialer.open(Some(50)).await?;
            write_frame(&mut socket, &Open::Desk).await?;
            let (mut reader, mut writer) = tokio::io::split(socket);
            let mut commands = commands.lock().await;
            // Written for the last stream; the handshake after `Opened`
            // carries whatever they held.
            while commands.try_recv().is_ok() {}
            let send = |frame| events.unbounded_send(DeskEvent { host, frame });
            if send(DeskFrame::Opened).is_err() {
                return Ok(());
            }
            let read = async {
                loop {
                    let frame = match read_frame::<_, ServerFrame>(&mut reader).await? {
                        ServerFrame::Synced {
                            store,
                            node_namespace,
                            delta,
                            bodies,
                        } => DeskFrame::Synced {
                            store,
                            node_namespace,
                            delta,
                            bodies,
                        },
                        ServerFrame::CellsAvailable { frontier } => {
                            DeskFrame::CellsAvailable { frontier }
                        }
                        ServerFrame::TextApplied {
                            id,
                            operation,
                            transaction: _,
                        } => DeskFrame::TextApplied { id, operation },
                        ServerFrame::ResyncRequired => DeskFrame::ResyncRequired,
                        ServerFrame::Displaced => {
                            anyhow::bail!("the desk moved to a newer window on this device")
                        }
                    };
                    if send(frame).is_err() {
                        return Ok(());
                    }
                }
            };
            let write = async {
                while let Some(frame) = commands.next().await {
                    write_frame(&mut writer, &frame).await?;
                }
                anyhow::Ok(())
            };
            tokio::select! {
                result = read => result,
                result = write => result,
            }
        })
    }
}

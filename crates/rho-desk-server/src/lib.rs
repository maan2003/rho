//! The desk, as an agent host keeps it.
//!
//! The desk is the user's: the client makes every cell and verdict, and a
//! host keeps a copy so that devices can sync through it. [`DeskServer`] is
//! that copy and the streams that reach it; [`store`] is the copy on disk.
//! The wire is `rho_agent_host_proto::desk::stream`.

pub mod store;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rho_agent_host_proto::desk::cells::DeviceId;
use rho_agent_host_proto::desk::stream::{ClientFrame, ServerFrame};
use rho_db::RhoDb;
use rho_rpc::parts::{read_frame_optional, write_frame};
use tokio::sync::{Mutex, Notify, broadcast, mpsc};

use crate::store::DeskCellStore;

/// One GUI's hold on a desk device.
///
/// A device is one GUI, and the CRDT gives each device its own namespace, so
/// two live writers under one device id would collide on versions. The hold
/// is therefore exclusive — but exclusive to the *newest* connection: a GUI
/// that died without closing leaves its hold behind, and the GUI the user
/// just restarted must not be the one that is refused.
struct DeskBinding {
    /// Which stream holds it, so a stream ending only lets go of a hold
    /// that is still its own.
    connection: u64,
    /// The displaced stream's writer, to tell it why it is going.
    outgoing: mpsc::UnboundedSender<ServerFrame>,
    /// Set when a newer stream takes the device. A displaced stream may not
    /// write: its mutations are refused from this moment, whether or not
    /// its socket has noticed yet.
    displaced: AtomicBool,
    /// Wakes the displaced stream's read loop so it ends rather than
    /// sitting on a socket nobody is reading.
    closed: Notify,
}

/// What a desk stream holds after `Sync`.
struct DeskSession {
    device: DeviceId,
    node_namespace: u16,
    binding: Arc<DeskBinding>,
}

/// The host's copy of the desk, and every stream that syncs with it.
pub struct DeskServer {
    store: DeskCellStore,
    devices: Mutex<HashMap<DeviceId, Arc<DeskBinding>>>,
    /// The fanout: every desk stream hears when the host's copy moves,
    /// whichever stream moved it.
    events: broadcast::Sender<ServerFrame>,
    next_stream: AtomicU64,
}

impl DeskServer {
    pub async fn open(db: RhoDb) -> anyhow::Result<Self> {
        Ok(Self {
            store: DeskCellStore::new(db).await.map_err(anyhow::Error::msg)?,
            devices: Mutex::new(HashMap::new()),
            events: broadcast::channel(1024).0,
            next_stream: AtomicU64::new(1),
        })
    }

    /// A desk stream: one GUI's replica kept in step with the host's copy.
    ///
    /// Every desk stream hears the host's pokes, synced or not; writing waits
    /// on `Sync`, which binds the stream to its device. An error ends the
    /// stream, and the client comes back and syncs again.
    pub async fn serve<R, W>(&self, mut reader: R, writer: W) -> anyhow::Result<()>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<ServerFrame>();
        let writer_task = tokio::spawn(async move {
            let mut writer = writer;
            while let Some(frame) = outgoing_rx.recv().await {
                if write_frame(&mut writer, &frame).await.is_err() {
                    break;
                }
            }
        });
        // Forwarded by a task of its own rather than raced against the read
        // below: a read cut off halfway through a frame loses it.
        let mut events_rx = self.events.subscribe();
        let events_tx = outgoing_tx.clone();
        let events_task = tokio::spawn(async move {
            loop {
                let frame = match events_rx.recv().await {
                    Ok(frame) => frame,
                    Err(broadcast::error::RecvError::Lagged(_)) => ServerFrame::ResyncRequired,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if events_tx.send(frame).is_err() {
                    break;
                }
            }
        });
        let stream_id = self.next_stream.fetch_add(1, Ordering::Relaxed);
        let mut session: Option<DeskSession> = None;
        let result = loop {
            // A displaced stream stops here rather than sitting on a socket
            // nobody is reading: the GUI that held this device has been told,
            // and the window that took it is the live one.
            let displaced = session.as_ref().map(|session| Arc::clone(&session.binding));
            let frame = read_frame_optional::<_, ClientFrame>(&mut reader);
            let read = match displaced {
                Some(binding) => {
                    tokio::select! {
                        biased;
                        () = binding.closed.notified() => break Ok(()),
                        read = frame => read,
                    }
                }
                None => frame.await,
            };
            let frame = match read {
                Ok(Some(frame)) => frame,
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
            };
            if let Err(error) = self
                .handle(&outgoing_tx, stream_id, &mut session, frame)
                .await
            {
                break Err(error);
            }
        };
        // Let go of the device only if the hold is still this stream's: a newer
        // window may have taken it, and ending must not unbind theirs.
        if let Some(session) = session {
            let mut devices = self.devices.lock().await;
            if devices
                .get(&session.device)
                .is_some_and(|held| Arc::ptr_eq(held, &session.binding))
            {
                devices.remove(&session.device);
            }
        }
        events_task.abort();
        let _ = events_task.await;
        // The last frame, `Displaced` among them, still goes out: every sender
        // is gone now, so the writer ends once it has written what it holds.
        drop(outgoing_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), writer_task).await;
        result
    }

    async fn handle(
        &self,
        outgoing_tx: &mpsc::UnboundedSender<ServerFrame>,
        stream_id: u64,
        session: &mut Option<DeskSession>,
        frame: ClientFrame,
    ) -> anyhow::Result<()> {
        match frame {
            ClientFrame::Sync {
                device,
                known,
                store,
                bodies,
            } => {
                if session
                    .as_ref()
                    .is_some_and(|session| session.device != device)
                {
                    anyhow::bail!("Desk stream is already bound to another device");
                }
                let node_namespace = self
                    .store
                    .node_namespace(device)
                    .await
                    .map_err(anyhow::Error::msg)?;
                // The client says which store it counted `known` in. If that
                // is not this store, the answer is the whole of this one: the
                // client's numbers were counted elsewhere, and a difference
                // taken from them would leave it holding a desk made of two
                // stores at once.
                let (store, delta) = self
                    .store
                    .sync_for(store, &known)
                    .map_err(anyhow::Error::msg)?;
                let binding = match session.take() {
                    // This stream already holds the device: syncing again is a
                    // resync, not a second writer.
                    Some(session) => session.binding,
                    None => {
                        let binding = Arc::new(DeskBinding {
                            connection: stream_id,
                            outgoing: outgoing_tx.clone(),
                            displaced: AtomicBool::new(false),
                            closed: Notify::new(),
                        });
                        // Newest wins. A device is one GUI, so a hold that is
                        // still standing belongs to a GUI that has died or is
                        // stale — the user has just restarted theirs, and
                        // refusing it would leave them with no desk until the
                        // transport gave up on the old connection, which over
                        // iroh is the ten minutes of
                        // `rho_iroh_auth::AUTHENTICATED_IDLE_TIMEOUT`.
                        if let Some(held) = self
                            .devices
                            .lock()
                            .await
                            .insert(device, Arc::clone(&binding))
                            && held.connection != stream_id
                        {
                            held.displaced.store(true, Ordering::SeqCst);
                            let _ = held.outgoing.send(ServerFrame::Displaced);
                            held.closed.notify_one();
                        }
                        binding
                    }
                };
                *session = Some(DeskSession {
                    device,
                    node_namespace,
                    binding,
                });
                let _ = outgoing_tx.send(ServerFrame::Synced {
                    store,
                    node_namespace,
                    delta,
                    bodies: self.store.bodies_since(&bodies),
                });
                Ok(())
            }
            ClientFrame::CellsApply { cells } => {
                // The other half of the handshake. The same two conditions as a
                // mutation, and for the same reason: they are about this
                // stream, not about what the cells say.
                let Some(session) = session.as_ref() else {
                    anyhow::bail!("Desk stream must sync before writing");
                };
                anyhow::ensure!(
                    !session.binding.displaced.load(Ordering::SeqCst),
                    "The desk moved to a newer window on this device"
                );
                match self.store.apply_cells(cells).await {
                    Ok(()) => {
                        let frontier = self.store.frontier().map_err(anyhow::Error::msg)?;
                        let _ = self.events.send(ServerFrame::CellsAvailable { frontier });
                    }
                    Err(error) => {
                        tracing::warn!(%error, device = ?session.device,
                            "a client's catch-up cells did not merge");
                    }
                }
                Ok(())
            }
            ClientFrame::MutationApply { mutation } => {
                let stamp = mutation.stamp;
                // Nothing here is a verdict on what the user wrote: the two
                // conditions below are about this stream, and they end it the
                // same way the text path does. The desk is the client's, and
                // the daemon holds a copy so that clients can sync through it.
                let Some(session) = session.as_ref() else {
                    anyhow::bail!("Desk stream must sync before writing");
                };
                // Displaced, so this stream's device id belongs to another
                // window now: writing under it would put two authors in one
                // CRDT namespace.
                anyhow::ensure!(
                    !session.binding.displaced.load(Ordering::SeqCst),
                    "The desk moved to a newer window on this device"
                );
                let device = session.device;
                match self.store.apply_mutation(device, mutation).await {
                    // No answer goes back. The write was done on the client
                    // when the client made it; what the other devices need is
                    // the poke that says there is something to sync.
                    Ok(()) => {
                        let frontier = self.store.frontier().map_err(anyhow::Error::msg)?;
                        let _ = self.events.send(ServerFrame::CellsAvailable { frontier });
                    }
                    // What is left is a mutation that could not be decoded into
                    // the store at all. There is no answer for it any more, and
                    // the client is not waiting for one; the log is where it
                    // goes.
                    Err(error) => {
                        tracing::warn!(%error, device = ?device, version = stamp.version,
                            "a desk mutation did not merge");
                    }
                }
                Ok(())
            }
            ClientFrame::TextApply {
                id,
                operation,
                transaction,
            } => {
                let Some(session) = session.as_ref() else {
                    anyhow::bail!("Desk stream must sync before writing text");
                };
                anyhow::ensure!(
                    !session.binding.displaced.load(Ordering::SeqCst),
                    "The desk moved to a newer window on this device"
                );
                let namespace = session.node_namespace;
                if self
                    .store
                    .apply_body(
                        namespace,
                        id.clone(),
                        operation.clone(),
                        transaction.clone(),
                    )
                    .await
                    .map_err(anyhow::Error::msg)?
                {
                    let _ = self.events.send(ServerFrame::TextApplied {
                        id,
                        operation,
                        transaction,
                    });
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// A device is one GUI, and the newest window wins it.
    ///
    /// The user's GUI panicked and restarted; the daemon still held the old
    /// connection's binding, and the restarted GUI was refused with "Desk
    /// device already has an active writer connection" until the transport
    /// gave up on the dead one — over iroh that is the ten minutes of
    /// `rho_iroh_auth::AUTHENTICATED_IDLE_TIMEOUT`. So a second `DeskSync`
    /// displaces the first. The guard itself stays real: the displaced
    /// connection may not write afterwards, because two writers under one
    /// device id would collide in the CRDT's per-device namespace.
    #[tokio::test]
    async fn a_newer_window_takes_the_device_and_the_displaced_one_may_not_write() {
        use rho_agent_host_proto::desk::cells::{
            CellMutation, CellWrite, DeviceId, Id, Property, Stamp, State, Uuid, Version,
        };

        let temp = tempfile::tempdir().unwrap();
        let server = test_server(temp.path()).await;
        let device = DeviceId([7; 16]);

        let (older_tx, mut older_rx) = tokio::sync::mpsc::unbounded_channel();
        let (newer_tx, _newer_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut older: Option<DeskSession> = None;
        let mut newer: Option<DeskSession> = None;

        let sync = |device| ClientFrame::Sync {
            bodies: std::collections::BTreeMap::new(),
            device,
            known: Version::default(),
            store: None,
        };
        desk_message(&server, &older_tx, 1, &mut older, sync(device))
            .await
            .expect("the first window binds the device");
        desk_message(&server, &newer_tx, 2, &mut newer, sync(device))
            .await
            .expect("and the window the user just restarted binds it too");

        // The older stream is told why it is going.
        let told = std::iter::from_fn(|| older_rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(
            matches!(told.last(), Some(ServerFrame::Displaced)),
            "the displaced window is told, and not left guessing: {told:?}"
        );

        let mutation = CellMutation {
            stamp: Stamp { device, version: 1 },
            writes: vec![CellWrite {
                id: Id::Note(Uuid([9; 16])),
                property: Property::State(State::Open),
            }],
            verdict: None,
        };
        // The daemon no longer answers a write with a refusal, so the one
        // condition that is about the connection rather than about what the
        // user wrote ends the stream, the way the text path already
        // did. Two authors in one CRDT namespace is not a thing to carry on
        // through.
        let broken = desk_message(
            &server,
            &older_tx,
            1,
            &mut older,
            ClientFrame::MutationApply {
                mutation: mutation.clone(),
            },
        )
        .await
        .expect_err("it may not write under a device id that is another window's now");
        assert_eq!(
            broken.to_string(),
            "The desk moved to a newer window on this device"
        );

        // The window that took the device writes.
        desk_message(
            &server,
            &newer_tx,
            2,
            &mut newer,
            ClientFrame::MutationApply { mutation },
        )
        .await
        .unwrap();
        // Nothing is sent back for it, so the store is where the answer is.
        assert_eq!(
            server.store.frontier().unwrap().get(&device).copied(),
            Some(1),
            "the live window's write lands"
        );
    }

    /// The stream end to end, over pipes: a sync is answered, a write on
    /// one stream pokes the other, and a newer window's sync ends the
    /// older stream with `Displaced` as its last frame.
    #[tokio::test]
    async fn a_desk_stream_syncs_pokes_and_is_displaced() {
        use rho_agent_host_proto::desk::cells::{
            CellMutation, CellWrite, DeviceId, Id, Property, Stamp, State, Uuid, Version,
        };
        use rho_rpc::parts::{read_frame, write_frame};

        let temp = tempfile::tempdir().unwrap();
        let server = test_server(temp.path()).await;
        let open = |server: &Arc<DeskServer>| {
            let (client, end) = tokio::io::duplex(1 << 20);
            let (reader, writer) = tokio::io::split(end);
            let server = Arc::clone(server);
            let task = tokio::spawn(async move { server.serve(reader, writer).await });
            (client, task)
        };
        let sync = |device| ClientFrame::Sync {
            bodies: BTreeMap::new(),
            device,
            known: Version::default(),
            store: None,
        };
        let device = DeviceId([7; 16]);

        let (mut older, older_task) = open(&server);
        write_frame(&mut older, &sync(device)).await.unwrap();
        let answer: ServerFrame = read_frame(&mut older).await.unwrap();
        assert!(matches!(answer, ServerFrame::Synced { .. }), "{answer:?}");

        let (mut other, _other_task) = open(&server);
        write_frame(&mut other, &sync(DeviceId([8; 16])))
            .await
            .unwrap();
        let _: ServerFrame = read_frame(&mut other).await.unwrap();

        let mutation = CellMutation {
            stamp: Stamp { device, version: 1 },
            writes: vec![CellWrite {
                id: Id::Note(Uuid([9; 16])),
                property: Property::State(State::Open),
            }],
            verdict: None,
        };
        write_frame(&mut older, &ClientFrame::MutationApply { mutation })
            .await
            .unwrap();
        let poke: ServerFrame = read_frame(&mut other).await.unwrap();
        assert_eq!(
            poke,
            ServerFrame::CellsAvailable {
                frontier: Version::from([(device, 1)])
            },
            "the other device hears there is something to sync"
        );

        let (mut newer, _newer_task) = open(&server);
        write_frame(&mut newer, &sync(device)).await.unwrap();
        let _: ServerFrame = read_frame(&mut newer).await.unwrap();
        let mut last = None;
        while let Ok(frame) = read_frame::<_, ServerFrame>(&mut older).await {
            last = Some(frame);
        }
        assert_eq!(
            last,
            Some(ServerFrame::Displaced),
            "the older stream is told why, and then it ends"
        );
        older_task.await.unwrap().unwrap();
    }

    /// One desk frame through the server's own handler.
    async fn desk_message(
        server: &Arc<DeskServer>,
        outgoing: &tokio::sync::mpsc::UnboundedSender<ServerFrame>,
        stream: u64,
        session: &mut Option<DeskSession>,
        frame: ClientFrame,
    ) -> anyhow::Result<()> {
        server.handle(outgoing, stream, session, frame).await
    }

    async fn test_server(root: &std::path::Path) -> Arc<DeskServer> {
        let db = RhoDb::open(root.join("rho.redb"));
        Arc::new(DeskServer::open(db).await.unwrap())
    }
}

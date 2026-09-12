//! The client's own copy of the desk cells, resumable from the version it
//! holds.
//!
//! `DeskCells` is built empty at launch and filled by the first
//! `DeskSynced`. Between those two moments the client holds a desk that
//! says nothing, and an empty desk and a desk that has not arrived are the
//! same value with opposite meanings: the first says the user has said
//! nothing, the second says nobody has asked. Readers that could not tell
//! them apart read the first meaning and were wrong — a snoozed agent was
//! dealt again, and listed as running, for as long as the first sync took.
//!
//! This is the replica that removes the state instead of guarding it at
//! each reader. The client opens from the file and asks the daemon only
//! for what came after: `DeskSync` already carries the client's `Version`
//! and the daemon already answers `Store::since`, so persisting the
//! confirmed cells and their version is the whole of the client's half.
//!
//! It holds `confirmed` and never `view`. A client that dies with
//! mutations in flight must open without them: an unacknowledged write is
//! the daemon's to accept or refuse, and a replica that remembered one
//! would show the user a verdict that was never taken.
//!
//! Like the agent mirror it is a copy, never a source. Anything doubted is
//! dropped and asked for again from the start.

use std::path::Path;
use std::sync::mpsc;

use redb::TableDefinition;
use rho_db::{RhoDb, Sen, SenValue};
use rho_ui_proto::desk_tree::cells::{
    BodySnapshot, Cell, DeviceId, Id, PropertyKey, Snapshot, Stamp, VerdictEvent, Version,
};

/// How far this client has read a host's store, by the host's name. The
/// name rather than the host id: ids are handed out in attach order and
/// mean nothing across a restart, which is the same reason the agent
/// mirror keys its cursor by name.
const DESK_HOSTS: TableDefinition<&str, Sen<StoredDeskHost>> =
    TableDefinition::new("gui_desk_host_v1");
/// This device's id in the desk store, one row. It belongs in the
/// database and not beside it because it means nothing without the
/// replica: the daemon counts this device's stamps, so a fresh replica
/// reusing the old id writes stamps the daemon has already counted, and
/// last-writer-wins drops them without a word. Delete the file and the
/// id goes with it, and this is a new device.
const DESK_DEVICE: TableDefinition<(), Sen<DeviceId>> = TableDefinition::new("gui_desk_device_v1");
/// One cell per row, at the key the store itself uses. A cell is never
/// removed: a delete is a `Deleted(true)` cell with a stamp like any
/// other, so it resumes in the same delta as everything else and cannot
/// be missed by asking only for what is new.
const DESK_CELLS: TableDefinition<Sen<CellKey>, Sen<Cell>> =
    TableDefinition::new("gui_desk_cells_v1");
/// The verdict log, keyed the way the store keys it: id and stamp, so an
/// undo and the verdict it undoes are two rows rather than one.
const DESK_VERDICTS: TableDefinition<Sen<VerdictKey>, Sen<VerdictEvent>> =
    TableDefinition::new("gui_desk_verdicts_v1");
/// A note's text, whole. The wire sends every body on every sync, so
/// there is nothing finer to store yet; per-body versions are their own
/// change and this table takes them without moving.
const DESK_BODIES: TableDefinition<Sen<BodyKey>, Sen<BodySnapshot>> =
    TableDefinition::new("gui_desk_bodies_v1");

/// What the client holds of one host, and how far it read to hold it.
/// The store's own identity belongs here too, so that a replica counting
/// in one store is dropped rather than believed when it meets another;
/// the wire does not carry it yet and the daemon's half adds both.
#[derive(Clone, Debug, senax_encoder::Encode, senax_encoder::Decode)]
struct StoredDeskHost {
    version: Version,
    /// The store the version was counted in, said by the daemon. A version
    /// is a count of writes per device inside one store, so the same
    /// numbers read against another store name writes that never happened.
    /// Held so the next handshake can say whose numbers these are, and so
    /// a daemon that does not know the name can say to throw them away.
    store: DeviceId,
    /// The text replica id the daemon gave this device. It is the
    /// device's, not the connection's, so it is the same number after a
    /// restart — and it has to be held, because the client edits a note
    /// from the replica before the daemon has answered and an edit made
    /// under a borrowed replica id is an edit attributed to someone else.
    namespace: u16,
}

#[derive(Clone, Debug, senax_encoder::Encode, senax_encoder::Decode)]
struct CellKey {
    host: String,
    id: Id,
    property: PropertyKey,
}

#[derive(Clone, Debug, senax_encoder::Encode, senax_encoder::Decode)]
struct VerdictKey {
    host: String,
    id: Id,
    stamp: Stamp,
}

#[derive(Clone, Debug, senax_encoder::Encode, senax_encoder::Decode)]
struct BodyKey {
    host: String,
    id: Id,
}

/// One host's desk as the replica holds it: what to build the store from,
/// and the bodies to give the map its buffers.
#[derive(Debug, Default)]
pub struct HeldDesk {
    /// Whether the replica holds this host at all. This is the whole
    /// point of the file and it is not the same question as whether the
    /// desk is empty: a desk the client has read and found empty says the
    /// user has said nothing, and a host it has never seen says nobody
    /// has asked. Only the second means "do not draw yet".
    pub known: bool,
    pub namespace: u16,
    pub store: DeviceId,
    pub snapshot: Snapshot,
    pub bodies: Vec<BodySnapshot>,
}

enum Write {
    Delta {
        host: String,
        store: DeviceId,
        namespace: u16,
        delta: Snapshot,
        bodies: Vec<BodySnapshot>,
    },
    /// Text this client typed. It says nothing about the cells, so the
    /// host's version vector is left alone: a local edit is not a claim
    /// to have seen anything of the store.
    Bodies {
        host: String,
        bodies: Vec<BodySnapshot>,
    },
    Reset(String),
    Flush(mpsc::SyncSender<()>),
}

pub struct DeskMirror {
    db: RhoDb,
    /// Taken on drop: closing the channel is what tells the writer thread
    /// to finish, and the file stays open until it has.
    sender: Option<mpsc::Sender<Write>>,
    writer: Option<std::thread::JoinHandle<()>>,
}

impl DeskMirror {
    /// Opens the client's database at `state_dir` and takes the replica's
    /// tables in it. For tests; the session's own database is opened once
    /// by the model thread and handed to [`DeskMirror::open_on`].
    pub fn open(state_dir: &Path) -> std::io::Result<Self> {
        Self::open_on(rho_db::client::open(state_dir)?)
    }

    /// The replica's tables in a database somebody else opened.
    pub fn open_on(db: RhoDb) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread().build()?;
        runtime.block_on(async {
            let mut write = db.write().await;
            write.open_table(DESK_HOSTS);
            write.open_table(DESK_CELLS);
            write.open_table(DESK_VERDICTS);
            write.open_table(DESK_BODIES);
            write.commit();
        });
        let (sender, receiver) = mpsc::channel();
        let writer_db = db.clone();
        // Off the UI thread: a frame must never wait on a commit.
        let handle = std::thread::Builder::new()
            .name("rho-desk-mirror".into())
            .spawn(move || writer(writer_db, receiver))?;
        Ok(Self {
            db,
            sender: Some(sender),
            writer: Some(handle),
        })
    }

    /// One host's cells, verdicts and bodies, and the version they were
    /// read through. The tables are walked and filtered by host rather
    /// than ranged: a client has one host, sometimes a few, and a walk of
    /// its own cells is what the store's own load costs anyway.
    pub fn load(&self, host: &str) -> HeldDesk {
        let read = self.db.read();
        let hosts = read.open_table(DESK_HOSTS);
        let Some(stored) = hosts.get(&host) else {
            return HeldDesk::default();
        };
        let stored = stored.value().into_owned();
        let cells = read
            .open_table(DESK_CELLS)
            .iter()
            .filter(|(key, _)| key.value().into_owned().host == host)
            .map(|(_, value)| value.value().into_owned())
            .collect::<Vec<_>>();
        let verdicts = read
            .open_table(DESK_VERDICTS)
            .iter()
            .filter_map(|(key, value)| {
                let key = key.value().into_owned();
                (key.host == host).then(|| (key.id, key.stamp, value.value().into_owned()))
            })
            .collect::<Vec<_>>();
        let bodies = read
            .open_table(DESK_BODIES)
            .iter()
            .filter(|(key, _)| key.value().into_owned().host == host)
            .map(|(_, value)| value.value().into_owned())
            .collect::<Vec<_>>();
        HeldDesk {
            known: true,
            namespace: stored.namespace,
            store: stored.store,
            snapshot: Snapshot {
                cells,
                verdicts,
                version: stored.version,
            },
            bodies,
        }
    }

    /// What the daemon just told this client, written as it was merged.
    /// The version is the store's own after the merge, not the delta's, so
    /// what is on disk and what the next `DeskSync` asks for are the same
    /// number.
    pub fn write_delta(
        &self,
        host: &str,
        store: DeviceId,
        namespace: u16,
        delta: Snapshot,
        bodies: Vec<BodySnapshot>,
    ) {
        self.send(Write::Delta {
            host: host.to_owned(),
            store,
            namespace,
            delta,
            bodies,
        });
    }

    /// Text typed on this client, kept next to what arrived. The daemon
    /// never sends a client its own operations back, so this is the only
    /// way a note written here survives a restart.
    pub fn write_bodies(&self, host: &str, bodies: Vec<BodySnapshot>) {
        self.send(Write::Bodies {
            host: host.to_owned(),
            bodies,
        });
    }

    /// Everything held for a host, dropped. For a store this replica does
    /// not count in: the cells came from somewhere else and asking for
    /// what is new would never bring them into line.
    pub fn reset_host(&self, host: &str) {
        self.send(Write::Reset(host.to_owned()));
    }

    /// Waits for everything already queued to commit.
    pub fn flush(&self) {
        let (send, receive) = mpsc::sync_channel(0);
        if let Some(sender) = self.sender.as_ref()
            && sender.send(Write::Flush(send)).is_ok()
        {
            let _ = receive.recv();
        }
    }

    fn send(&self, write: Write) {
        if self
            .sender
            .as_ref()
            .is_none_or(|sender| sender.send(write).is_err())
        {
            tracing::error!("desk mirror writer stopped");
        }
    }
}

impl Drop for DeskMirror {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn writer(db: RhoDb, receiver: mpsc::Receiver<Write>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("build desk mirror runtime");
    // Everything queued goes in one transaction: a sync arrives as several
    // deltas faster than each can commit on its own.
    while let Ok(first) = receiver.recv() {
        let mut batch = vec![first];
        while let Ok(next) = receiver.try_recv() {
            batch.push(next);
        }
        let mut flushed = Vec::new();
        runtime.block_on(async {
            let mut transaction = db.write().await;
            for write in batch {
                apply(&mut transaction, write, &mut flushed);
            }
            transaction.commit();
        });
        for done in flushed {
            let _ = done.send(());
        }
    }
}

/// Body history added to what is held, never replacing it: the daemon
/// sends only the operations this client lacks, and the client's own
/// typing arrives here the same way, one operation at a time.
fn keep_bodies(transaction: &mut rho_db::WriteTxn, host: &str, bodies: &[BodySnapshot]) {
    let mut table = transaction.open_table(DESK_BODIES);
    for body in bodies {
        let key = BodyKey {
            host: host.to_owned(),
            id: body.id.clone(),
        };
        let merged = match table.get(SenValue::borrowed(&key)) {
            Some(held) => {
                let mut held = held.value().into_owned();
                held.merge(body.clone());
                held
            }
            None => body.clone(),
        };
        table.insert(SenValue::Owned(key), SenValue::borrowed(&merged));
    }
}

fn apply(
    transaction: &mut rho_db::WriteTxn,
    write: Write,
    flushed: &mut Vec<mpsc::SyncSender<()>>,
) {
    match write {
        Write::Delta {
            host,
            store,
            namespace,
            delta,
            bodies,
        } => {
            {
                let mut cells = transaction.open_table(DESK_CELLS);
                for cell in &delta.cells {
                    let key = CellKey {
                        host: host.clone(),
                        id: cell.id.clone(),
                        property: cell.property.key(),
                    };
                    cells.insert(SenValue::Owned(key), SenValue::borrowed(cell));
                }
            }
            {
                let mut verdicts = transaction.open_table(DESK_VERDICTS);
                for (id, stamp, event) in &delta.verdicts {
                    let key = VerdictKey {
                        host: host.clone(),
                        id: id.clone(),
                        stamp: *stamp,
                    };
                    verdicts.insert(SenValue::Owned(key), SenValue::borrowed(event));
                }
            }
            keep_bodies(transaction, &host, &bodies);
            transaction.open_table(DESK_HOSTS).insert(
                &host.as_str(),
                SenValue::borrowed(&StoredDeskHost {
                    version: delta.version,
                    store,
                    namespace,
                }),
            );
        }
        Write::Bodies { host, bodies } => keep_bodies(transaction, &host, &bodies),
        Write::Reset(host) => {
            transaction.open_table(DESK_HOSTS).remove(&host.as_str());
            {
                let mut cells = transaction.open_table(DESK_CELLS);
                let held = cells
                    .iter()
                    .map(|(key, _)| key.value().into_owned())
                    .filter(|key| key.host == host)
                    .collect::<Vec<_>>();
                for key in held {
                    cells.remove(SenValue::Owned(key));
                }
            }
            {
                let mut verdicts = transaction.open_table(DESK_VERDICTS);
                let held = verdicts
                    .iter()
                    .map(|(key, _)| key.value().into_owned())
                    .filter(|key| key.host == host)
                    .collect::<Vec<_>>();
                for key in held {
                    verdicts.remove(SenValue::Owned(key));
                }
            }
            {
                let mut bodies = transaction.open_table(DESK_BODIES);
                let held = bodies
                    .iter()
                    .map(|(key, _)| key.value().into_owned())
                    .filter(|key| key.host == host)
                    .collect::<Vec<_>>();
                for key in held {
                    bodies.remove(SenValue::Owned(key));
                }
            }
        }
        Write::Flush(done) => flushed.push(done),
    }
}

/// Held behind a lock only so that `close` can take it: every reader takes
/// the lock uncontended, and the writes go down a channel anyway.
static GLOBAL: std::sync::OnceLock<std::sync::RwLock<Option<DeskMirror>>> =
    std::sync::OnceLock::new();
static CLOSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Who to tell when the replica is there to be read.
static OPENED: std::sync::Mutex<Vec<Box<dyn FnOnce() + Send>>> = std::sync::Mutex::new(Vec::new());

fn global() -> std::sync::RwLockReadGuard<'static, Option<DeskMirror>> {
    GLOBAL
        .get_or_init(|| std::sync::RwLock::new(None))
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Opens the replica in the state directory `main` named, if it named one.
/// Called from the model thread, like the agent mirror's, so that no frame
/// waits on the file being opened.
pub fn open_stated(db: Option<RhoDb>) {
    let Some(db) = db else {
        // Nothing to open, and the readers waiting on it are told so;
        // this session reads the desk from the daemon alone.
        opened();
        return;
    };
    if let Err(error) = init(db) {
        tracing::warn!(%error, "the desk replica is unavailable; this session reads the desk from the daemon");
        opened();
    }
}

/// Opens the replica for this session. Without it every call below is a
/// no-op and the GUI starts with no desk until the daemon answers, which
/// is exactly what it did before this existed — and what a test wants,
/// since a test names no state directory.
pub fn init(db: RhoDb) -> std::io::Result<()> {
    let mirror = DeskMirror::open_on(db)?;
    let mut global = GLOBAL
        .get_or_init(|| std::sync::RwLock::new(None))
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if global.is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "the desk replica is already initialized",
        ));
    }
    // A quit that beat the open closes nothing and installs nothing.
    if CLOSED.load(std::sync::atomic::Ordering::Acquire) {
        return Ok(());
    }
    *global = Some(mirror);
    drop(global);
    opened();
    Ok(())
}

/// Drains the writer and closes the file, so the next start finds it shut
/// cleanly.
pub fn close() {
    CLOSED.store(true, std::sync::atomic::Ordering::Release);
    let taken = GLOBAL
        .get_or_init(|| std::sync::RwLock::new(None))
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(mirror) = taken {
        mirror.flush();
        drop(mirror);
    }
}

/// This client's device id, minted on the first launch that had a
/// database and kept in it ever after.
///
/// The daemon binds one writer connection per device and a device's
/// stamps must keep ascending across restarts, so a fresh id every
/// launch would both lock the GUI out of a second window and lose that
/// ordering.
///
/// Read when the replica is, and from the same file: nothing waits for
/// the database here, because nothing on the desk happens before the
/// replica is loaded anyway. Until it opens there is no replica and no
/// id, and the caller gets one of this process's own — which is also
/// what a test wants, since several GUIs in one process are several
/// devices.
pub fn device() -> DeviceId {
    match global().as_ref() {
        Some(mirror) => device_in(&mirror.db),
        None => DeviceId(uuid::Uuid::new_v4().into_bytes()),
    }
}

/// The device row in a database the caller opened. A test passes its own
/// tempdir's; nothing here resolves a path.
pub fn device_in(db: &RhoDb) -> DeviceId {
    // Read and mint in the one write transaction: the table may not exist
    // yet — this can run before the mirror has opened its tables — and a
    // read of a table that was never created is a panic, while opening it
    // for writing creates it. It also makes the mint atomic, which two
    // GUIs sharing a file would need.
    //
    // Not on the mirror's writer thread either: the id has to be on disk
    // before it is handed out, or a crash between minting and writing
    // leaves the next launch a different device with these stamps behind
    // it.
    futures::executor::block_on(async {
        let mut write = db.write().await;
        let mut table = write.open_table(DESK_DEVICE);
        if let Some(device) = table.get(&()) {
            return device.value().into_owned();
        }
        let device = DeviceId(uuid::Uuid::new_v4().into_bytes());
        table.insert(&(), SenValue::borrowed(&device));
        drop(table);
        write.commit();
        device
    })
}

/// Runs `hook` when the replica is open, or now if it already is.
///
/// The file opens on the model thread, after the window exists, so a
/// reader that only looked at startup found nothing and drew a desk the
/// user's verdicts were missing from until a daemon answered. This is
/// how it is told to look again, and it costs no wait: the frame that
/// asked goes on drawing.
pub fn on_open(hook: impl FnOnce() + Send + 'static) {
    // A session told of no state directory has no replica and never will,
    // which is settled news rather than news that has not arrived: a test,
    // and a client that could not be told where its files live, must not
    // sit waiting for a file that is not coming.
    if global().is_some() || crate::mirror::state_dir().is_none() {
        hook();
        return;
    }
    OPENED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(Box::new(hook));
}

fn opened() {
    let hooks = std::mem::take(
        &mut *OPENED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    for hook in hooks {
        hook();
    }
}

pub fn load(host: &str) -> HeldDesk {
    global()
        .as_ref()
        .map(|mirror| mirror.load(host))
        .unwrap_or_default()
}

pub fn write_delta(
    host: &str,
    store: DeviceId,
    namespace: u16,
    delta: Snapshot,
    bodies: Vec<BodySnapshot>,
) {
    match global().as_ref() {
        Some(mirror) => mirror.write_delta(host, store, namespace, delta, bodies),
        // Not an error — a session told of no state directory keeps
        // nothing — but it is the answer to "why did this client resume
        // from nothing", so it is said once per delta rather than never.
        None => tracing::debug!(host, "no desk replica is open; this delta is not kept"),
    }
}

/// Text typed on this client, kept in the replica.
pub fn write_bodies(host: &str, bodies: Vec<BodySnapshot>) {
    match global().as_ref() {
        Some(mirror) => mirror.write_bodies(host, bodies),
        None => tracing::debug!(host, "no desk replica is open; this text is not kept"),
    }
}

pub fn reset_host(host: &str) {
    if let Some(mirror) = global().as_ref() {
        mirror.reset_host(host);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rho_ui_proto::desk_tree::cells::{Property, Store, Uuid};

    use super::*;

    /// The store the daemon in these tests answers as.
    const DAEMON: DeviceId = DeviceId([5; 16]);

    fn note(seed: u8) -> Id {
        Id::Note(Uuid([seed; 16]))
    }

    /// Who asked to be told is told when the file opens, and asking
    /// after it has opened is answered at once. The replica opens on the
    /// model thread, well after the window is up, so a reader that only
    /// looked at startup drew a desk without the user's verdicts in it
    /// until a daemon answered.
    ///
    /// This is the only test that installs the global replica, because
    /// installing it is once per process.
    #[test]
    fn a_reader_is_told_when_the_replica_opens_and_told_at_once_if_it_already_has() {
        let dir = tempfile::tempdir().unwrap();
        // A session told of no state directory is told at once instead:
        // there is no file coming, and a reader waiting for one would
        // never draw. So this test names one, its own.
        crate::mirror::set_state_dir(dir.path().to_owned());
        let db = rho_db::client::open(dir.path()).unwrap();
        let before = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&before);
        on_open(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(
            before.load(Ordering::SeqCst),
            0,
            "nothing is open yet, so nobody has been told"
        );

        init(db).expect("the replica installs once");
        assert_eq!(
            before.load(Ordering::SeqCst),
            1,
            "the open tells everyone who asked"
        );

        let after = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&after);
        on_open(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(
            after.load(Ordering::SeqCst),
            1,
            "asking after the open is answered where it stands"
        );
        close();
    }

    /// The replica builds a note's history up out of the pieces it is
    /// sent. The daemon answers a sync with only the operations the
    /// client lacks, so a body that arrived as a delta must not replace
    /// the one on disk, or the words the client already had are lost the
    /// moment somebody types the next one.
    #[test]
    fn a_body_delta_is_added_to_what_is_held_rather_than_replacing_it() {
        let dir = tempfile::tempdir().unwrap();
        let mirror = DeskMirror::open(dir.path()).unwrap();
        let store = Store::new(DeviceId([7; 16]));
        let id = note(1);
        let first = BodySnapshot {
            id: id.clone(),
            operations: vec![edit(1, 1)],
            transactions: Vec::new(),
        };
        let second = BodySnapshot {
            id: id.clone(),
            operations: vec![edit(1, 2)],
            transactions: Vec::new(),
        };

        mirror.write_delta("desk", DAEMON, 1, store.snapshot(), vec![first]);
        mirror.write_delta("desk", DAEMON, 1, store.snapshot(), vec![second]);
        mirror.flush();

        let held = mirror.load("desk");
        let body = held
            .bodies
            .iter()
            .find(|body| body.id == id)
            .expect("the note's history is held");
        assert_eq!(
            body.version(),
            rho_ui_proto::desk_tree::cells::BodyVersion::from([(1, 2)]),
            "both operations are there, the first one not thrown away by the second delta"
        );
    }

    /// Text typed on this client is kept, and keeping it says nothing
    /// about the cells: the version vector the next sync asks with is the
    /// one the daemon's deltas set, not an empty one left by local typing.
    #[test]
    fn text_typed_here_is_kept_and_leaves_the_cell_version_where_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let mirror = DeskMirror::open(dir.path()).unwrap();
        let mut store = Store::new(DeviceId([7; 16]));
        store
            .write(note(2), Property::Name("release notes".into()))
            .unwrap();
        let id = note(1);

        mirror.write_delta("desk", DAEMON, 1, store.snapshot(), Vec::new());
        mirror.write_bodies(
            "desk",
            vec![BodySnapshot {
                id: id.clone(),
                operations: vec![edit(1, 1)],
                transactions: Vec::new(),
            }],
        );
        mirror.flush();

        let held = mirror.load("desk");
        assert!(
            held.bodies.iter().any(|body| body.id == id),
            "the note typed here is held"
        );
        assert_eq!(
            held.snapshot.version,
            store.snapshot().version,
            "and the version vector is still what the daemon's delta left"
        );
    }

    fn edit(replica_id: u16, value: u32) -> rho_ui_proto::desk_tree::TextOperation {
        rho_ui_proto::desk_tree::TextOperation::Edit {
            timestamp: rho_ui_proto::desk_tree::TreeClock { value, replica_id },
            version: Vec::new(),
            ranges: vec![(0, 0)],
            new_text: vec!["x".into()],
        }
    }

    /// The device id is minted once and kept, so the stamps this device
    /// writes go on ascending across a restart. A second database is a
    /// second device: the id lives with the replica whose writes it
    /// counts, and a fresh replica reusing an old id would write stamps
    /// the daemon has already counted and lose them silently.
    #[test]
    fn the_device_id_is_minted_once_per_database_and_read_back_from_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::client::open(dir.path()).unwrap();
        let first = device_in(&db);
        assert_eq!(first, device_in(&db), "the same database, the same device");
        drop(db);

        let other = tempfile::tempdir().unwrap();
        let db = rho_db::client::open(other.path()).unwrap();
        assert_ne!(
            first,
            device_in(&db),
            "another database has never written a stamp, so it is another device"
        );
    }

    /// What the replica is for: a client that has written a host's cells
    /// down opens holding them, and the version it opens with is the one
    /// the daemon is asked to resume from.
    #[test]
    fn a_written_desk_is_read_back_whole_with_the_version_it_was_read_through() {
        let dir = tempfile::tempdir().unwrap();
        let mirror = DeskMirror::open(dir.path()).unwrap();
        let mut store = Store::new(DeviceId([7; 16]));
        let id = note(1);
        store
            .write(id.clone(), Property::Name("rho".into()))
            .unwrap();
        store.write(id.clone(), Property::PaceDays(3)).unwrap();
        let snapshot = store.snapshot();

        mirror.write_delta("desk", DAEMON, 42, snapshot.clone(), Vec::new());
        mirror.flush();

        let held = mirror.load("desk");
        assert!(held.known, "the host was written, so it is known");
        assert_eq!(
            held.namespace, 42,
            "the device's replica id is held, so an edit before the daemon answers is this device's"
        );
        assert_eq!(
            held.snapshot.version, snapshot.version,
            "the version is what the next DeskSync asks from"
        );
        assert_eq!(
            held.store, DAEMON,
            "the store the version was counted in is held with it, since the number means nothing without it"
        );
        let read = Store::from_snapshot(DeviceId([7; 16]), held.snapshot).unwrap();
        let facts = read.facts(&id);
        assert_eq!(facts.name.as_deref(), Some("rho"));
        assert_eq!(facts.pace_days, 3);
    }

    /// A host the replica has never held is not an empty desk: the two
    /// mean opposite things, and only the second says the user has said
    /// nothing.
    #[test]
    fn a_host_never_written_is_not_the_same_as_an_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        let mirror = DeskMirror::open(dir.path()).unwrap();
        assert!(!mirror.load("desk").known);

        mirror.write_delta(
            "desk",
            DAEMON,
            42,
            Store::new(DeviceId([7; 16])).snapshot(),
            Vec::new(),
        );
        mirror.flush();
        assert!(
            mirror.load("desk").known,
            "a desk read and found empty is still a desk this client has read"
        );
    }

    /// A second delta adds to the first rather than replacing it, and
    /// moves the version on: that is what makes the ask a resume.
    #[test]
    fn a_later_delta_adds_to_what_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let mirror = DeskMirror::open(dir.path()).unwrap();
        let device = DeviceId([7; 16]);
        let mut store = Store::new(device);
        let first = note(1);
        store
            .write(first.clone(), Property::Name("rho".into()))
            .unwrap();
        let held_through = store.version().clone();
        mirror.write_delta("desk", DAEMON, 42, store.snapshot(), Vec::new());

        let second = note(2);
        store
            .write(second.clone(), Property::Name("slack".into()))
            .unwrap();
        mirror.write_delta("desk", DAEMON, 42, store.since(&held_through), Vec::new());
        mirror.flush();

        let held = mirror.load("desk");
        assert_ne!(held.snapshot.version, held_through, "the version moved");
        let read = Store::from_snapshot(device, held.snapshot).unwrap();
        assert_eq!(read.facts(&first).name.as_deref(), Some("rho"));
        assert_eq!(read.facts(&second).name.as_deref(), Some("slack"));
    }

    /// A store this replica does not count in: everything held for the
    /// host goes, so the next sync asks from nothing.
    #[test]
    fn a_reset_host_is_forgotten_whole() {
        let dir = tempfile::tempdir().unwrap();
        let mirror = DeskMirror::open(dir.path()).unwrap();
        let mut store = Store::new(DeviceId([7; 16]));
        store.write(note(1), Property::Name("rho".into())).unwrap();
        mirror.write_delta("desk", DAEMON, 42, store.snapshot(), Vec::new());
        mirror.flush();
        assert!(mirror.load("desk").known);

        mirror.reset_host("desk");
        mirror.flush();
        assert!(
            !mirror.load("desk").known,
            "a dropped replica is a host nobody has read"
        );
    }
}

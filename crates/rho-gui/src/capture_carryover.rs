//! One shot, on the first run of the build that deleted the capture store:
//! everything the user had written there becomes a note at the root.
//!
//! Machine rows are not carried. A Slack thread re-derives itself from the
//! mirror the moment it matters again, and a page is a node already; the
//! only thing here that cannot be rebuilt is text a person typed.
//!
//! Everything the carry-over needs lives in this file: the frozen decoder
//! for the old rows, the marker that says it has run, and the writes. It
//! cannot be removed in the change that ships it (the user must run that
//! build once), so it is written to delete cleanly: drop this file, its
//! `mod` line, its call in `handle_event`, and the journal event.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{Context, Window};
use redb::TableDefinition;
use rho_db::{RhoDb, Sen};
use senax_encoder::{Decode, Decoder as _, Encode};

use crate::registry::HostId;
use crate::workspace::Workspace;

/// The old store's table, named as it was written. redb records the type
/// names a table was created with, so they are part of the format and the
/// reader has to answer to them.
const ITEMS: &str = "rho_gui_inbox_items_v2";
/// Written beside the rows once they have been carried, so a second run of
/// the same build does not make a second copy of every note.
const CARRIED: TableDefinition<Sen<String>, Sen<bool>> =
    TableDefinition::new("rho_gui_capture_carryover_v1");
const CARRIED_KEY: &str = "carried";

/// One process carries at most once, however many hosts sync.
static ATTEMPTED: AtomicBool = AtomicBool::new(false);

/// The kinds the old store held. Only `Capture` is the user's own writing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
enum InboxKind {
    Ping,
    Capture,
    Obligation,
    Slack,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
struct NodeIdentity {
    replica_id: u16,
    counter: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
enum SourceReference {
    Page {
        id: String,
    },
    SlackThread {
        workspace: String,
        channel: String,
        thread_ts: String,
        latest_ts: String,
    },
    DeskNode {
        host: u32,
        node_id: NodeIdentity,
    },
    External {
        source: String,
        reference: String,
    },
    None,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
struct CapturedContext {
    host: Option<String>,
    room: Option<String>,
    focused_surface: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct InboxId(String);

/// The row as it was stored. Field order is the wire order: this is a
/// decoder for bytes already on disk, not a type anything writes.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct InboxItem {
    id: InboxId,
    kind: InboxKind,
    text: String,
    source: SourceReference,
    context: CapturedContext,
    captured_at_ms: i64,
    #[senax(default)]
    deferred_until_ms: Option<i64>,
    #[senax(default)]
    resurfacing_count: u32,
    #[senax(default)]
    waiting_on: Option<String>,
}

/// Raw bytes under the old table's type name, so a row that no longer
/// decodes can be counted instead of taking the whole carry-over down.
#[derive(Debug)]
struct StoredBytes;

impl redb::Value for StoredBytes {
    type SelfType<'a> = &'a [u8];
    type AsBytes<'a> = &'a [u8];

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> &'a [u8]
    where
        Self: 'a,
    {
        data
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a &'b [u8]) -> &'a [u8]
    where
        Self: 'b,
    {
        value
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new("rho-db::Sen<rho_gui::inbox::InboxItem>")
    }
}

#[derive(Debug)]
struct StoredKey;

impl redb::Value for StoredKey {
    type SelfType<'a> = &'a [u8];
    type AsBytes<'a> = &'a [u8];

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> &'a [u8]
    where
        Self: 'a,
    {
        data
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a &'b [u8]) -> &'a [u8]
    where
        Self: 'b,
    {
        value
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new("rho-db::Sen<alloc::string::String>")
    }
}

impl redb::Key for StoredKey {
    fn compare(left: &[u8], right: &[u8]) -> std::cmp::Ordering {
        left.cmp(right)
    }
}

const STORED: TableDefinition<StoredKey, StoredBytes> = TableDefinition::new(ITEMS);

fn store_path() -> Option<PathBuf> {
    Some(dirs::state_dir()?.join("rho/inbox.redb"))
}

/// The text of every capture, oldest first, and how many rows could not be
/// read at all.
fn captures(db: &RhoDb) -> (Vec<String>, u32) {
    let read = db.read();
    if !read.has_table(ITEMS) {
        return (Vec::new(), 0);
    }
    let table = read.open_table(STORED);
    let mut unreadable = 0;
    let mut rows = Vec::new();
    for (_, value) in table.iter() {
        let mut bytes = value.value();
        match InboxItem::decode(&mut bytes) {
            Ok(item) => rows.push(item),
            Err(_) => unreadable += 1,
        }
    }
    rows.sort_by_key(|item| item.captured_at_ms);
    let texts = rows
        .into_iter()
        .filter(|item| item.kind == InboxKind::Capture)
        .map(|item| item.text)
        .filter(|text| !text.trim().is_empty())
        .collect();
    (texts, unreadable)
}

fn already_carried(db: &RhoDb) -> bool {
    let read = db.read();
    if !read.has_table("rho_gui_capture_carryover_v1") {
        return false;
    }
    read.open_table(CARRIED)
        .get(rho_db::SenValue::owned(CARRIED_KEY.to_owned()))
        .is_some_and(|carried| *carried.value().as_ref())
}

fn mark_carried(db: &RhoDb) {
    futures::executor::block_on(async {
        let mut write = db.write().await;
        write.open_table(CARRIED).insert(
            rho_db::SenValue::owned(CARRIED_KEY.to_owned()),
            rho_db::SenValue::owned(true),
        );
        write.commit();
    });
}

impl Workspace {
    /// Runs the carry-over once the tree is there to write into. The marker
    /// is set only after the notes are queued, so a failure here leaves the
    /// text where it is for the next run.
    pub(crate) fn carry_over_captures(
        &mut self,
        host: HostId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ATTEMPTED.swap(true, Ordering::Relaxed) {
            return;
        }
        let Some(path) = store_path().filter(|path| path.exists()) else {
            return;
        };
        let db = RhoDb::open(path);
        if already_carried(&db) {
            return;
        }
        let (texts, unreadable) = captures(&db);
        if texts.is_empty() && unreadable == 0 {
            mark_carried(&db);
            return;
        }
        let mut writes = Vec::new();
        let mut bodies = Vec::new();
        for text in texts {
            let Some((node_id, note)) = self.desk_cells.create_note_writes(host, None) else {
                break;
            };
            writes.extend(note);
            bodies.push((node_id, text));
        }
        let notes = bodies.len() as u32;
        if !writes.is_empty() {
            let Some(stamp) = self.apply_desk_writes(host, writes, None, window, cx) else {
                return;
            };
            self.pending_desk_texts.insert((host, stamp), bodies);
            self.sync_tree_dashboard(host, window, cx);
        }
        mark_carried(&db);
        crate::journal::record(crate::journal::Event::CaptureCarryover { notes, unreadable });
    }
}

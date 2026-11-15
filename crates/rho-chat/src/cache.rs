//! The client's own copy of every host's chat log, so a client comes up
//! knowing the chat and asks a host only for what came after. Beside it,
//! what the user wrote that no host has taken ([`crate::outbox`]) and what
//! they are writing in each thread.
//!
//! Writes go to a thread of their own, batched into one transaction each:
//! a frame never waits on a commit. Everything in the log tables is a copy
//! the host still has, so a table whose shape changed is dropped and read
//! again from the host.

use std::sync::mpsc;

use redb::{TableDefinition, TableHandle as _};
use rho_agent_types::{ChatRecord, ChatSeq, MessageId};
use rho_db::{RhoDb, Sen, SenValue};

use crate::outbox::{Kept, Outgoing};

/// Every record, by the host's machine seed and position.
const LOG: TableDefinition<(u64, u64), Sen<ChatRecord>> = TableDefinition::new("chat2_log");
/// Where this client stands in each host's log.
const CURSOR: TableDefinition<u64, u64> = TableDefinition::new("chat2_cursor");
/// What the user wrote that no host has taken yet, by host and client id.
const OUTBOX: TableDefinition<(u64, u64), Sen<Outgoing>> = TableDefinition::new("chat2_outbox");
/// What the user is writing under each parent and has not sent, by host
/// and the parent's id (zero for a root). Sending moves it to the outbox.
const DRAFTS: TableDefinition<(u64, u64), &str> = TableDefinition::new("chat2_draft");

/// The tables the topic-shaped client kept the user's own words in. Every
/// other topic table was a copy and is dropped by `rho-agents-client`;
/// these two are converted on the first open and dropped then.
///
/// A topic's id is its first message's id, and the host's migration kept
/// every seq, so a topic is now the root message under the same number:
/// a draft or an unsent message "in topic T" is one under parent `T`.
mod legacy {
    use rho_agent_types::{AgentId, ChatSeq, ContentPart, MessageId, UnixMs};
    use rho_db::Sen;
    use senax_encoder::{Decode, Encode};

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
    pub struct StreamId(pub ChatSeq);

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
    pub struct TopicId(pub ChatSeq);

    pub const OUTBOX: redb::TableDefinition<(u64, u64), Sen<Outgoing>> =
        redb::TableDefinition::new("gui_chat_outbox_v1");
    pub const DRAFTS: redb::TableDefinition<(u64, u64), &str> =
        redb::TableDefinition::new("gui_topic_draft_v1");

    /// Named as the old crate named them, so redb opens the tables.
    #[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
    pub enum Destination {
        Topic(TopicId),
        NewTopic { stream: StreamId, name: String },
        AfterStart(u64),
    }

    #[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
    pub struct Outgoing {
        pub at: UnixMs,
        pub content: Vec<ContentPart>,
        pub mentions: Vec<AgentId>,
        pub to: Destination,
    }

    impl Outgoing {
        /// Where it goes now. A message that was to follow another into a
        /// topic the host had not made yet has nothing to follow: it is
        /// posted as a root, which is where the topic would have been.
        pub fn convert(self) -> super::Outgoing {
            let parent = match self.to {
                Destination::Topic(topic) => Some(MessageId(ChatSeq(topic.0.0))),
                Destination::NewTopic { .. } | Destination::AfterStart(_) => None,
            };
            super::Outgoing {
                at: self.at,
                parent,
                mentions: self.mentions,
                content: self.content,
            }
        }
    }
}

/// Moves the user's words out of the topic-shaped tables, once.
fn convert_legacy(write: &mut rho_db::WriteTxn) {
    let unsent: Vec<((u64, u64), legacy::Outgoing)> = write
        .try_open_table(legacy::OUTBOX)
        .map(|old| {
            old.iter()
                .map(|(key, value)| (key.value(), value.value().into_owned()))
                .collect()
        })
        .unwrap_or_default();
    if !unsent.is_empty() {
        let mut outbox = write.open_table(OUTBOX);
        for (key, outgoing) in unsent {
            outbox.insert(key, SenValue::borrowed(&outgoing.convert()));
        }
    }
    write.delete_table(legacy::OUTBOX.name());
    let drafts: Vec<((u64, u64), String)> = write
        .try_open_table(legacy::DRAFTS)
        .map(|old| {
            old.iter()
                .map(|(key, value)| (key.value(), value.value().to_owned()))
                .collect()
        })
        .unwrap_or_default();
    if !drafts.is_empty() {
        let mut table = write.open_table(DRAFTS);
        for (key, text) in drafts {
            table.insert(key, text.as_str());
        }
    }
    write.delete_table(legacy::DRAFTS.name());
}

/// What the copy holds when a client starts.
#[derive(Default)]
pub struct Loaded {
    /// Each host's cursor.
    pub cursors: Vec<(u64, ChatSeq)>,
    pub outbox: Vec<(u64, u64, Outgoing)>,
    pub drafts: Vec<(u64, Option<MessageId>, String)>,
}

enum Write {
    /// A run of a host's log, and the cursor it moves to. One transaction,
    /// so the cursor never claims records that are not there.
    Records {
        seed: u64,
        records: Vec<ChatRecord>,
    },
    /// A host's log is not the one the copy holds: the copy starts over.
    Reset(u64),
    Outbox(u64, Kept),
    Draft(u64, Option<MessageId>, String),
    Flush(mpsc::SyncSender<()>),
}

/// Opens a copied table, dropping it first if the file records it under
/// other key/value types: redb writes the Rust path of a value type into
/// the table, and everything in these is a copy the host still has.
fn open_or_rebuild<K, V>(write: &mut rho_db::WriteTxn, definition: TableDefinition<K, V>)
where
    K: redb::Key + 'static,
    V: redb::Value + 'static,
{
    if write.try_open_table(definition).is_none() {
        write.delete_table(definition.name());
        write.open_table(definition);
    }
}

pub struct Cache {
    db: RhoDb,
    /// Taken on drop: closing the channel is what tells the writer thread
    /// to finish, and the database file stays open until it has.
    sender: Option<mpsc::Sender<Write>>,
    writer: Option<std::thread::JoinHandle<()>>,
}

impl Cache {
    /// The copy's tables in a database somebody else opened. Every other
    /// kind of client state is in there too, under its own names; this
    /// touches the copy's and nothing else.
    pub fn open_on(db: RhoDb) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread().build()?;
        runtime.block_on(async {
            let mut write = db.write().await;
            open_or_rebuild(&mut write, LOG);
            open_or_rebuild(&mut write, CURSOR);
            write.open_table(OUTBOX);
            write.open_table(DRAFTS);
            convert_legacy(&mut write);
            write.commit();
        });
        let (sender, receiver) = mpsc::channel();
        let writer_db = db.clone();
        let handle = std::thread::Builder::new()
            .name("rho-chat-cache".into())
            .spawn(move || writer(writer_db, receiver))?;
        Ok(Self {
            db,
            sender: Some(sender),
            writer: Some(handle),
        })
    }

    pub fn load(&self) -> Loaded {
        let read = self.db.read();
        Loaded {
            cursors: read
                .open_table(CURSOR)
                .iter()
                .map(|(seed, cursor)| (seed.value(), ChatSeq(cursor.value())))
                .collect(),
            outbox: read
                .open_table(OUTBOX)
                .iter()
                .map(|(key, value)| {
                    let (seed, id) = key.value();
                    (seed, id, value.value().into_owned())
                })
                .collect(),
            drafts: read
                .open_table(DRAFTS)
                .iter()
                .map(|(key, value)| {
                    let (seed, parent) = key.value();
                    (seed, parent_of(parent), value.value().to_owned())
                })
                .collect(),
        }
    }

    /// A host's whole log in order.
    pub fn read_log(&self, seed: u64) -> Vec<ChatRecord> {
        self.db
            .read()
            .open_table(LOG)
            .range((seed, 0)..=(seed, u64::MAX))
            .map(|(_, value)| value.value().into_owned())
            .collect()
    }

    pub fn write_records(&self, seed: u64, records: Vec<ChatRecord>) {
        self.send(Write::Records { seed, records });
    }

    pub fn reset(&self, seed: u64) {
        self.send(Write::Reset(seed));
    }

    pub fn write_outbox(&self, seed: u64, kept: Kept) {
        self.send(Write::Outbox(seed, kept));
    }

    /// A draft as it stands; empty drops it.
    pub fn write_draft(&self, seed: u64, parent: Option<MessageId>, text: String) {
        self.send(Write::Draft(seed, parent, text));
    }

    /// Waits until everything sent so far is committed.
    pub fn flush(&self) {
        let (done, wait) = mpsc::sync_channel(1);
        self.send(Write::Flush(done));
        let _ = wait.recv();
    }

    fn send(&self, write: Write) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(write);
        }
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn parent_key(parent: Option<MessageId>) -> u64 {
    parent.map_or(0, |parent| parent.0.0)
}

fn parent_of(key: u64) -> Option<MessageId> {
    (key != 0).then_some(MessageId(ChatSeq(key)))
}

fn writer(db: RhoDb, receiver: mpsc::Receiver<Write>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("build chat cache runtime");
    // Everything queued goes in one transaction: a catch-up arrives as
    // many pages faster than each can commit on its own.
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

fn apply(
    transaction: &mut rho_db::WriteTxn,
    write: Write,
    flushed: &mut Vec<mpsc::SyncSender<()>>,
) {
    match write {
        Write::Records { seed, records } => {
            let Some(last) = records.last() else {
                return;
            };
            let through = last.seq.0;
            let mut log = transaction.open_table(LOG);
            for record in &records {
                log.insert((seed, record.seq.0), SenValue::borrowed(record));
            }
            drop(log);
            let mut cursor = transaction.open_table(CURSOR);
            let held = cursor.get(seed).map_or(0, |held| held.value());
            cursor.insert(seed, held.max(through));
        }
        Write::Reset(seed) => {
            let mut log = transaction.open_table(LOG);
            let held: Vec<u64> = log
                .range((seed, 0)..=(seed, u64::MAX))
                .map(|(key, _)| key.value().1)
                .collect();
            for seq in held {
                log.remove((seed, seq));
            }
            drop(log);
            transaction.open_table(CURSOR).remove(seed);
        }
        Write::Outbox(seed, Kept::Put(id, outgoing)) => {
            transaction
                .open_table(OUTBOX)
                .insert((seed, id), SenValue::borrowed(&outgoing));
        }
        Write::Outbox(seed, Kept::Gone(id)) => {
            transaction.open_table(OUTBOX).remove((seed, id));
        }
        Write::Draft(seed, parent, text) => {
            let mut drafts = transaction.open_table(DRAFTS);
            let key = (seed, parent_key(parent));
            if text.is_empty() {
                drafts.remove(key);
            } else {
                drafts.insert(key, text.as_str());
            }
        }
        Write::Flush(done) => flushed.push(done),
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{Address, ChatEvent, ContentPart, UnixMs};

    use super::*;

    fn record(seq: u64) -> ChatRecord {
        ChatRecord {
            seq: ChatSeq(seq),
            at: UnixMs(seq),
            event: ChatEvent::Posted {
                parent: None,
                from: Address::User,
                mentions: Vec::new(),
                content: vec![ContentPart::Text {
                    text: format!("m{seq}"),
                }],
            },
        }
    }

    /// The log, cursor, outbox and drafts come back as written; a reset
    /// drops one host's log and cursor and leaves the other's.
    #[test]
    fn the_copy_reads_back_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::client::open(dir.path()).unwrap();
        let cache = Cache::open_on(db.clone()).unwrap();
        cache.write_records(1, vec![record(1), record(2)]);
        cache.write_records(2, vec![record(1)]);
        cache.write_records(1, vec![record(3)]);
        let outgoing = Outgoing {
            at: UnixMs(5),
            parent: Some(MessageId(ChatSeq(2))),
            mentions: Vec::new(),
            content: vec![ContentPart::Text { text: "hi".into() }],
        };
        cache.write_outbox(1, Kept::Put(7, outgoing.clone()));
        cache.write_outbox(1, Kept::Put(8, outgoing.clone()));
        cache.write_outbox(1, Kept::Gone(8));
        cache.write_draft(1, None, "a root".into());
        cache.write_draft(1, Some(MessageId(ChatSeq(2))), "under two".into());
        cache.write_draft(1, Some(MessageId(ChatSeq(2))), String::new());
        cache.flush();

        let loaded = cache.load();
        let mut cursors = loaded.cursors.clone();
        cursors.sort();
        assert_eq!(cursors, [(1, ChatSeq(3)), (2, ChatSeq(1))]);
        assert_eq!(loaded.outbox, [(1, 7, outgoing)]);
        assert_eq!(loaded.drafts, [(1, None, "a root".to_owned())]);
        assert_eq!(
            cache
                .read_log(1)
                .iter()
                .map(|record| record.seq.0)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );

        cache.reset(1);
        cache.flush();
        assert!(cache.read_log(1).is_empty());
        assert_eq!(cache.load().cursors, [(2, ChatSeq(1))]);
        assert_eq!(cache.read_log(2).len(), 1);
        drop(cache);
        // Opened again on the same file: the tables are the same shape, so
        // nothing is dropped.
        let cache = Cache::open_on(db).unwrap();
        assert_eq!(cache.read_log(2).len(), 1);
    }

    /// The topic client's unsent messages and drafts come across: a topic
    /// becomes the root message under its number, a topic yet to start
    /// becomes a root post, and the old tables are gone afterwards.
    #[test]
    fn the_topic_clients_words_are_converted_once() {
        use legacy::{StreamId, TopicId};
        use rho_agent_types::AgentId;
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::client::open(dir.path()).unwrap();
        let text = |text: &str| vec![ContentPart::Text { text: text.into() }];
        let agent = AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async {
                let mut write = db.write().await;
                let mut outbox = write.open_table(legacy::OUTBOX);
                outbox.insert(
                    (1, 10),
                    SenValue::borrowed(&legacy::Outgoing {
                        at: UnixMs(1),
                        content: text("in topic 4"),
                        mentions: vec![agent],
                        to: legacy::Destination::Topic(TopicId(ChatSeq(4))),
                    }),
                );
                outbox.insert(
                    (1, 11),
                    SenValue::borrowed(&legacy::Outgoing {
                        at: UnixMs(2),
                        content: text("a new topic"),
                        mentions: Vec::new(),
                        to: legacy::Destination::NewTopic {
                            stream: StreamId(ChatSeq(1)),
                            name: "x".into(),
                        },
                    }),
                );
                drop(outbox);
                write
                    .open_table(legacy::DRAFTS)
                    .insert((1, 4), "half a reply");
                write.commit();
            });

        let cache = Cache::open_on(db.clone()).unwrap();
        let loaded = cache.load();
        assert_eq!(
            loaded.outbox,
            [
                (
                    1,
                    10,
                    Outgoing {
                        at: UnixMs(1),
                        parent: Some(MessageId(ChatSeq(4))),
                        mentions: vec![agent],
                        content: text("in topic 4"),
                    }
                ),
                (
                    1,
                    11,
                    Outgoing {
                        at: UnixMs(2),
                        parent: None,
                        mentions: Vec::new(),
                        content: text("a new topic"),
                    }
                ),
            ]
        );
        assert_eq!(
            loaded.drafts,
            [(1, Some(MessageId(ChatSeq(4))), "half a reply".to_owned())]
        );
        drop(cache);
        let read = db.read();
        assert!(!read.has_table(legacy::OUTBOX.name()));
        assert!(!read.has_table(legacy::DRAFTS.name()));
    }
}

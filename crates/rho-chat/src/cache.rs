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
use rho_agent_types::{ChatRecord, ChatSeq, EndpointId, MessageId};
use rho_db::{RhoDb, Sen, SenValue};

use crate::outbox::{Kept, Outgoing};

/// A host in a key: its iroh identity's bytes.
type Host = [u8; 32];

/// Every record, by the host and position.
const LOG: TableDefinition<(Host, u64), Sen<ChatRecord>> = TableDefinition::new("chat3_log");
/// Where this client stands in each host's log.
const CURSOR: TableDefinition<Host, u64> = TableDefinition::new("chat3_cursor");
/// What the user wrote that no host has taken yet, by host and client id.
const OUTBOX: TableDefinition<(Host, u64), Sen<Outgoing>> = TableDefinition::new("chat3_outbox");
/// What the user is writing under each parent and has not sent, by host
/// and the parent's id (zero for a root). Sending moves it to the outbox.
const DRAFTS: TableDefinition<(Host, u64), &str> = TableDefinition::new("chat3_draft");

/// The same tables while a host's database seed named its log
/// (`temp-migration`). A host's rows move to the tables above on its first
/// hello, which says both names; the log and cursor are a copy and are
/// dropped instead, the session having asked for the log from the start.
mod seeded {
    use super::*;

    pub const LOG: TableDefinition<(u64, u64), Sen<ChatRecord>> = TableDefinition::new("chat2_log");
    pub const CURSOR: TableDefinition<u64, u64> = TableDefinition::new("chat2_cursor");
    pub const OUTBOX: TableDefinition<(u64, u64), Sen<Outgoing>> =
        TableDefinition::new("chat2_outbox");
    pub const DRAFTS: TableDefinition<(u64, u64), &str> = TableDefinition::new("chat2_draft");
}

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
        let mut outbox = write.open_table(seeded::OUTBOX);
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
        let mut table = write.open_table(seeded::DRAFTS);
        for (key, text) in drafts {
            table.insert(key, text.as_str());
        }
    }
    write.delete_table(legacy::DRAFTS.name());
}

/// What the copy holds when a client starts.
#[derive(Debug, Default, PartialEq)]
pub struct Loaded {
    /// Each host's cursor.
    pub cursors: Vec<(EndpointId, ChatSeq)>,
    pub outbox: Vec<(EndpointId, u64, Outgoing)>,
    pub drafts: Vec<(EndpointId, Option<MessageId>, String)>,
}

enum Write {
    /// A run of a host's log, and the cursor it moves to. One transaction,
    /// so the cursor never claims records that are not there.
    Records {
        host: Host,
        records: Vec<ChatRecord>,
    },
    /// A host's log is not the one the copy holds: the copy starts over.
    Reset(Host),
    Outbox(Host, Kept),
    Draft(Host, Option<MessageId>, String),
    /// The host's rows under its seed move to its endpoint
    /// (`temp-migration`).
    Adopt {
        machine_seed: u64,
        host: Host,
    },
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
            open_or_rebuild(&mut write, seeded::LOG);
            open_or_rebuild(&mut write, seeded::CURSOR);
            write.open_table(seeded::OUTBOX);
            write.open_table(seeded::DRAFTS);
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
                .map(|(host, cursor)| (EndpointId(host.value()), ChatSeq(cursor.value())))
                .collect(),
            outbox: read
                .open_table(OUTBOX)
                .iter()
                .map(|(key, value)| {
                    let (host, id) = key.value();
                    (EndpointId(host), id, value.value().into_owned())
                })
                .collect(),
            drafts: read
                .open_table(DRAFTS)
                .iter()
                .map(|(key, value)| {
                    let (host, parent) = key.value();
                    (
                        EndpointId(host),
                        parent_of(parent),
                        value.value().to_owned(),
                    )
                })
                .collect(),
        }
    }

    /// A host's whole log in order.
    pub fn read_log(&self, host: EndpointId) -> Vec<ChatRecord> {
        self.db
            .read()
            .open_table(LOG)
            .range((host.0, 0)..=(host.0, u64::MAX))
            .map(|(_, value)| value.value().into_owned())
            .collect()
    }

    /// The user's own words for one host: what waits unsent, by client
    /// id, and the drafts by parent.
    #[allow(clippy::type_complexity)]
    pub fn load_host(
        &self,
        host: EndpointId,
    ) -> (Vec<(u64, Outgoing)>, Vec<(Option<MessageId>, String)>) {
        let read = self.db.read();
        let outbox = read
            .open_table(OUTBOX)
            .range((host.0, 0)..=(host.0, u64::MAX))
            .map(|(key, value)| (key.value().1, value.value().into_owned()))
            .collect();
        let drafts = read
            .open_table(DRAFTS)
            .range((host.0, 0)..=(host.0, u64::MAX))
            .map(|(key, value)| (parent_of(key.value().1), value.value().to_owned()))
            .collect();
        (outbox, drafts)
    }

    /// Whether anything was kept under the host's seed; if so, it is under
    /// the endpoint once this returns (`temp-migration`).
    pub fn adopt_seed(&self, machine_seed: u64, host: EndpointId) -> bool {
        let known = {
            let read = self.db.read();
            read.open_table(seeded::CURSOR).get(machine_seed).is_some()
                || read
                    .open_table(seeded::OUTBOX)
                    .range((machine_seed, 0)..=(machine_seed, u64::MAX))
                    .next()
                    .is_some()
                || read
                    .open_table(seeded::DRAFTS)
                    .range((machine_seed, 0)..=(machine_seed, u64::MAX))
                    .next()
                    .is_some()
        };
        if !known {
            return false;
        }
        self.send(Write::Adopt {
            machine_seed,
            host: host.0,
        });
        self.flush();
        true
    }

    pub fn write_records(&self, host: EndpointId, records: Vec<ChatRecord>) {
        self.send(Write::Records {
            host: host.0,
            records,
        });
    }

    pub fn reset(&self, host: EndpointId) {
        self.send(Write::Reset(host.0));
    }

    pub fn write_outbox(&self, host: EndpointId, kept: Kept) {
        self.send(Write::Outbox(host.0, kept));
    }

    /// A draft as it stands; empty drops it.
    pub fn write_draft(&self, host: EndpointId, parent: Option<MessageId>, text: String) {
        self.send(Write::Draft(host.0, parent, text));
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

/// Moves one host's unsent messages and drafts from under its seed to
/// under its endpoint, and drops the copy of its log kept under the seed
/// (`temp-migration`).
fn adopt(transaction: &mut rho_db::WriteTxn, machine_seed: u64, host: Host) {
    let range = (machine_seed, 0)..=(machine_seed, u64::MAX);
    let unsent: Vec<(u64, Outgoing)> = transaction
        .open_table(seeded::OUTBOX)
        .range(range.clone())
        .map(|(key, value)| (key.value().1, value.value().into_owned()))
        .collect();
    let drafts: Vec<(u64, String)> = transaction
        .open_table(seeded::DRAFTS)
        .range(range.clone())
        .map(|(key, value)| (key.value().1, value.value().to_owned()))
        .collect();
    let log: Vec<u64> = transaction
        .open_table(seeded::LOG)
        .range(range)
        .map(|(key, _)| key.value().1)
        .collect();
    {
        let mut outbox = transaction.open_table(OUTBOX);
        for (id, outgoing) in &unsent {
            outbox.insert((host, *id), SenValue::borrowed(outgoing));
        }
    }
    {
        let mut old = transaction.open_table(seeded::OUTBOX);
        for (id, _) in &unsent {
            old.remove((machine_seed, *id));
        }
    }
    {
        let mut table = transaction.open_table(DRAFTS);
        for (parent, text) in &drafts {
            table.insert((host, *parent), text.as_str());
        }
    }
    {
        let mut old = transaction.open_table(seeded::DRAFTS);
        for (parent, _) in &drafts {
            old.remove((machine_seed, *parent));
        }
    }
    {
        let mut old = transaction.open_table(seeded::LOG);
        for seq in log {
            old.remove((machine_seed, seq));
        }
    }
    transaction.open_table(seeded::CURSOR).remove(machine_seed);
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
        Write::Records { host, records } => {
            let Some(last) = records.last() else {
                return;
            };
            let through = last.seq.0;
            let mut log = transaction.open_table(LOG);
            for record in &records {
                log.insert((host, record.seq.0), SenValue::borrowed(record));
            }
            drop(log);
            let mut cursor = transaction.open_table(CURSOR);
            let held = cursor.get(host).map_or(0, |held| held.value());
            cursor.insert(host, held.max(through));
        }
        Write::Reset(host) => {
            let mut log = transaction.open_table(LOG);
            let held: Vec<u64> = log
                .range((host, 0)..=(host, u64::MAX))
                .map(|(key, _)| key.value().1)
                .collect();
            for seq in held {
                log.remove((host, seq));
            }
            drop(log);
            transaction.open_table(CURSOR).remove(host);
        }
        Write::Outbox(host, Kept::Put(id, outgoing)) => {
            transaction
                .open_table(OUTBOX)
                .insert((host, id), SenValue::borrowed(&outgoing));
        }
        Write::Outbox(host, Kept::Gone(id)) => {
            transaction.open_table(OUTBOX).remove((host, id));
        }
        Write::Draft(host, parent, text) => {
            let mut drafts = transaction.open_table(DRAFTS);
            let key = (host, parent_key(parent));
            if text.is_empty() {
                drafts.remove(key);
            } else {
                drafts.insert(key, text.as_str());
            }
        }
        Write::Adopt { machine_seed, host } => adopt(transaction, machine_seed, host),
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

    const H1: EndpointId = EndpointId([1; 32]);
    const H2: EndpointId = EndpointId([2; 32]);

    /// The log, cursor, outbox and drafts come back as written; a reset
    /// drops one host's log and cursor and leaves the other's.
    #[test]
    fn the_copy_reads_back_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::client::open(dir.path()).unwrap();
        let cache = Cache::open_on(db.clone()).unwrap();
        cache.write_records(H1, vec![record(1), record(2)]);
        cache.write_records(H2, vec![record(1)]);
        cache.write_records(H1, vec![record(3)]);
        let outgoing = Outgoing {
            at: UnixMs(5),
            parent: Some(MessageId(ChatSeq(2))),
            mentions: Vec::new(),
            content: vec![ContentPart::Text { text: "hi".into() }],
        };
        cache.write_outbox(H1, Kept::Put(7, outgoing.clone()));
        cache.write_outbox(H1, Kept::Put(8, outgoing.clone()));
        cache.write_outbox(H1, Kept::Gone(8));
        cache.write_draft(H1, None, "a root".into());
        cache.write_draft(H1, Some(MessageId(ChatSeq(2))), "under two".into());
        cache.write_draft(H1, Some(MessageId(ChatSeq(2))), String::new());
        cache.flush();

        let loaded = cache.load();
        let mut cursors = loaded.cursors.clone();
        cursors.sort();
        assert_eq!(cursors, [(H1, ChatSeq(3)), (H2, ChatSeq(1))]);
        assert_eq!(loaded.outbox, [(H1, 7, outgoing.clone())]);
        assert_eq!(loaded.drafts, [(H1, None, "a root".to_owned())]);
        assert_eq!(
            cache.load_host(H1),
            (vec![(7, outgoing)], vec![(None, "a root".to_owned())])
        );
        assert_eq!(cache.load_host(H2), (Vec::new(), Vec::new()));
        assert_eq!(
            cache
                .read_log(H1)
                .iter()
                .map(|record| record.seq.0)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );

        cache.reset(H1);
        cache.flush();
        assert!(cache.read_log(H1).is_empty());
        assert_eq!(cache.load().cursors, [(H2, ChatSeq(1))]);
        assert_eq!(cache.read_log(H2).len(), 1);
        drop(cache);
        // Opened again on the same file: the tables are the same shape, so
        // nothing is dropped.
        let cache = Cache::open_on(db).unwrap();
        assert_eq!(cache.read_log(H2).len(), 1);
    }

    /// What was kept under a host's seed moves under its endpoint once:
    /// the unsent messages and drafts come across, the log copy is
    /// dropped, and another seed's rows stay where they are.
    #[test]
    fn a_seeds_rows_are_adopted_by_its_endpoint_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::client::open(dir.path()).unwrap();
        let outgoing = Outgoing {
            at: UnixMs(5),
            parent: None,
            mentions: Vec::new(),
            content: vec![ContentPart::Text { text: "hi".into() }],
        };
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async {
                let mut write = db.write().await;
                for seed in [5, 6] {
                    write
                        .open_table(seeded::LOG)
                        .insert((seed, 1), SenValue::borrowed(&record(1)));
                    write.open_table(seeded::CURSOR).insert(seed, 1);
                    write
                        .open_table(seeded::OUTBOX)
                        .insert((seed, 9), SenValue::borrowed(&outgoing));
                    write
                        .open_table(seeded::DRAFTS)
                        .insert((seed, 1), "under one");
                }
                write.commit();
            });

        let cache = Cache::open_on(db.clone()).unwrap();
        assert_eq!(cache.load(), Loaded::default());
        assert!(!cache.adopt_seed(7, H2));
        assert!(cache.adopt_seed(5, H1));
        assert!(!cache.adopt_seed(5, H1));
        let loaded = cache.load();
        assert_eq!(loaded.cursors, []);
        assert_eq!(loaded.outbox, [(H1, 9, outgoing.clone())]);
        assert_eq!(
            loaded.drafts,
            [(H1, Some(MessageId(ChatSeq(1))), "under one".to_owned())]
        );
        assert!(cache.read_log(H1).is_empty());
        drop(cache);
        let read = db.read();
        assert!(
            read.open_table(seeded::LOG)
                .range((5, 0)..=(5, u64::MAX))
                .next()
                .is_none()
        );
        assert!(read.open_table(seeded::CURSOR).get(5).is_none());
        assert!(read.open_table(seeded::CURSOR).get(6).is_some());
        assert_eq!(
            read.open_table(seeded::OUTBOX)
                .range((6, 0)..=(6, u64::MAX))
                .count(),
            1
        );
        assert_eq!(
            read.open_table(seeded::DRAFTS)
                .range((6, 0)..=(6, u64::MAX))
                .count(),
            1
        );
    }

    /// The topic client's unsent messages and drafts come across: a topic
    /// becomes the root message under its number, a topic yet to start
    /// becomes a root post, and the old tables are gone afterwards. They
    /// wait under the host's seed until its endpoint adopts them.
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
        assert!(cache.load().outbox.is_empty());
        assert!(cache.adopt_seed(1, H1));
        let loaded = cache.load();
        assert_eq!(
            loaded.outbox,
            [
                (
                    H1,
                    10,
                    Outgoing {
                        at: UnixMs(1),
                        parent: Some(MessageId(ChatSeq(4))),
                        mentions: vec![agent],
                        content: text("in topic 4"),
                    }
                ),
                (
                    H1,
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
            [(H1, Some(MessageId(ChatSeq(4))), "half a reply".to_owned())]
        );
        drop(cache);
        let read = db.read();
        assert!(!read.has_table(legacy::OUTBOX.name()));
        assert!(!read.has_table(legacy::DRAFTS.name()));
    }
}

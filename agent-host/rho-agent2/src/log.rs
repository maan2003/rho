//! The agent's log: the one record on the host. The model's context and the
//! chat are both projections of it, and it is only ever appended to.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::Path;

pub use rho_agent_types::AgentId;
use rho_agent_types::UnixMs;
use rho_inference2::{CacheKey, Call, Carry, Image, Usage};
use senax_encoder::{Decode, Encode};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
pub struct MessageId(pub u64);

impl MessageId {
    pub fn new() -> Self {
        Self(rand::random())
    }
}

impl Default for MessageId {
    fn default() -> Self {
        Self::new()
    }
}

/// Who a message is from or to.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub enum Party {
    Human,
    Agent(AgentId),
}

/// Part of a message body. Text is kept as written; structure lives around
/// it, never inside it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Block {
    Text(String),
    /// A span of an earlier message, by byte range.
    Quote {
        of: MessageId,
        start: u64,
        end: u64,
    },
}

/// Why the model was woken.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Wake {
    Message,
    AgentMessage,
    /// The latest cell's code returned.
    Returned,
    Notify,
    /// A command or host call ended.
    Failure,
    Checkin,
    /// The last step wrote prose and made no call.
    Prose,
    /// The host restarted; everything running is gone.
    Restarted,
    /// The model's history has branched; notebook state was not changed.
    Rewound,
    /// Request the provider to compact the current context.
    Compaction,
    /// Continue a turn after a provider-only compaction response.
    CompactionReply,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Notice {
    Error(String),
    Restarted,
    Archived,
    FreshNotebook,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Entry {
    /// The first entry.
    Created {
        at: UnixMs,
        cache_key: CacheKey,
    },
    /// One model response, and the cell it started.
    Step {
        at: UnixMs,
        call: Option<Call>,
        prose: String,
        carry: Carry,
        usage: Usage,
    },
    /// What the model was shown when woken: news from the notebook, and the
    /// messages delivered with it.
    Woken {
        at: UnixMs,
        why: Wake,
        report: String,
        images: Vec<Image>,
        messages: Vec<MessageId>,
    },
    Received {
        at: UnixMs,
        id: MessageId,
        from: Party,
        body: Vec<Block>,
    },
    Sent {
        at: UnixMs,
        id: MessageId,
        to: Party,
        text: String,
    },
    Status {
        at: UnixMs,
        text: String,
    },
    /// Since when some cell awaits `human.reply()`; `None` once none does.
    Awaiting {
        at: UnixMs,
        since: Option<UnixMs>,
    },
    Notice {
        at: UnixMs,
        notice: Notice,
    },
    /// Provider compaction is requested through the model's normal input.
    CompactionTrigger {
        at: UnixMs,
        /// A pure manual request needs no model answer after compaction.
        manual: bool,
    },
    /// Branch before physical log index `to`; abandoned events remain on disk.
    Rewound {
        at: UnixMs,
        to: u64,
    },
}

impl Entry {
    pub fn at(&self) -> UnixMs {
        match self {
            Entry::Created { at, .. }
            | Entry::Step { at, .. }
            | Entry::Woken { at, .. }
            | Entry::Received { at, .. }
            | Entry::Sent { at, .. }
            | Entry::Status { at, .. }
            | Entry::Awaiting { at, .. }
            | Entry::Notice { at, .. }
            | Entry::Rewound { at, .. }
            | Entry::CompactionTrigger { at, .. } => *at,
        }
    }
}

/// Physical indices on the branch selected by append-only rewind markers.
pub fn visible_positions(entries: &[Entry]) -> Vec<usize> {
    let mut visible = Vec::new();
    for (position, entry) in entries.iter().enumerate() {
        if let Entry::Rewound { to, .. } = entry {
            visible.retain(|kept| (*kept as u64) < *to);
        }
        visible.push(position);
    }
    visible
}

/// The log in memory, and on disk as length-prefixed senax records when it
/// has a file.
pub struct Log {
    entries: Vec<Entry>,
    file: Option<File>,
}

impl Log {
    pub fn in_memory() -> Self {
        Self {
            entries: Vec::new(),
            file: None,
        }
    }

    /// Open `path`, reading what it holds. A torn last record, from a crash
    /// mid-write, is dropped.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let mut entries = Vec::new();
        let mut valid = 0u64;
        if path.exists() {
            let mut reader = BufReader::new(File::open(path)?);
            loop {
                let mut len = [0u8; 4];
                if reader.read_exact(&mut len).is_err() {
                    break;
                }
                let mut record = vec![0u8; u32::from_le_bytes(len) as usize];
                if reader.read_exact(&mut record).is_err() {
                    break;
                }
                let Ok(entry) = senax_encoder::decode::<Entry>(&mut &record[..]) else {
                    break;
                };
                entries.push(entry);
                valid += 4 + record.len() as u64;
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        file.set_len(valid)?;
        let mut log = Self {
            entries,
            file: Some(file),
        };
        if let Some(file) = &mut log.file {
            use std::io::Seek;
            file.seek(std::io::SeekFrom::End(0))?;
        }
        Ok(log)
    }

    pub fn append(&mut self, entry: Entry) -> anyhow::Result<()> {
        if let Some(file) = &mut self.file {
            let record = senax_encoder::encode(&entry)?;
            let mut framed = Vec::with_capacity(4 + record.len());
            framed.extend_from_slice(&(record.len() as u32).to_le_bytes());
            framed.extend_from_slice(&record);
            file.write_all(&framed)?;
            file.sync_data()?;
        }
        self.entries.push(entry);
        Ok(())
    }

    /// Physical positions of the current branch, oldest first. Rewind
    /// markers remain visible, while entries on abandoned branches do not.
    /// Positions, unlike branch indices, never change after an append.
    pub fn visible_positions(&self) -> Vec<usize> {
        visible_positions(&self.entries)
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_reopens_with_what_was_appended_and_drops_a_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let mut log = Log::open(&path).unwrap();
        log.append(Entry::Created {
            at: UnixMs(1),
            cache_key: CacheKey::new(),
        })
        .unwrap();
        log.append(Entry::Status {
            at: UnixMs(2),
            text: "reading".into(),
        })
        .unwrap();
        drop(log);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[9, 0, 0, 0, 1]).unwrap();
        drop(file);
        let mut log = Log::open(&path).unwrap();
        assert_eq!(log.entries().len(), 2);
        log.append(Entry::Status {
            at: UnixMs(3),
            text: "done".into(),
        })
        .unwrap();
        assert_eq!(Log::open(&path).unwrap().entries().len(), 3);
    }
}

#[cfg(test)]
mod branch_tests {
    use super::*;

    fn text(id: u64) -> Entry {
        Entry::Received {
            at: UnixMs(id),
            id: MessageId(id),
            from: Party::Human,
            body: vec![Block::Text(format!("message {id}"))],
        }
    }

    #[test]
    fn rewinds_select_an_append_only_lineage_even_after_a_second_fork() {
        let mut log = Log::in_memory();
        for entry in [text(1), text(2), text(3)] {
            log.append(entry).unwrap();
        }
        log.append(Entry::Rewound {
            at: UnixMs(4),
            to: 1,
        })
        .unwrap();
        log.append(text(5)).unwrap();
        log.append(Entry::Rewound {
            at: UnixMs(6),
            to: 1,
        })
        .unwrap();
        log.append(text(7)).unwrap();
        assert_eq!(log.entries().len(), 7, "abandoned branch remains on disk");
        assert_eq!(log.visible_positions(), vec![0, 5, 6]);
    }
}

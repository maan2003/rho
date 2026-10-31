//! The rho2 store: workset logs, the heads projected from them, and the
//! mail log ([ARCH-rho2](../../specs/ARCH-rho2.md)).
//!
//! Each workset has one log, keyed by `(workset, wpos)`, dense from zero and
//! never rewritten. Agent and workset heads are folded from it in the same
//! transaction as every append. Mail is one host-wide log, dense from one.
//! Deleting a workset removes its log and heads; mail stays.

use redb::TableDefinition;
use rho_agent_types::{
    Address, AgentId, ContentPart, InputWake, Mail, MailId, SendKind, UnixMs, WorksetId,
    WorksetKind, Wpos,
};
use rho_db::{ReadTxn, Sen, SenValue, WriteTxn};
use senax_encoder::{Decode, Encode};

use crate::entry::{CompactionState, Entry, Notice};
use crate::log::ClaudeRewind;
use crate::record::{AgentSpec, ClaudeRow, Event, Record};

type WorksetKey = [u8; 6];

const WORKSET_LOG: TableDefinition<(WorksetKey, u64), Sen<Record<'static>>> =
    TableDefinition::new("workset_log");
const WORKSET_HEADS: TableDefinition<WorksetKey, Sen<WorksetHead>> =
    TableDefinition::new("workset_heads");
const AGENT_HEADS: TableDefinition<AgentId, Sen<AgentHead>> =
    TableDefinition::new("workset_agent_heads");
const MAIL_LOG: TableDefinition<u64, Sen<Mail>> = TableDefinition::new("mail_log");
/// The user's mail by the client's own id for it, so a resend is logged once.
const MAIL_CLIENT_IDS: TableDefinition<u64, u64> = TableDefinition::new("mail_client_ids");

/// What a workset is, folded from its log.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct WorksetHead {
    pub kind: WorksetKind,
    pub created_at: UnixMs,
    /// Where the next row goes.
    pub next: Wpos,
    /// Its agents, in creation order.
    pub agents: Vec<AgentId>,
}

/// What an agent is now, folded from its rows in its workset's log.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct AgentHead {
    pub workset: WorksetId,
    pub spec: AgentSpec,
    pub created_at: UnixMs,
    /// Its `AgentCreated` row.
    pub created: Wpos,
    /// Its newest row.
    pub last: Wpos,
    /// The newest mail handed to it by an `Input`.
    pub delivered: MailId,
    /// A Claude rewind whose destination is not verified yet.
    pub claude_rewind: Option<ClaudeRewind>,
    pub native: NativeCursor,
}

/// The native replay position and recovery facts, derived from the agent's
/// visible rows.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct NativeCursor {
    /// Where the native context starts: after the request a compaction
    /// answered, or the agent's creation.
    pub from: Wpos,
    /// The agent's newest request, which a compacting step answers.
    request: Option<Wpos>,
    pub archived: bool,
    /// A request was ever made: a restart owes a notice.
    pub woken: bool,
    pub compaction: CompactionState,
}

impl NativeCursor {
    fn observe(&mut self, pos: Wpos, entry: &Entry) {
        self.compaction.observe(entry);
        match entry {
            Entry::RequestSent { .. } => {
                self.request = Some(pos);
                self.woken = true;
            }
            Entry::Step { carry, .. } => {
                // An input after the request may not have been delivered by
                // it, so the context starts just after the request.
                if carry.has_compaction()
                    && let Some(request) = self.request
                {
                    self.from = request.next();
                }
                self.request = None;
            }
            Entry::Notice {
                notice: Notice::Archived,
                ..
            } => self.archived = true,
            Entry::Notice {
                notice: Notice::FreshNotebook,
                ..
            } => self.archived = false,
            _ => {}
        }
    }
}

impl AgentHead {
    fn fold(&mut self, pos: Wpos, event: &Event<'_>) {
        self.last = pos;
        match event {
            Event::AgentChanged { spec, .. } => self.spec = spec.clone(),
            Event::Input {
                mail: Some(mail), ..
            } => self.delivered = self.delivered.max(*mail),
            Event::Native { entry, .. } => self.native.observe(pos, entry),
            Event::Claude {
                row: ClaudeRow::RewindPending(rewind),
                ..
            } => self.claude_rewind = rewind.clone(),
            Event::WorksetCreated { .. }
            | Event::Evaluated { .. }
            | Event::AgentCreated { .. }
            | Event::Input { .. }
            | Event::Rewound { .. }
            | Event::Failed { .. }
            | Event::ExecObserved { .. }
            | Event::Claude { .. } => {}
        }
    }
}

/// One append, after it commits: what a follower needs to read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appended {
    Row { workset: WorksetId, pos: Wpos },
    Mail(MailId),
    WorksetDeleted(WorksetId),
}

/// The database's append observer, so a reader learns of every commit
/// without each writer carrying a channel. A lagged reader catches up from
/// the tables, which are the truth.
pub struct Feed {
    sender: tokio::sync::broadcast::Sender<Appended>,
}

pub fn feed(db: &rho_db::RhoDb) -> tokio::sync::broadcast::Receiver<Appended> {
    db.observer(|| Feed {
        sender: tokio::sync::broadcast::Sender::new(4096),
    })
    .sender
    .subscribe()
}

fn tell(write: &mut WriteTxn, appended: Appended) {
    if let Some(feed) = write.observer::<Feed>() {
        let sender = feed.sender.clone();
        write.after_commit(move || {
            let _ = sender.send(appended);
        });
    }
}

fn log_range(workset: WorksetId, from: Wpos) -> std::ops::RangeInclusive<(WorksetKey, u64)> {
    (workset.0, from.0)..=(workset.0, u64::MAX)
}

/// An agent's rows that are still its history, oldest first: a `Rewound`
/// takes back the agent's rows from `to` up to itself.
fn visible(rows: Vec<(Wpos, Record<'static>)>) -> Vec<(Wpos, Record<'static>)> {
    let mut kept: Vec<(Wpos, Record<'static>)> = Vec::with_capacity(rows.len());
    for (pos, record) in rows {
        if let Event::Rewound { to, .. } = &record.event {
            let keep = kept.partition_point(|(kept, _)| kept < to);
            kept.truncate(keep);
        }
        kept.push((pos, record));
    }
    kept
}

pub trait StoreRead {
    fn workset(&self, workset: WorksetId) -> Option<WorksetHead>;
    fn worksets(&self) -> Vec<(WorksetId, WorksetHead)>;
    fn agent(&self, agent: AgentId) -> Option<AgentHead>;
    fn agents(&self) -> Vec<(AgentId, AgentHead)>;
    /// Up to `limit` rows of a workset from `from`.
    fn workset_rows(
        &self,
        workset: WorksetId,
        from: Wpos,
        limit: usize,
    ) -> Vec<(Wpos, Record<'static>)>;
    fn workset_row(&self, workset: WorksetId, pos: Wpos) -> Option<Record<'static>>;
    /// The agent's visible rows from `from`, rewinds applied.
    fn agent_rows(&self, agent: AgentId, from: Wpos) -> Vec<(Wpos, Record<'static>)>;
    fn mail_head(&self) -> MailId;
    /// Up to `limit` mails after `after`, in order.
    fn mail_since(&self, after: MailId, limit: usize) -> Vec<Mail>;
    fn mail(&self, id: MailId) -> Option<Mail>;
    /// Mail to `to` after `after`, in order.
    fn inbox(&self, to: Address, after: MailId) -> Vec<Mail>;
}

impl StoreRead for ReadTxn {
    fn workset(&self, workset: WorksetId) -> Option<WorksetHead> {
        self.open_table(WORKSET_HEADS)
            .get(workset.0)
            .map(|head| head.value().into_owned())
    }

    fn worksets(&self) -> Vec<(WorksetId, WorksetHead)> {
        self.open_table(WORKSET_HEADS)
            .iter()
            .map(|(key, head)| (WorksetId(key.value()), head.value().into_owned()))
            .collect()
    }

    fn agent(&self, agent: AgentId) -> Option<AgentHead> {
        self.open_table(AGENT_HEADS)
            .get(agent)
            .map(|head| head.value().into_owned())
    }

    fn agents(&self) -> Vec<(AgentId, AgentHead)> {
        self.open_table(AGENT_HEADS)
            .iter()
            .map(|(key, head)| (key.value(), head.value().into_owned()))
            .collect()
    }

    fn workset_rows(
        &self,
        workset: WorksetId,
        from: Wpos,
        limit: usize,
    ) -> Vec<(Wpos, Record<'static>)> {
        self.open_table(WORKSET_LOG)
            .range(log_range(workset, from))
            .take(limit)
            .map(|(key, record)| (Wpos(key.value().1), record.value().into_owned()))
            .collect()
    }

    fn workset_row(&self, workset: WorksetId, pos: Wpos) -> Option<Record<'static>> {
        self.open_table(WORKSET_LOG)
            .get((workset.0, pos.0))
            .map(|record| record.value().into_owned())
    }

    fn agent_rows(&self, agent: AgentId, from: Wpos) -> Vec<(Wpos, Record<'static>)> {
        let Some(head) = self.agent(agent) else {
            return Vec::new();
        };
        // Rewinds are read from the agent's start, so one that hides rows
        // after `from` is seen even when it was made from before `from`.
        let rows = self
            .open_table(WORKSET_LOG)
            .range(log_range(head.workset, head.created))
            .map(|(key, record)| (Wpos(key.value().1), record.value().into_owned()))
            .filter(|(_, record)| record.event.agent() == Some(agent))
            .collect();
        visible(rows)
            .into_iter()
            .filter(|(pos, _)| *pos >= from)
            .collect()
    }

    fn mail_head(&self) -> MailId {
        self.open_table(MAIL_LOG)
            .iter()
            .next_back()
            .map_or(MailId(0), |(key, _)| MailId(key.value()))
    }

    fn mail_since(&self, after: MailId, limit: usize) -> Vec<Mail> {
        self.open_table(MAIL_LOG)
            .range(after.0 + 1..)
            .take(limit)
            .map(|(_, mail)| mail.value().into_owned())
            .collect()
    }

    fn mail(&self, id: MailId) -> Option<Mail> {
        self.open_table(MAIL_LOG)
            .get(id.0)
            .map(|mail| mail.value().into_owned())
    }

    fn inbox(&self, to: Address, after: MailId) -> Vec<Mail> {
        self.open_table(MAIL_LOG)
            .range(after.0 + 1..)
            .map(|(_, mail)| mail.value().into_owned())
            .filter(|mail| mail.to == to)
            .collect()
    }
}

/// A mail as its sender gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMail {
    pub from: Address,
    pub to: Address,
    pub re: Option<MailId>,
    pub kind: Option<SendKind>,
    pub content: Vec<ContentPart>,
}

pub trait StoreWrite {
    fn init_store_tables(&mut self);
    /// A new workset with its first row. Panics if the id is taken.
    fn create_workset(&mut self, workset: WorksetId, at: UnixMs, kind: WorksetKind, code: String);
    /// Append one row to a workset's log, folding the heads it changes.
    fn append(&mut self, workset: WorksetId, record: &Record<'_>) -> anyhow::Result<Wpos>;
    /// Append a mail; with `client_id`, a mail already posted under that id
    /// is returned instead of a second one.
    fn post_mail(&mut self, at: UnixMs, mail: NewMail, client_id: Option<u64>) -> MailId;
    /// Remove a workset's log and heads. Mail stays.
    fn delete_workset(&mut self, workset: WorksetId) -> bool;
    /// An agent's head, read inside this transaction.
    fn agent_head(&mut self, agent: AgentId) -> Option<AgentHead>;
}

impl StoreWrite for WriteTxn {
    fn init_store_tables(&mut self) {
        self.open_table(WORKSET_LOG);
        self.open_table(WORKSET_HEADS);
        self.open_table(AGENT_HEADS);
        self.open_table(MAIL_LOG);
        self.open_table(MAIL_CLIENT_IDS);
    }

    fn create_workset(&mut self, workset: WorksetId, at: UnixMs, kind: WorksetKind, code: String) {
        assert!(
            self.open_table(WORKSET_HEADS).get(workset.0).is_none(),
            "workset {workset} exists"
        );
        let head = WorksetHead {
            kind: kind.clone(),
            created_at: at,
            next: Wpos::ZERO,
            agents: Vec::new(),
        };
        self.open_table(WORKSET_HEADS)
            .insert(workset.0, SenValue::borrowed(&head));
        self.append(
            workset,
            &Record {
                at,
                event: Event::WorksetCreated { kind, code },
            },
        )
        .expect("a new workset takes its first row");
    }

    fn append(&mut self, workset: WorksetId, record: &Record<'_>) -> anyhow::Result<Wpos> {
        let mut head = self
            .open_table(WORKSET_HEADS)
            .get(workset.0)
            .map(|head| head.value().into_owned())
            .ok_or_else(|| anyhow::anyhow!("no workset {workset}"))?;
        let pos = head.next;
        anyhow::ensure!(
            (pos == Wpos::ZERO) == matches!(record.event, Event::WorksetCreated { .. }),
            "a workset's creation is its first row and nothing else is"
        );
        match (&record.event, record.event.agent()) {
            (Event::AgentCreated { agent, spec }, _) => {
                anyhow::ensure!(
                    self.open_table(AGENT_HEADS).get(*agent).is_none(),
                    "agent {} exists",
                    agent.encoded()
                );
                head.agents.push(*agent);
                let agent_head = AgentHead {
                    workset,
                    spec: spec.clone(),
                    created_at: record.at,
                    created: pos,
                    last: pos,
                    delivered: MailId(0),
                    claude_rewind: None,
                    native: NativeCursor {
                        from: pos,
                        ..NativeCursor::default()
                    },
                };
                self.open_table(AGENT_HEADS)
                    .insert(*agent, SenValue::borrowed(&agent_head));
            }
            (_, Some(agent)) => {
                let mut agent_head = self
                    .open_table(AGENT_HEADS)
                    .get(agent)
                    .map(|head| head.value().into_owned())
                    .filter(|head| head.workset == workset)
                    .ok_or_else(|| {
                        anyhow::anyhow!("no agent {} in workset {workset}", agent.encoded())
                    })?;
                if let Event::Rewound { to, .. } = &record.event {
                    anyhow::ensure!(
                        *to > agent_head.created && *to <= agent_head.last,
                        "a rewind stays within the agent's history"
                    );
                }
                agent_head.fold(pos, &record.event);
                self.open_table(AGENT_HEADS)
                    .insert(agent, SenValue::borrowed(&agent_head));
                if matches!(record.event, Event::Rewound { .. }) {
                    // The native cursor is a fold of visible rows; a rewind
                    // changes which rows those are.
                    self.open_table(WORKSET_LOG)
                        .insert((workset.0, pos.0), SenValue::borrowed(record));
                    head.next = pos.next();
                    self.open_table(WORKSET_HEADS)
                        .insert(workset.0, SenValue::borrowed(&head));
                    rebuild_native(self, agent);
                    tell(self, Appended::Row { workset, pos });
                    return Ok(pos);
                }
            }
            (_, None) => {}
        }
        self.open_table(WORKSET_LOG)
            .insert((workset.0, pos.0), SenValue::borrowed(record));
        head.next = pos.next();
        self.open_table(WORKSET_HEADS)
            .insert(workset.0, SenValue::borrowed(&head));
        tell(self, Appended::Row { workset, pos });
        Ok(pos)
    }

    fn post_mail(&mut self, at: UnixMs, mail: NewMail, client_id: Option<u64>) -> MailId {
        if let Some(client_id) = client_id
            && let Some(id) = self.open_table(MAIL_CLIENT_IDS).get(client_id)
        {
            return MailId(id.value());
        }
        let id = {
            let log = self.open_table(MAIL_LOG);
            log.iter()
                .next_back()
                .map_or(MailId(1), |(key, _)| MailId(key.value()).next())
        };
        let NewMail {
            from,
            to,
            re,
            kind,
            content,
        } = mail;
        let mail = Mail {
            id,
            at,
            from,
            to,
            re,
            kind,
            content,
        };
        self.open_table(MAIL_LOG)
            .insert(id.0, SenValue::borrowed(&mail));
        if let Some(client_id) = client_id {
            self.open_table(MAIL_CLIENT_IDS).insert(client_id, id.0);
        }
        tell(self, Appended::Mail(id));
        id
    }

    fn agent_head(&mut self, agent: AgentId) -> Option<AgentHead> {
        self.open_table(AGENT_HEADS)
            .get(agent)
            .map(|head| head.value().into_owned())
    }

    fn delete_workset(&mut self, workset: WorksetId) -> bool {
        let Some(head) = self
            .open_table(WORKSET_HEADS)
            .get(workset.0)
            .map(|head| head.value().into_owned())
        else {
            return false;
        };
        for agent in &head.agents {
            self.open_table(AGENT_HEADS).remove(*agent);
        }
        let positions: Vec<u64> = self
            .open_table(WORKSET_LOG)
            .range(log_range(workset, Wpos::ZERO))
            .map(|(key, _)| key.value().1)
            .collect();
        let mut log = self.open_table(WORKSET_LOG);
        for pos in positions {
            log.remove((workset.0, pos));
        }
        drop(log);
        self.open_table(WORKSET_HEADS).remove(workset.0);
        tell(self, Appended::WorksetDeleted(workset));
        true
    }
}

/// Fold the native cursor again from the agent's visible rows.
fn rebuild_native(write: &mut WriteTxn, agent: AgentId) {
    let mut head = write
        .open_table(AGENT_HEADS)
        .get(agent)
        .expect("agent head")
        .value()
        .into_owned();
    let rows: Vec<(Wpos, Record<'static>)> = write
        .open_table(WORKSET_LOG)
        .range(log_range(head.workset, head.created))
        .map(|(key, record)| (Wpos(key.value().1), record.value().into_owned()))
        .filter(|(_, record)| record.event.agent() == Some(agent))
        .collect();
    let mut cursor = NativeCursor {
        from: head.created,
        ..NativeCursor::default()
    };
    for (pos, record) in visible(rows) {
        if let Event::Native { entry, .. } = &record.event {
            cursor.observe(pos, entry);
        }
    }
    head.native = cursor;
    write
        .open_table(AGENT_HEADS)
        .insert(agent, SenValue::borrowed(&head));
}

/// The wake an input from `from` gets.
pub fn wake_of(from: Address) -> InputWake {
    match from {
        Address::User => InputWake::Person,
        Address::Agent(_) => InputWake::Agent,
    }
}

#[cfg(test)]
mod tests;

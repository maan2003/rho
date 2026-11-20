//! A host's log folded into its forest: every message, where each hangs,
//! what each thread is named, and who is a member of what.
//!
//! The fold is told each record once, oldest first, and answers what the
//! record changed ([`Told`]) so a screen refreshes only that. It is the
//! same fold whether the records come from the disk copy or the host.

use std::collections::{BTreeMap, HashMap};

use rho_agent_types::{
    Address, AgentId, ChatEvent, ChatRecord, ChatSeq, MentionKind, Message, MessageId, Revision,
    ThreadStatus, UnixMs, one_line,
};

/// What one record changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Told {
    /// A message was posted, in the thread named.
    Posted {
        message: MessageId,
        thread: MessageId,
    },
    /// A message and what hangs under it moved; the thread it was in and
    /// the one it is in now (each `None` for the roots).
    Moved {
        message: MessageId,
        from: Option<MessageId>,
        to: Option<MessageId>,
    },
    /// A mail's content or a thread's members or name changed.
    Thread(MessageId),
    /// A record that changed nothing.
    Nothing,
}

/// A thread's head: what a screen names it by, and whom a message in it
/// reaches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    pub id: MessageId,
    pub name: String,
    /// From the root down to this thread: each thread's id and name.
    pub path: Vec<(MessageId, String)>,
    pub members: Vec<AgentId>,
}

/// One host's forest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostChat {
    /// The newest record told.
    pub head: ChatSeq,
    messages: BTreeMap<MessageId, Message>,
    /// Each message's children in log order; `None` holds the roots.
    children: BTreeMap<Option<MessageId>, Vec<MessageId>>,
    names: HashMap<MessageId, String>,
    statuses: BTreeMap<MessageId, ThreadStatus>,
    /// What each agent last said about each thread: joined or left.
    members: BTreeMap<(MessageId, AgentId), bool>,
    /// The first thread each agent joined: its thread, the one it started
    /// from, where a stop it did not choose draws the user.
    first_joined: BTreeMap<AgentId, MessageId>,
}

impl HostChat {
    pub fn tell(&mut self, record: &ChatRecord) -> Told {
        if record.seq <= self.head {
            return Told::Nothing;
        }
        self.head = record.seq;
        match &record.event {
            ChatEvent::Posted { parent, .. } => {
                let Some(message) = Message::from_record(record) else {
                    return Told::Nothing;
                };
                let id = message.id;
                self.children.entry(*parent).or_default().push(id);
                self.messages.insert(id, message);
                Told::Posted {
                    message: id,
                    thread: self.thread_of(id),
                }
            }
            ChatEvent::Reparented { message, parent } => {
                let Some(from) = self.messages.get(message).map(|found| found.parent) else {
                    return Told::Nothing;
                };
                if from == *parent || self.is_above_or_self(*message, *parent) {
                    return Told::Nothing;
                }
                if let Some(siblings) = self.children.get_mut(&from) {
                    siblings.retain(|sibling| sibling != message);
                }
                self.children.entry(*parent).or_default().push(*message);
                self.messages.get_mut(message).expect("checked").parent = *parent;
                Told::Moved {
                    message: *message,
                    from,
                    to: *parent,
                }
            }
            ChatEvent::Joined { thread, agent } => {
                self.members.insert((*thread, *agent), true);
                self.first_joined.entry(*agent).or_insert(*thread);
                Told::Thread(*thread)
            }
            ChatEvent::Left { thread, agent } => {
                self.members.insert((*thread, *agent), false);
                Told::Thread(*thread)
            }
            ChatEvent::Edited { message, .. } | ChatEvent::Deleted { message } => {
                let Some(mail) = self.messages.get_mut(message) else {
                    return Told::Nothing;
                };
                mail.apply_revision(&record.event);
                Told::Thread(self.subthread_of(*message))
            }
            ChatEvent::Named { thread, name } => {
                if !self.messages.contains_key(thread) {
                    return Told::Nothing;
                }
                let name = one_line(name);
                if name.is_empty() {
                    self.names.remove(thread);
                } else {
                    self.names.insert(*thread, name);
                }
                Told::Thread(*thread)
            }
        }
    }

    /// Applies the current ephemeral snapshot from the host, never from disk.
    pub fn set_statuses(&mut self, statuses: Vec<ThreadStatus>) {
        self.statuses = statuses
            .into_iter()
            .map(|status| (status.thread, status))
            .collect();
    }

    pub fn status(&self, thread: MessageId) -> Option<&ThreadStatus> {
        self.statuses.get(&thread)
    }

    pub fn statuses(&self) -> impl Iterator<Item = &ThreadStatus> {
        self.statuses.values()
    }

    /// One root channel by id, with the name used in a project address.
    pub fn project(&self, channel: MessageId) -> Option<crate::protocol::Project> {
        if !self.is_project(channel) {
            return None;
        }
        Some(crate::protocol::Project {
            channel,
            name: self.name(channel)?,
        })
    }

    /// A root that was itself addressed to the user is mail, not a project.
    pub fn is_project(&self, id: MessageId) -> bool {
        self.message(id).is_some_and(|root| {
            root.parent.is_none()
                && root.revision != Revision::Deleted
                && !matches!(
                    root.mentions(Address::User),
                    Some(MentionKind::Ask | MentionKind::Result)
                )
        })
    }

    pub fn message(&self, id: MessageId) -> Option<&Message> {
        self.messages.get(&id)
    }

    pub fn messages(&self) -> impl Iterator<Item = &Message> {
        self.messages.values()
    }

    /// The messages under `parent` in log order; the roots for `None`.
    pub fn children(&self, parent: Option<MessageId>) -> &[MessageId] {
        self.children
            .get(&parent)
            .map_or(&[], |children| children.as_slice())
    }

    pub fn roots(&self) -> &[MessageId] {
        self.children(None)
    }

    /// The thread a message belongs to: the message right under the root
    /// above it, which is what a screen opens and what a card is. A root
    /// is a stream and belongs to itself; a message right under a root
    /// starts a thread.
    pub fn thread_of(&self, id: MessageId) -> MessageId {
        let mut cursor = id;
        loop {
            let Some(parent) = self
                .messages
                .get(&cursor)
                .and_then(|message| message.parent)
            else {
                return cursor;
            };
            if self
                .messages
                .get(&parent)
                .is_none_or(|message| message.parent.is_none())
            {
                return cursor;
            }
            cursor = parent;
        }
    }

    /// The screen that reads a mail: itself for a root or project row,
    /// otherwise its parent, including an addressed root mail.
    pub fn subthread_of(&self, id: MessageId) -> MessageId {
        let Some(parent) = self.messages.get(&id).and_then(|message| message.parent) else {
            return id;
        };
        if self.is_project(parent) { id } else { parent }
    }

    /// The root above a message, or the message when it is one.
    pub fn root_of(&self, id: MessageId) -> MessageId {
        let mut cursor = id;
        while let Some(parent) = self
            .messages
            .get(&cursor)
            .and_then(|message| message.parent)
        {
            cursor = parent;
        }
        cursor
    }

    /// Whether `thread` is `message` or above it.
    fn is_above_or_self(&self, message: MessageId, thread: Option<MessageId>) -> bool {
        let mut cursor = thread;
        while let Some(at) = cursor {
            if at == message {
                return true;
            }
            cursor = self.messages.get(&at).and_then(|found| found.parent);
        }
        false
    }

    /// A thread's name: as named, else the first line of its message.
    pub fn name(&self, id: MessageId) -> Option<String> {
        if let Some(name) = self.names.get(&id) {
            return Some(name.clone());
        }
        self.messages.get(&id).map(Message::headline)
    }

    pub fn thread(&self, id: MessageId) -> Option<Thread> {
        self.messages.get(&id)?;
        let mut path = Vec::new();
        let mut cursor = Some(id);
        while let Some(at) = cursor {
            path.push((at, self.name(at).unwrap_or_default()));
            cursor = self.messages.get(&at).and_then(|message| message.parent);
        }
        path.reverse();
        Some(Thread {
            id,
            name: self.name(id).unwrap_or_default(),
            path,
            members: self.members_under(Some(id)),
        })
    }

    /// Whom a message under `parent` reaches by membership: for each agent
    /// with a record at `parent` or above, the nearest decides.
    pub fn members_under(&self, parent: Option<MessageId>) -> Vec<AgentId> {
        let mut decided: BTreeMap<AgentId, bool> = BTreeMap::new();
        let mut cursor = parent;
        while let Some(thread) = cursor {
            for ((_, agent), joined) in self
                .members
                .range((thread, AgentId::MIN)..=(thread, AgentId::MAX))
            {
                decided.entry(*agent).or_insert(*joined);
            }
            cursor = self
                .messages
                .get(&thread)
                .and_then(|message| message.parent);
        }
        decided
            .into_iter()
            .filter_map(|(agent, joined)| joined.then_some(agent))
            .collect()
    }

    /// Every agent that joined a thread, with the first it joined: its
    /// own thread.
    pub fn agent_threads(&self) -> impl Iterator<Item = (AgentId, MessageId)> + '_ {
        self.first_joined
            .iter()
            .map(|(agent, thread)| (*agent, *thread))
    }

    /// The thread an agent is the agent of: the card of the first thread
    /// it joined.
    pub fn agent_thread(&self, agent: AgentId) -> Option<MessageId> {
        self.first_joined
            .get(&agent)
            .map(|joined| self.thread_of(*joined))
    }

    /// The newest message in a thread or under it, the thread's own
    /// message included.
    pub fn newest_in(&self, thread: MessageId) -> Option<(MessageId, UnixMs)> {
        let mut newest = self
            .messages
            .get(&thread)
            .map(|message| (message.id, message.at));
        let mut stack = vec![thread];
        while let Some(at) = stack.pop() {
            for child in self.children(Some(at)) {
                if let Some(message) = self.messages.get(child) {
                    if newest.is_none_or(|(id, _)| message.id > id) {
                        newest = Some((message.id, message.at));
                    }
                    stack.push(*child);
                }
            }
        }
        newest
    }

    /// Every message in a thread or under it that is not the user's,
    /// newest first: what can be unread.
    pub fn others_in(&self, thread: MessageId) -> Vec<MessageId> {
        let mut found = Vec::new();
        let mut stack = vec![thread];
        while let Some(at) = stack.pop() {
            if let Some(message) = self.messages.get(&at)
                && message.from != Address::User
                && message.revision != Revision::Deleted
            {
                found.push(at);
            }
            stack.extend(self.children(Some(at)).iter().copied());
        }
        found.sort_unstable_by(|a, b| b.cmp(a));
        found
    }

    /// The root whose name is `name`, if there is one.
    pub fn root_named(&self, name: &str) -> Option<MessageId> {
        let wanted = one_line(name);
        self.roots().iter().rev().copied().find(|root| {
            self.project(*root)
                .is_some_and(|project| project.name == wanted)
        })
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{AgentIdDomain, ContentPart};

    use super::*;

    fn agent(n: u64) -> AgentId {
        AgentId::from_counter(n, &AgentIdDomain(0)).unwrap()
    }

    fn id(n: u64) -> MessageId {
        MessageId(ChatSeq(n))
    }

    fn record(seq: u64, event: ChatEvent) -> ChatRecord {
        ChatRecord {
            seq: ChatSeq(seq),
            at: UnixMs(seq),
            event,
        }
    }

    fn posted(seq: u64, parent: Option<u64>, from: Address, text: &str) -> ChatRecord {
        record(
            seq,
            ChatEvent::Posted {
                parent: parent.map(id),
                from,
                mentions: Vec::new(),
                content: vec![ContentPart::Text { text: text.into() }],
            },
        )
    }

    /// A forest told in order: roots, threads under them, replies, a
    /// subthread, members by the nearest record, names, and a move that
    /// takes the subtree.
    #[test]
    fn the_fold_keeps_the_forest() {
        let (a, b) = (agent(1), agent(2));
        let mut chat = HostChat::default();
        assert_eq!(
            chat.tell(&posted(1, None, Address::User, "rho")),
            Told::Posted {
                message: id(1),
                thread: id(1)
            }
        );
        assert_eq!(
            chat.tell(&posted(2, Some(1), Address::User, "Fix it\nmore")),
            Told::Posted {
                message: id(2),
                thread: id(2)
            }
        );
        assert_eq!(
            chat.tell(&record(
                3,
                ChatEvent::Joined {
                    thread: id(2),
                    agent: a
                }
            )),
            Told::Thread(id(2))
        );
        assert_eq!(
            chat.tell(&posted(4, Some(2), Address::Agent(a), "on it")),
            Told::Posted {
                message: id(4),
                thread: id(2)
            }
        );
        assert_eq!(
            chat.tell(&posted(5, Some(4), Address::User, "deeper")),
            Told::Posted {
                message: id(5),
                thread: id(2)
            }
        );
        assert_eq!(
            chat.tell(&record(
                6,
                ChatEvent::Joined {
                    thread: id(1),
                    agent: b
                }
            )),
            Told::Thread(id(1))
        );
        assert_eq!(
            chat.tell(&record(
                7,
                ChatEvent::Left {
                    thread: id(4),
                    agent: b
                }
            )),
            Told::Thread(id(4))
        );
        assert_eq!(
            chat.tell(&record(
                8,
                ChatEvent::Named {
                    thread: id(2),
                    name: "  Fixed  it ".into()
                }
            )),
            Told::Thread(id(2))
        );
        assert_eq!(
            chat.tell(&record(
                8,
                ChatEvent::Named {
                    thread: id(2),
                    name: "again".into()
                }
            )),
            Told::Nothing,
            "told once"
        );
        assert_eq!(chat.head, ChatSeq(8));

        assert_eq!(chat.roots(), [id(1)]);
        assert_eq!(chat.children(Some(id(2))), [id(4)]);
        assert_eq!(chat.thread_of(id(1)), id(1));
        assert_eq!(chat.thread_of(id(2)), id(2));
        assert_eq!(chat.thread_of(id(4)), id(2));
        assert_eq!(chat.thread_of(id(5)), id(2));
        assert_eq!(chat.subthread_of(id(4)), id(2));
        assert_eq!(chat.subthread_of(id(5)), id(4));
        assert_eq!(chat.root_of(id(5)), id(1));
        assert_eq!(chat.name(id(2)).as_deref(), Some("Fixed it"));
        assert_eq!(chat.name(id(4)).as_deref(), Some("on it"));
        let thread = chat.thread(id(4)).unwrap();
        assert_eq!(
            thread.path,
            vec![
                (id(1), "rho".into()),
                (id(2), "Fixed it".into()),
                (id(4), "on it".into())
            ]
        );
        assert_eq!(thread.members, [a], "b left this branch");
        assert_eq!(chat.members_under(Some(id(2))), [a, b]);
        assert_eq!(chat.members_under(Some(id(1))), [b]);
        assert_eq!(chat.newest_in(id(2)), Some((id(5), UnixMs(5))));
        assert_eq!(chat.others_in(id(2)), [id(4)]);
        assert_eq!(chat.root_named(" rho "), Some(id(1)));
        assert_eq!(chat.root_named("ops"), None);

        // Under itself or its own child: nothing. Elsewhere: the subtree goes.
        assert_eq!(
            chat.tell(&record(
                9,
                ChatEvent::Reparented {
                    message: id(2),
                    parent: Some(id(5))
                }
            )),
            Told::Nothing
        );
        chat.tell(&posted(10, None, Address::User, "ops"));
        assert_eq!(
            chat.tell(&record(
                11,
                ChatEvent::Reparented {
                    message: id(4),
                    parent: Some(id(10))
                }
            )),
            Told::Moved {
                message: id(4),
                from: Some(id(2)),
                to: Some(id(10))
            }
        );
        assert_eq!(chat.children(Some(id(2))), Vec::<MessageId>::new());
        assert_eq!(chat.children(Some(id(10))), [id(4)]);
        assert_eq!(chat.root_of(id(5)), id(10));
        assert_eq!(chat.thread(id(5)).unwrap().path[0], (id(10), "ops".into()));
        assert_eq!(chat.newest_in(id(2)), Some((id(2), UnixMs(2))));
        assert_eq!(
            chat.tell(&record(
                12,
                ChatEvent::Reparented {
                    message: id(4),
                    parent: Some(id(10))
                }
            )),
            Told::Nothing,
            "where it is"
        );
        assert_eq!(
            chat.tell(&record(
                13,
                ChatEvent::Reparented {
                    message: id(10),
                    parent: None
                }
            )),
            Told::Nothing,
            "a root stays a root"
        );
    }
}

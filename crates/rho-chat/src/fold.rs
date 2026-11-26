//! A host's log folded into its forest: every message, where each hangs,
//! what each thread is named, and who is a member of what.
//!
//! The fold is told each record once, oldest first, and answers what the
//! record changed ([`Told`]) so a screen refreshes only that. It is the
//! same fold whether the records come from the disk copy or the host.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rho_agent_types::{
    Address, AgentId, ChatEvent, ChatRecord, ChatSeq, Message, MessageId, ProjectConfig, ProjectId,
    Revision, ThreadWork, UnixMs, Voice, one_line,
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
    #[cfg(feature = "ui")]
    pub(crate) text: rho_frame::TextIndex<MessageId>,
    /// Each message's children in log order; `None` holds the roots.
    children: BTreeMap<Option<MessageId>, Vec<MessageId>>,
    names: HashMap<MessageId, String>,
    /// Each project's newest settings, by its anchor.
    projects: BTreeMap<MessageId, ProjectConfig>,
    work: BTreeMap<MessageId, ThreadWork>,
    /// What each agent last said about each thread: joined or left.
    members: BTreeMap<(MessageId, AgentId), bool>,
    /// The first thread each agent joined: its thread, the one it started
    /// from, where a stop it did not choose draws the user.
    first_joined: BTreeMap<AgentId, MessageId>,
    /// The thread each agent joined last: a standing agent joins each
    /// idea's thread as it takes it up.
    last_joined: BTreeMap<AgentId, MessageId>,
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
                #[cfg(feature = "ui")]
                self.refresh_text_tree(id, true);
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
                let siblings = self.children.entry(*parent).or_default();
                let slot = siblings.partition_point(|id| id < message);
                siblings.insert(slot, *message);
                self.messages.get_mut(message).expect("checked").parent = *parent;
                #[cfg(feature = "ui")]
                self.refresh_text_tree(*message, false);
                Told::Moved {
                    message: *message,
                    from,
                    to: *parent,
                }
            }
            ChatEvent::Project { project, config } => {
                if !self.messages.contains_key(&project.0) {
                    return Told::Nothing;
                }
                // A root that becomes a project comes into the catalogue
                // with what hangs under it.
                let was = self.projects.insert(project.0, config.clone());
                #[cfg(feature = "ui")]
                if was.is_none() {
                    self.refresh_text_tree(project.0, true);
                }
                #[cfg(not(feature = "ui"))]
                let _ = was;
                Told::Thread(project.0)
            }
            ChatEvent::Joined { thread, agent } => {
                self.members.insert((*thread, *agent), true);
                self.first_joined.entry(*agent).or_insert(*thread);
                self.last_joined.insert(*agent, *thread);
                Told::Thread(*thread)
            }
            ChatEvent::Left { thread, agent } => {
                self.members.insert((*thread, *agent), false);
                Told::Thread(*thread)
            }
            ChatEvent::Edited { message, .. }
            | ChatEvent::Revised { message, .. }
            | ChatEvent::TakenBack { message, .. }
            | ChatEvent::Recovered { message, .. }
            | ChatEvent::Deleted { message } => {
                #[cfg(feature = "ui")]
                let was_agent_only = self.agent_only(*message);
                let Some(mail) = self.messages.get_mut(message) else {
                    return Told::Nothing;
                };
                mail.apply_revision(record);
                #[cfg(feature = "ui")]
                if was_agent_only != self.agent_only(*message) && self.root_of(*message) == *message
                {
                    self.refresh_text_tree(*message, true);
                } else {
                    self.refresh_text(*message, true);
                }
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

    // Visibility depends on the containing root. A move or a root's audience
    // change can expose or hide descendants without changing their bodies.
    #[cfg(feature = "ui")]
    fn refresh_text_tree(&mut self, root: MessageId, changed: bool) {
        self.refresh_text(root, changed);
        let mut descendants = self.children(Some(root)).to_vec();
        while let Some(id) = descendants.pop() {
            self.refresh_text(id, false);
            descendants.extend_from_slice(self.children(Some(id)));
        }
    }

    #[cfg(feature = "ui")]
    fn refresh_text(&mut self, id: MessageId, changed: bool) {
        let visible = self.messages.get(&id).filter(|mail| {
            mail.revision != Revision::Deleted
                && !self.is_project(id)
                && !self.agent_only(id)
                && !self.agent_only(self.root_of(id))
        });
        match visible {
            Some(mail) if changed || !self.text.contains(&id) => {
                self.text
                    .update(id, crate::reading::body(&mail.content), mail.version().1.0)
            }
            None => self.text.remove(&id),
            _ => {}
        }
    }

    /// Applies the current ephemeral snapshot from the host, never from disk.
    pub fn set_work(&mut self, work: Vec<(MessageId, ThreadWork)>) {
        self.work = work.into_iter().collect();
    }

    /// First post order chooses a flash, even if an earlier one was edited.
    #[cfg(feature = "ui")]
    pub(crate) fn latest_flash(&self, thread: MessageId) -> Option<&Message> {
        self.reading_order(thread)
            .filter(|mail| mail.flash_deadline().is_some())
            .max_by_key(|mail| mail.id)
    }

    /// The status words and the latest time the project wrote to this
    /// thread. A new user idea can change the status to `working` without
    /// changing when the project last wrote.
    #[cfg(feature = "ui")]
    pub(crate) fn thread_activity(&self, thread: MessageId) -> (Option<String>, Option<UnixMs>) {
        let mut latest = None;
        let mut latest_user = None;
        let mut project_at = None;
        for mail in self.reading_order(thread).filter(|mail| {
            !matches!(
                mail.revision,
                Revision::Deleted | Revision::TakenBack { .. }
            )
        }) {
            if mail.from == Address::User {
                latest_user = Some(mail);
            } else if mail.to_user() {
                // The project's words: a message or a flash, never one
                // agent's words to another.
                project_at = project_at.max(Some(mail.version().1));
                if latest.is_none_or(|old: &Message| old.version().0 < mail.version().0) {
                    latest = Some(mail);
                }
            }
        }
        let flash = self.latest_flash(thread);
        let words = if let Some(flash) = flash {
            Some(flash.headline())
        } else if self.work.get(&thread) == Some(&ThreadWork::Working)
            && latest_user
                .is_some_and(|user| latest.is_none_or(|mail| mail.version().0 < user.id.0))
        {
            Some("working".into())
        } else {
            latest.map(Message::headline)
        };
        (words, project_at)
    }

    /// The opening mail and its immediate replies are shown in a thread.
    pub(crate) fn reading_order(
        &self,
        thread: MessageId,
    ) -> impl Iterator<Item = &Message> + Clone {
        std::iter::once(thread)
            .chain(self.children(Some(thread)).iter().copied())
            .filter_map(|id| self.message(id))
    }

    pub fn work(&self, thread: MessageId) -> Option<ThreadWork> {
        self.work.get(&thread).copied()
    }

    /// A project by its anchor, with the name used in its address.
    pub fn project(&self, root: MessageId) -> Option<crate::protocol::Project> {
        if !self.is_project(root) {
            return None;
        }
        Some(crate::protocol::Project {
            id: ProjectId(root),
            name: self.name(root)?,
            config: self.projects.get(&root)?.clone(),
        })
    }

    /// Every live project, oldest first.
    pub fn projects(&self) -> impl Iterator<Item = crate::protocol::Project> + '_ {
        self.projects.keys().filter_map(|root| self.project(*root))
    }

    /// A root the log made a project, and did not delete.
    pub fn is_project(&self, id: MessageId) -> bool {
        self.projects.contains_key(&id)
            && self
                .message(id)
                .is_some_and(|root| root.parent.is_none() && root.revision != Revision::Deleted)
    }

    /// What the user does not see in the catalogue: one agent's words to
    /// another, and any root that is not a project, such as the container
    /// of agents that work outside every project.
    pub fn agent_only(&self, id: MessageId) -> bool {
        let Some(mail) = self.message(id) else {
            return false;
        };
        matches!(mail.voice, Voice::ToAgent(_)) || (mail.parent.is_none() && !self.is_project(id))
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

    /// Who works anywhere under this project, including in its threads.
    pub fn members_in_project(&self, project: MessageId) -> BTreeSet<AgentId> {
        self.members
            .iter()
            .filter_map(|(&(thread, agent), &joined)| {
                (joined && self.root_of(thread) == project).then_some(agent)
            })
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

    /// The thread an agent works in now: the one it joined last, read as
    /// the thread its words show in. An agent another agent made joined
    /// its brief, which hangs in the thread of the work it helps.
    pub fn working_thread(&self, agent: AgentId) -> Option<MessageId> {
        self.last_joined
            .get(&agent)
            .map(|joined| self.thread_of(*joined))
    }

    /// The newest words in a thread or under it, the thread's own mail
    /// included.
    pub fn newest_in(&self, thread: MessageId) -> Option<(ChatSeq, UnixMs)> {
        let mut newest = self.message(thread).map(Message::version);
        let mut stack = vec![thread];
        while let Some(at) = stack.pop() {
            for &child in self.children(Some(at)) {
                if let Some(message) = self.message(child) {
                    newest = newest.max(Some(message.version()));
                    stack.push(child);
                }
            }
        }
        newest
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

    #[cfg(feature = "ui")]
    #[test]
    fn thread_row_prefers_the_newest_flash_then_falls_back_after_its_tombstone() {
        let mut log = HostChat::default();
        log.tell(&posted(1, None, Address::User, "project"));
        make_project(&mut log, 1);
        log.tell(&posted(2, Some(1), Address::User, "idea"));
        let project_words = |seq, words, voice| {
            voiced(posted(seq, Some(2), Address::Agent(agent(3)), words), voice)
        };
        log.tell(&project_words(3, "First update", Voice::Flash));
        log.tell(&project_words(4, "Second update", Voice::Flash));
        log.tell(&project_words(5, "A later kept result", Voice::Message));
        log.tell(&record(
            6,
            ChatEvent::Edited {
                message: id(3),
                content: vec![ContentPart::Text {
                    text: "First update is edited".into(),
                }],
            },
        ));
        assert_eq!(log.latest_flash(id(2)).unwrap().id, id(4));
        assert_eq!(
            log.thread_activity(id(2)).0.as_deref(),
            Some("Second update")
        );
        log.tell(&record(7, ChatEvent::Deleted { message: id(4) }));
        assert_eq!(
            log.thread_activity(id(2)).0.as_deref(),
            Some("First update is edited")
        );
        log.tell(&record(8, ChatEvent::Deleted { message: id(3) }));
        assert!(log.latest_flash(id(2)).is_none());
        assert_eq!(
            log.thread_activity(id(2)).0.as_deref(),
            Some("A later kept result")
        );
    }

    #[cfg(feature = "ui")]
    #[test]
    fn text_projection_follows_edits_deletion_and_moves_into_or_out_of_private_roots() {
        let mut log = HostChat::default();
        log.tell(&posted(1, None, Address::User, "visible project"));
        make_project(&mut log, 1);
        log.tell(&posted(
            2,
            Some(1),
            Address::Agent(agent(3)),
            "Clock report",
        ));
        log.tell(&posted(3, Some(2), Address::User, "loose winding"));
        log.tell(&voiced(
            posted(4, None, Address::Agent(agent(3)), "private root"),
            Voice::ToAgent(agent(7)),
        ));
        assert_eq!(log.text.body(&id(1)), None);
        assert_eq!(log.text.body(&id(2)), Some("Clock report"));
        assert_eq!(log.text.body(&id(3)), Some("loose winding"));
        assert_eq!(log.text.body(&id(4)), None);

        log.tell(&record(
            5,
            ChatEvent::Reparented {
                message: id(2),
                parent: Some(id(4)),
            },
        ));
        assert!(!log.text.contains(&id(2)));
        assert!(!log.text.contains(&id(3)));
        log.tell(&record(
            6,
            ChatEvent::Edited {
                message: id(3),
                content: vec![ContentPart::Text {
                    text: "wind restored".into(),
                }],
            },
        ));
        assert!(!log.text.contains(&id(3)));
        log.tell(&record(
            7,
            ChatEvent::Reparented {
                message: id(2),
                parent: Some(id(1)),
            },
        ));
        assert_eq!(log.text.body(&id(3)), Some("wind restored"));
        log.tell(&record(8, ChatEvent::Deleted { message: id(3) }));
        assert!(!log.text.contains(&id(3)));
    }

    #[cfg(feature = "ui")]
    #[test]
    fn a_human_idea_is_catalogued_but_one_agents_words_to_another_are_not() {
        let mut log = HostChat::default();
        log.tell(&posted(1, None, Address::User, "tide"));
        make_project(&mut log, 1);
        log.tell(&posted(2, Some(1), Address::User, "Measure clock jitter."));
        log.tell(&posted(
            3,
            Some(1),
            Address::User,
            "Inspect tide B for drift.",
        ));
        log.tell(&voiced(
            posted(
                4,
                Some(1),
                Address::Agent(agent(3)),
                "Private clock comparison.",
            ),
            Voice::ToAgent(agent(7)),
        ));
        assert_eq!(log.text.body(&id(2)), Some("Measure clock jitter."));
        assert!(
            !log.agent_only(id(3)),
            "the subject must be findable by name"
        );
        assert_eq!(log.text.body(&id(3)), Some("Inspect tide B for drift."));
        assert!(log.agent_only(id(4)));
        assert_eq!(log.text.body(&id(4)), None);
    }

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
                retry_of: None,
                thread_reply: None,
                from,
                voice: Voice::Message,
                content: vec![ContentPart::Text { text: text.into() }],
            },
        )
    }

    fn voiced(mut record: ChatRecord, voice: Voice) -> ChatRecord {
        let ChatEvent::Posted { voice: told, .. } = &mut record.event else {
            unreachable!("only a post has a voice")
        };
        *told = voice;
        record
    }

    fn config() -> ProjectConfig {
        ProjectConfig {
            place: rho_agent_types::ProjectPlace::parse("~/src/tide").unwrap(),
            mode: rho_agent_types::ProjectMode::One,
            runtime: rho_agent_types::RuntimeLabel::default(),
        }
    }

    /// Makes `root` a project the way a host log does, but with no seq of
    /// its own, so a test keeps message ids equal to seqs.
    fn make_project(log: &mut HostChat, root: u64) {
        log.projects.insert(id(root), config());
    }

    #[test]
    fn a_root_is_a_project_only_once_the_host_says_so() {
        let mut mail = HostChat::default();
        mail.tell(&posted(1, None, Address::User, "Restart fixture"));
        mail.tell(&posted(2, Some(1), Address::Agent(agent(1)), "Restarted"));
        mail.tell(&posted(3, None, Address::User, "Calendar repairs"));
        mail.tell(&posted(4, Some(3), Address::User, "Fix the leap day"));
        assert!(mail.agent_only(id(3)));
        assert_eq!(mail.root_named("Calendar repairs"), None);
        #[cfg(feature = "ui")]
        assert_eq!(mail.text.body(&id(4)), None);

        assert_eq!(
            mail.tell(&record(
                5,
                ChatEvent::Project {
                    project: ProjectId(id(3)),
                    config: config(),
                }
            )),
            Told::Thread(id(3))
        );
        assert!(!mail.is_project(id(1)));
        assert!(mail.agent_only(id(1)));
        assert!(mail.root_named("Restart fixture").is_none());
        assert_eq!(mail.root_named("Calendar repairs"), Some(id(3)));
        assert!(!mail.agent_only(id(3)));
        assert_eq!(
            mail.project(id(3)).map(|project| project.config),
            Some(config())
        );
        #[cfg(feature = "ui")]
        assert_eq!(mail.text.body(&id(4)), Some("Fix the leap day"));

        // A project event for a message the fold does not know changes
        // nothing.
        assert_eq!(
            mail.tell(&record(
                6,
                ChatEvent::Project {
                    project: ProjectId(id(40)),
                    config: config(),
                }
            )),
            Told::Nothing
        );
        assert!(!mail.is_project(id(40)));
    }

    #[test]
    fn project_members_include_threads_but_not_another_project_or_left_members() {
        let mut mail = HostChat::default();
        for (seq, parent) in [
            (1, None),
            (2, None),
            (3, Some(1)),
            (4, Some(2)),
            (5, Some(3)),
        ] {
            mail.tell(&posted(seq, parent, Address::User, "work"));
        }
        let (a, b, c) = (agent(1), agent(2), agent(3));
        for (seq, thread, agent) in [(6, 1, a), (7, 3, a), (8, 5, b), (9, 3, c), (10, 4, c)] {
            mail.tell(&record(
                seq,
                ChatEvent::Joined {
                    thread: id(thread),
                    agent,
                },
            ));
        }
        mail.tell(&record(
            11,
            ChatEvent::Left {
                thread: id(3),
                agent: c,
            },
        ));
        assert_eq!(mail.members_in_project(id(1)), BTreeSet::from([a, b]));
        assert_eq!(mail.members_in_project(id(2)), BTreeSet::from([c]));
    }

    /// A forest told in order: roots, threads under them, replies, a
    /// subthread, members by the nearest record, names, and a move that
    /// takes the subtree.
    #[test]
    fn the_fold_keeps_the_forest() {
        let (a, b) = (agent(1), agent(2));
        let mut mail = HostChat::default();
        assert_eq!(
            mail.tell(&posted(1, None, Address::User, "rho")),
            Told::Posted {
                message: id(1),
                thread: id(1)
            }
        );
        make_project(&mut mail, 1);
        assert_eq!(
            mail.tell(&posted(2, Some(1), Address::User, "Fix it\nmore")),
            Told::Posted {
                message: id(2),
                thread: id(2)
            }
        );
        assert_eq!(
            mail.tell(&record(
                3,
                ChatEvent::Joined {
                    thread: id(2),
                    agent: a
                }
            )),
            Told::Thread(id(2))
        );
        assert_eq!(
            mail.tell(&posted(4, Some(2), Address::Agent(a), "on it")),
            Told::Posted {
                message: id(4),
                thread: id(2)
            }
        );
        assert_eq!(
            mail.tell(&posted(5, Some(4), Address::User, "deeper")),
            Told::Posted {
                message: id(5),
                thread: id(2)
            }
        );
        assert_eq!(
            mail.tell(&record(
                6,
                ChatEvent::Joined {
                    thread: id(1),
                    agent: b
                }
            )),
            Told::Thread(id(1))
        );
        assert_eq!(
            mail.tell(&record(
                7,
                ChatEvent::Left {
                    thread: id(4),
                    agent: b
                }
            )),
            Told::Thread(id(4))
        );
        assert_eq!(
            mail.tell(&record(
                8,
                ChatEvent::Named {
                    thread: id(2),
                    name: "  Fixed  it ".into()
                }
            )),
            Told::Thread(id(2))
        );
        assert_eq!(
            mail.tell(&record(
                8,
                ChatEvent::Named {
                    thread: id(2),
                    name: "again".into()
                }
            )),
            Told::Nothing,
            "told once"
        );
        assert_eq!(mail.head, ChatSeq(8));

        assert_eq!(mail.roots(), [id(1)]);
        assert_eq!(mail.children(Some(id(2))), [id(4)]);
        assert_eq!(mail.thread_of(id(1)), id(1));
        assert_eq!(mail.thread_of(id(2)), id(2));
        assert_eq!(mail.thread_of(id(4)), id(2));
        assert_eq!(mail.thread_of(id(5)), id(2));
        assert_eq!(mail.subthread_of(id(4)), id(2));
        assert_eq!(mail.subthread_of(id(5)), id(4));
        assert_eq!(mail.root_of(id(5)), id(1));
        assert_eq!(mail.name(id(2)).as_deref(), Some("Fixed it"));
        assert_eq!(mail.name(id(4)).as_deref(), Some("on it"));
        let thread = mail.thread(id(4)).unwrap();
        assert_eq!(
            thread.path,
            vec![
                (id(1), "rho".into()),
                (id(2), "Fixed it".into()),
                (id(4), "on it".into())
            ]
        );
        assert_eq!(thread.members, [a], "b left this branch");
        assert_eq!(mail.members_under(Some(id(2))), [a, b]);
        assert_eq!(mail.members_under(Some(id(1))), [b]);
        assert_eq!(mail.newest_in(id(2)), Some((id(5).0, UnixMs(5))));
        assert_eq!(mail.root_named(" rho "), Some(id(1)));
        assert_eq!(mail.root_named("ops"), None);

        // Under itself or its own child: nothing. Elsewhere: the subtree goes.
        assert_eq!(
            mail.tell(&record(
                9,
                ChatEvent::Reparented {
                    message: id(2),
                    parent: Some(id(5))
                }
            )),
            Told::Nothing
        );
        mail.tell(&posted(10, None, Address::User, "ops"));
        assert_eq!(
            mail.tell(&record(
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
        assert_eq!(mail.children(Some(id(2))), Vec::<MessageId>::new());
        assert_eq!(mail.children(Some(id(10))), [id(4)]);
        assert_eq!(mail.root_of(id(5)), id(10));
        assert_eq!(mail.thread(id(5)).unwrap().path[0], (id(10), "ops".into()));
        assert_eq!(mail.newest_in(id(2)), Some((id(2).0, UnixMs(2))));
        assert_eq!(
            mail.tell(&record(
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
            mail.tell(&record(
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

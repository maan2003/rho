//! Mail as the frame shows it.
//!
//! [`ChatSystem`] owns each host's forest, disk copy, outbox, and session.
//! It plugs into the frame as a [`System`] under the `thread` kind: a
//! root anchors a project, and each reply directly below it starts a
//! thread. Both open as a [`ThreadView`]. The idea being written opens
//! at `chat:idea` ([`idea`]). [`crate::cards`] says which threads want the
//! user; this module supplies projects, moving threads, and the places
//! `Places` lists.

mod git;
mod idea;
mod pending;
#[cfg(test)]
mod private_tests;
mod projects;
#[cfg(test)]
mod search_bench;
mod thread;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Task, Window};
use idea::{Idea, IdeaMail, IdeaProject};
use jiff::Timestamp;
use pending::{PendingIdea, PendingIdeaId};
use projects::ProjectCreated;
use rho_agent_hosts::{HostId, HostStream, Link};
use rho_agent_types::{
    Address as Party, AgentId, ChatSeq, ContentPart, Doing, EndpointId, Message, MessageId,
    MessageVersion, ProjectId, ThreadWork, UnixMs,
};
use rho_dealer::rank::Trace;
use rho_dealer::{Address, Addressed as _, Claim, Marks};
use rho_frame::{
    AnySurface, Candidate, Command, Found, FoundText, FoundThread, Frame, FrameLink, Opening,
    Point, Project, Row, SearchPoint, Spelling, System, TextIndex, TextKind, TextPresenter,
    TextSearch, TextTarget,
};
pub use rho_notes::sheet;
use rho_rpc::protocol::Answer as RpcAnswer;
use rho_window::style::StyleClass;
pub use thread::ThreadView;

use crate::cache::Cache;
pub use crate::cache::{MailAudience, WrittenMail};
use crate::fold::HostChat;
use crate::names::{self, ProjectNames};
use crate::outbox::{Delivery, Kept, Outbox, Outgoing, OutgoingState};
use crate::protocol::{Name, Post, PostIdea, PostIdeaReply, RetryIdea};
use crate::stream::{Cursors, MailEvent, MailStream};
use crate::{ThreadId, cards};

pub static RENAME_THREAD: Spelling = Spelling {
    name: "rename thread",
    aliases: &[
        ("rename", 9),
        ("title", 8),
        ("subject", 8),
        ("name thread", 8),
    ],
    argument: Some("subject"),
};

/// How the chat's commands are spelled, for ranking every system's
/// together.
pub static SPELLINGS: &[&Spelling] = &[
    &RENAME_THREAD,
    &projects::NEW_PROJECT,
    &projects::SET_PROJECT_PLACE,
    &projects::SET_PROJECT_MODE,
    &projects::SET_PROJECT_RUNTIME,
    &projects::RENAME_PROJECT,
    &projects::RETIRE_PROJECT,
    &pending::WRITE_IDEA_AGAIN,
    &thread::REPLY_PRIVATELY,
    &thread::QUOTE_MESSAGE,
    &thread::WRITE_REPLY_AGAIN,
    &thread::SEND_MESSAGE,
    &thread::OPEN_UNDER_CURSOR,
    &thread::FORK_THREAD,
];

/// What the chat tells its listeners.
pub enum Event {
    /// The thread's draft was queued by a frame action.
    DraftSent(ThreadId),
    /// The user opened the transcript from one of its mails.
    Inspect { mail: Message, agent: AgentId },
    /// A host's forest changed.
    Changed { endpoint: EndpointId },
}

/// One attached host: what the user calls it, how to call it, and whose
/// log it carries once its session has said.
struct Slot {
    name: String,
    link: Link,
    endpoint: Option<EndpointId>,
}

/// An agent of a thread as the agents system describes it to mail: what to
/// call it, and what it is doing as its host last said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Participant {
    pub label: String,
    /// Its short code, `eng-q9tz`: what tells two of a thread's agents
    /// apart.
    pub code: String,
    pub runtime: Option<String>,
    pub created_at: Option<rho_agent_types::UnixMs>,
    /// `None` while its host has said nothing of it.
    pub doing: Option<Doing>,
    /// Why it stopped, in its host's words, while a failure stopped it.
    pub stopped_because: Option<String>,
    /// The agent that made it, for one another agent made: the user meets
    /// its work in the thread of the work it helps.
    pub parent: Option<AgentId>,
}

/// What mail knows of an agent of the host with this identity, and `None`
/// once it is gone; the agents system owns the answer.
pub type Participants = Rc<dyn Fn(EndpointId, AgentId, &App) -> Option<Participant>>;

enum PostAnswer {
    Posted(MessageId),
    IdeaRefused(String),
    ReplyRefused(String),
}

impl PostAnswer {
    fn mail(answer: RpcAnswer<MessageId>) -> Self {
        match answer {
            RpcAnswer::Done(message) => Self::Posted(message),
            RpcAnswer::Failed { reason } => Self::ReplyRefused(reason),
        }
    }
}

pub struct ChatSystem {
    notes: Entity<rho_notes::NoteSystem>,
    cache: Cache,
    hosts: HashMap<HostId, Slot>,
    /// Each log's forest, by the identity of the host that keeps it.
    logs: HashMap<EndpointId, HostChat>,
    cursors: Cursors,
    outbox: Outbox,
    /// Each project being created, and the idea waiting to address it.
    creating_projects: Vec<(ThreadId, Option<ProjectCreated>)>,
    /// The runtimes each host offers, as its last session said.
    runtimes: HashMap<EndpointId, rho_agent_types::RuntimeCatalogue>,
    known_places: projects::KnownPlaces,
    last_project: Option<ThreadId>,
    drafts: TextIndex<(EndpointId, Option<MessageId>)>,
    /// The idea being written on this device.
    idea: Idea,
    events: UnboundedSender<MailEvent>,
    frame: FrameLink,
    /// Whether an agent still exists on a host, its label, and what it is
    /// doing; the agents system knows.
    participants: Participants,
    /// The pushes and fetches that wait for the user's word, each shown in
    /// the thread its agent works in.
    git: git::GitAsks,
    /// Where the reader left each thread whose screen has closed, for the
    /// next open (`chat.Thread.open`).
    left: HashMap<ThreadId, thread::Left>,
    _pump: Task<()>,
    _git_pump: Task<()>,
}

impl EventEmitter<Event> for ChatSystem {}

impl ChatSystem {
    pub fn new(
        db: rho_db::RhoDb,
        frame: FrameLink,
        notes: Entity<rho_notes::NoteSystem>,
        cx: &mut Context<Self>,
    ) -> Self {
        thread::bind(cx);
        idea::bind(cx);
        // The window offers the idea key everywhere; chat answers it.
        let this = cx.weak_entity();
        App::on_action(cx, move |_: &rho_frame::keys::PromptIdea, cx| {
            if let Some(chat) = this.upgrade() {
                idea::prompt_later(chat, cx);
            }
        });
        let cache = Cache::open_on(db).expect("the chat copy opens");
        let loaded = cache.load();
        let mut logs = HashMap::new();
        for (seed, _) in &loaded.cursors {
            let mut chat = HostChat::default();
            for record in cache.read_log(*seed) {
                chat.tell(&record);
            }
            logs.insert(*seed, chat);
        }
        let outbox = Outbox::load(
            loaded
                .outbox
                .into_iter()
                .map(|(host, client_id, mut outgoing)| {
                    outgoing.import_addressed(|message| {
                        logs.get(&host)
                            .and_then(|chat| chat.message(message))
                            .map_or(message.0, |mail| mail.version().0)
                    });
                    (host, client_id, outgoing)
                })
                .collect(),
        );
        let idea = Idea::load(loaded.idea, cx);
        let mut drafts = TextIndex::default();
        for (seed, parent, text) in loaded.drafts {
            drafts.update((seed, parent), text, 0);
        }
        let (events, mut incoming) = unbounded::<MailEvent>();
        let pump = cx.spawn(async move |this, cx| {
            while let Some(event) = incoming.next().await {
                if this
                    .update(cx, |this, cx| this.mail_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let (git_streams, mut git_events) = rho_agent_hosts::git::GitStreams::new();
        let git_pump = cx.spawn(async move |this, cx| {
            while let Some(event) = git_events.next().await {
                if this
                    .update(cx, |this, cx| this.git_heard(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            notes,
            cursors: Cursors::new(loaded.cursors),
            outbox,
            creating_projects: Vec::new(),
            runtimes: HashMap::new(),
            known_places: Rc::new(|_, _| Vec::new()),
            last_project: loaded.last_project,
            drafts,
            idea,
            cache,
            hosts: HashMap::new(),
            logs,
            events,
            frame,
            participants: Rc::new(|_, agent, _| {
                Some(Participant {
                    label: agent.encoded(),
                    code: agent.encoded(),
                    runtime: None,
                    created_at: None,
                    doing: None,
                    stopped_because: None,
                    parent: None,
                })
            }),
            git: git::GitAsks::new(git_streams),
            left: HashMap::new(),
            _pump: pump,
            _git_pump: git_pump,
        }
    }

    /// Tells mail which agents still exist and how their transcripts are named.
    pub fn set_participants(&mut self, participants: Participants) {
        self.participants = participants;
    }

    /// An agent of the host with `endpoint`, `None` once it is gone.
    pub fn participant(
        &self,
        endpoint: EndpointId,
        agent: AgentId,
        cx: &App,
    ) -> Option<Participant> {
        (self.participants)(endpoint, agent, cx)
    }

    /// The label of an agent of the host with `endpoint`, `None` once
    /// it is gone.
    pub fn agent_label(&self, endpoint: EndpointId, agent: AgentId, cx: &App) -> Option<String> {
        self.participant(endpoint, agent, cx)
            .map(|participant| participant.label)
    }

    pub fn work(&self, thread: ThreadId) -> Option<ThreadWork> {
        self.host_chat(thread.host)?.work(thread.message)
    }

    /// A mail thread's subject and project address for a row elsewhere.
    pub fn thread_name(&self, thread: ThreadId, cx: &App) -> Option<(String, String)> {
        let names = self.project_names();
        self.with_host(thread.host, &names, cx, |host| {
            cards::name(host, thread.message)
        })?
    }

    /// Which project names two held hosts share, as their logs say now.
    fn project_names(&self) -> ProjectNames {
        ProjectNames::new(self.logs.values())
    }

    /// A project's address as the window shows it, on an attached host.
    pub fn project_address(&self, channel: ThreadId) -> Option<String> {
        self.project_address_in(&self.project_names(), channel)
    }

    fn project_address_in(&self, names: &ProjectNames, channel: ThreadId) -> Option<String> {
        let alias = self.attached_alias(channel.host)?;
        self.host_chat(channel.host)?
            .project(channel.message)
            .map(|project| names.address(&project.name, alias))
    }

    /// The project an address names: `tide@lab` on the host this device
    /// calls `lab`, or a bare `tide` while one host alone has a project of
    /// that name.
    pub fn project_named(&self, address: &str) -> Option<ThreadId> {
        let (name, alias) = names::split(address.trim());
        if alias.is_none() && self.project_names().is_shared(name) {
            return None;
        }
        let mut found = self.hosts.values().filter_map(|slot| {
            if alias.is_some_and(|alias| !slot.name.eq_ignore_ascii_case(alias)) {
                return None;
            }
            let host = slot.endpoint?;
            let message = self.host_chat(host)?.root_named(name)?;
            Some(ThreadId { host, message })
        });
        let project = found.next()?;
        found.next().is_none().then_some(project)
    }

    pub fn host_chat(&self, endpoint: EndpointId) -> Option<&HostChat> {
        self.logs.get(&endpoint)
    }

    pub fn written_mail(
        &self,
        endpoint: EndpointId,
        writes: &HashSet<(MessageId, Option<ChatSeq>)>,
    ) -> HashMap<(MessageId, Option<ChatSeq>), WrittenMail> {
        use rho_agent_types::Revision;
        let Some(log) = self.host_chat(endpoint) else {
            return HashMap::new();
        };
        let mut result = HashMap::new();
        let mut historical = HashSet::new();
        for &(id, version) in writes {
            let Some(mail) = log.message(id) else {
                continue;
            };
            let current = match mail.revision {
                Revision::Original => version.unwrap_or(id.0) == id.0,
                Revision::Edited { seq, .. } | Revision::Recovered { seq, .. } => {
                    version == Some(seq)
                }
                Revision::TakenBack { .. } | Revision::Deleted => false,
            };
            if current {
                result.insert(
                    (id, version),
                    WrittenMail::from_content(mail.from, mail.voice, &mail.content),
                );
            } else if let Some(version) = version {
                historical.insert((id, version));
            }
        }
        result.extend(
            self.cache
                .written_mail(endpoint, &historical)
                .into_iter()
                .map(|((id, version), written)| ((id, Some(version)), written)),
        );
        result
    }

    /// The hosts whose logs are held, with their names: those attached
    /// under the name the user gave them, the rest by identity.
    pub fn hosts(&self) -> Vec<(EndpointId, String)> {
        let mut hosts: Vec<(EndpointId, String)> = self
            .logs
            .keys()
            .map(|seed| (*seed, self.host_name(*seed)))
            .collect();
        hosts.sort_by(|a, b| a.1.cmp(&b.1));
        hosts
    }

    fn attached_alias(&self, endpoint: EndpointId) -> Option<&str> {
        self.hosts
            .values()
            .find(|slot| slot.endpoint == Some(endpoint))
            .map(|slot| slot.name.as_str())
    }

    pub fn host_name(&self, endpoint: EndpointId) -> String {
        self.attached_alias(endpoint)
            .map_or_else(|| endpoint.to_string(), str::to_owned)
    }

    fn project_of(&self, thread: ThreadId, cx: &App) -> String {
        let names = self.project_names();
        self.with_host(thread.host, &names, cx, |host| {
            cards::context(host, thread.message)
        })
        .unwrap_or_else(|| self.host_name(thread.host))
    }

    pub fn draft(&self, endpoint: EndpointId, parent: Option<MessageId>) -> &str {
        self.drafts.body(&(endpoint, parent)).unwrap_or("")
    }

    /// `fork thread` on `tangent`, a message: its thread's draft quotes it,
    /// unless the user has already written there.
    pub(super) fn start_tangent(&mut self, tangent: ThreadId) {
        if !self.draft(tangent.host, Some(tangent.message)).is_empty() {
            return;
        }
        let Some(mail) = self
            .host_chat(tangent.host)
            .and_then(|chat| chat.message(tangent.message))
        else {
            return;
        };
        let quote = thread::quote_lines(crate::reading::body(&mail.content).trim_end());
        self.set_draft(tangent.host, Some(tangent.message), quote);
    }

    pub fn set_draft(&mut self, endpoint: EndpointId, parent: Option<MessageId>, text: String) {
        self.cache.write_draft(endpoint, parent, text.clone());
        if text.is_empty() {
            self.drafts.remove(&(endpoint, parent));
        } else {
            self.drafts.update((endpoint, parent), text, 0);
        }
    }

    /// What the user wrote under `parent` that is still local.
    pub fn local_under(
        &self,
        endpoint: EndpointId,
        parent: Option<MessageId>,
    ) -> Vec<&crate::outbox::Outgoing> {
        self.outbox.local_under(endpoint, parent)
    }

    fn mail_event(&mut self, event: MailEvent, cx: &mut Context<Self>) {
        match event {
            MailEvent::Hello {
                host,
                endpoint,
                fresh,
                runtimes,
                ..
            } => {
                if let Some(slot) = self.hosts.get_mut(&host) {
                    slot.endpoint = Some(endpoint);
                }
                self.runtimes.insert(endpoint, runtimes);
                if fresh {
                    self.cache.reset(endpoint);
                    self.logs.insert(endpoint, HostChat::default());
                }
                self.logs.entry(endpoint).or_default();
                self.send_unsent(endpoint, cx);
                self.changed(endpoint, cx);
            }
            MailEvent::Records { host, records } => {
                let Some(endpoint) = self.hosts.get(&host).and_then(|slot| slot.endpoint) else {
                    return;
                };
                let chat = self.logs.entry(endpoint).or_default();
                for record in &records {
                    chat.tell(record);
                }
                if let Some(last) = records.last() {
                    self.cursors.advance(endpoint, last.seq);
                }
                self.cache.write_records(endpoint, records);
                self.finish_posted(endpoint, cx);
                self.finish_project_creation(cx);
                self.changed(endpoint, cx);
            }
            MailEvent::Work { host, threads } => {
                if let Some(endpoint) = self.hosts.get(&host).and_then(|slot| slot.endpoint) {
                    self.logs.entry(endpoint).or_default().set_work(threads);
                    self.changed(endpoint, cx);
                }
            }
            MailEvent::Closed { host } => {
                if let Some(endpoint) = self.hosts.get(&host).and_then(|slot| slot.endpoint)
                    && let Some(chat) = self.logs.get_mut(&endpoint)
                {
                    chat.set_work(Vec::new());
                    self.changed(endpoint, cx);
                }
            }
        }
    }

    /// A post receipt and the matching log entry can arrive in either
    /// order. Act on the address only once this host's mail log contains the
    /// root.
    fn finish_project_creation(&mut self, cx: &mut Context<Self>) {
        let mut pending = std::mem::take(&mut self.creating_projects);
        pending.retain(|(creation, created)| {
            if self.project_config(*creation).is_some()
                && let Some(project) = self.idea_project_at(*creation)
            {
                let echo = format!("created project: {}", project.name);
                if let Some(created) = created {
                    created(project, cx);
                }
                self.frame.echo(echo, StyleClass::SystemInfo, cx);
                false
            } else {
                true
            }
        });
        self.creating_projects = pending;
    }

    fn changed(&mut self, endpoint: EndpointId, cx: &mut Context<Self>) {
        cx.emit(Event::Changed { endpoint });
        cx.notify();
        self.frame.changed(cx);
    }

    /// Write to the thread. The deal's observation is not an address.
    pub fn reply(
        &mut self,
        thread: ThreadId,
        settles: Option<MessageVersion>,
        text: String,
        cx: &mut Context<Self>,
    ) -> u64 {
        // Retry is the one exceptional route. A failure can be replied to
        // by name too; use it only when it is still the last kept item.
        let failed_start = self.host_chat(thread.host).and_then(|chat| {
            let last = match settles {
                Some(dealt) => chat.message(dealt.message),
                None => chat
                    .reading_order(thread.message)
                    .filter(|mail| {
                        mail.voice == rho_agent_types::Voice::Message
                            && mail.revision != rho_agent_types::Revision::Deleted
                    })
                    .last(),
            };
            last.filter(|mail| mail.from == Party::StartFailed)
                .map(|mail| mail.id)
        });
        self.queue_thread_post(
            thread,
            text,
            if failed_start.is_some() {
                Delivery::Retry
            } else {
                Delivery::Mail
            },
            settles,
            failed_start,
            cx,
        )
    }

    fn queue_thread_post(
        &mut self,
        thread: ThreadId,
        text: String,
        delivery: Delivery,
        settles: Option<MessageVersion>,
        failed_start: Option<MessageId>,
        cx: &mut Context<Self>,
    ) -> u64 {
        let parent = Some(thread.message);
        let (id, kept) = self.outbox.keep(
            thread.host,
            rand::random(),
            Outgoing {
                at: UnixMs::now(),
                parent,
                failed_start,
                settles,
                content: vec![ContentPart::Text { text }],
                delivery,
                runtime: None,
                mode: None,
                state: OutgoingState::Queued,
            },
        );
        self.keep_outgoing(thread.host, parent, kept, cx);
        id
    }

    fn keep_outgoing(
        &mut self,
        endpoint: EndpointId,
        parent: Option<MessageId>,
        kept: Kept,
        cx: &mut Context<Self>,
    ) {
        let project = parent.and_then(|message| {
            self.project_root(ThreadId {
                host: endpoint,
                message,
            })
        });
        if let Some(project) = project {
            self.last_project = Some(project);
        }
        self.frame.changed(cx);
        self.cache
            .write_outbox(endpoint, kept, project.map(|project| project.message));
        self.send_unsent(endpoint, cx);
        cx.notify();
    }

    fn project_root(&self, thread: ThreadId) -> Option<ThreadId> {
        let chat = self.host_chat(thread.host)?;
        chat.message(thread.message)?;
        let root = chat.root_of(thread.message);
        chat.project(root)?;
        Some(ThreadId {
            message: root,
            ..thread
        })
    }

    fn idea_project_at(&self, project: ThreadId) -> Option<IdeaProject> {
        self.idea_project_in(&self.project_names(), project)
    }

    fn idea_project_in(&self, names: &ProjectNames, project: ThreadId) -> Option<IdeaProject> {
        // The project must be on an attached host. Its cached address and
        // outbox stay usable while the connection is interrupted.
        let alias = self.attached_alias(project.host)?;
        let held = self.host_chat(project.host)?.project(project.message)?;
        Some(IdeaProject {
            thread: project,
            name: names.address(&held.name, alias),
            qualified: names::qualified(&held.name, alias),
            runtime: Some(held.config.runtime),
            mode: held.config.mode,
        })
    }

    /// Names `thread` `named`, or asks for its name with `current` to
    /// edit.
    fn rename_thread(
        this: &Entity<Self>,
        thread: ThreadId,
        current: &str,
        named: Option<String>,
        frame: &mut Frame,
        window: &mut Window,
        cx: &mut Context<Frame>,
    ) {
        if let Some(named) = named {
            return Self::send_name(this, thread, &named, frame, cx);
        }
        let this = this.clone();
        frame.open_prompt(
            "rename thread:",
            Rc::new(|_, _, _| Vec::new()),
            Rc::new(move |frame, input, _, cx| Self::send_name(&this, thread, &input, frame, cx)),
            window,
            cx,
        );
        if let Some(prompt) = frame.minibuffer_mut() {
            prompt.set_input(current.to_owned(), window, cx);
            prompt.validate(|text| {
                text.trim()
                    .is_empty()
                    .then_some("rename thread: enter a subject")
            });
        }
    }

    fn send_name(
        this: &Entity<Self>,
        thread: ThreadId,
        name: &str,
        frame: &mut Frame,
        cx: &mut Context<Frame>,
    ) {
        let name = rho_agent_types::one_line(name);
        if name.trim().is_empty() {
            return frame.echo("rename thread: enter a subject", StyleClass::SystemInfo, cx);
        }
        this.update(cx, |chat, cx| {
            chat.call(
                thread.host,
                Name {
                    thread: thread.message,
                    name: name.clone(),
                },
                cx,
                move |chat, reply, cx| {
                    let words = match reply {
                        Ok(()) => format!("named: {name}"),
                        Err(error) => format!("rename thread: {error:#}"),
                    };
                    chat.frame.echo(words, StyleClass::SystemInfo, cx);
                },
            );
        });
    }

    /// `rename thread` away from a thread: which one, by subject, then
    /// its new name, unless it was typed on the line.
    fn pick_thread_to_rename(
        this: Entity<Self>,
        named: Option<String>,
        frame: &mut Frame,
        window: &mut Window,
        cx: &mut Context<Frame>,
    ) {
        let threads = this.clone();
        frame.open_prompt(
            "rename which thread:",
            Rc::new(move |frame, needle, cx| {
                let needle = needle.trim().to_lowercase();
                threads
                    .read(cx)
                    .find(frame.marks(), cx)
                    .into_iter()
                    .filter(|found| found.thread.is_some())
                    .filter(|found| found.title.to_lowercase().contains(&needle))
                    .map(|found| Candidate {
                        value: found.title,
                        description: found.description,
                    })
                    .collect()
            }),
            Rc::new(move |frame, input, window, cx| {
                let picked = this
                    .read(cx)
                    .find(frame.marks(), cx)
                    .into_iter()
                    .filter(|found| found.thread.is_some() && found.title == input.trim())
                    .find_map(|found| this.read(cx).thread_of(&found.address));
                match picked {
                    Some(thread) => Self::rename_thread(
                        &this,
                        thread,
                        input.trim(),
                        named.clone(),
                        frame,
                        window,
                        cx,
                    ),
                    None => frame.echo(
                        &format!("rename thread: no thread called {:?}", input.trim()),
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
            }),
            window,
            cx,
        );
    }

    fn link_for(&self, endpoint: EndpointId) -> Option<Link> {
        self.hosts
            .values()
            .find(|slot| slot.endpoint == Some(endpoint))
            .map(|slot| slot.link.clone())
    }

    /// A receipt and the corresponding mail can arrive in either order.
    /// Retire the optimistic copy only when the host log can replace it.
    fn finish_posted(&mut self, endpoint: EndpointId, cx: &mut Context<Self>) {
        let Some(message) = self.outbox.receipt(endpoint) else {
            return;
        };
        if !self
            .host_chat(endpoint)
            .is_some_and(|chat| chat.message(message).is_some())
        {
            return;
        }
        let Some((client_id, outgoing, kept)) = self.outbox.answered(endpoint) else {
            return;
        };
        self.cache.write_outbox(endpoint, kept, None);
        if outgoing.delivery == Delivery::Idea {
            let pending = PendingIdeaId(client_id).address();
            let posted = ThreadId {
                host: endpoint,
                message,
            }
            .address();
            let idea = pending::title(&rho_agent_types::content_text(&outgoing.content));
            let echo = match self.project_address(ThreadId {
                host: endpoint,
                message: outgoing.parent.expect("an idea is under a project"),
            }) {
                Some(project) => format!("sent to {project}: {idea}"),
                None => format!("sent: {idea}"),
            };
            // The thread takes the queued idea's place among the recent
            // places, and in view when the user was reading it.
            self.frame.with(cx, move |frame, window, cx| {
                frame.replace(&pending, posted, window, cx);
                frame.echo(&echo, StyleClass::SystemInfo, cx);
            });
        }
        cx.notify();
        self.send_unsent(endpoint, cx);
    }

    /// Queues an idea for its project's host, once per client id.
    fn send_idea(&mut self, mail: IdeaMail, cx: &mut Context<Self>) {
        let project = mail.project;
        if self.outbox.contains(mail.client_id) {
            return;
        }
        let outgoing = Outgoing {
            at: UnixMs::now(),
            parent: Some(project.message),
            failed_start: None,
            settles: None,
            content: vec![ContentPart::Text {
                text: mail.text.trim().to_owned(),
            }],
            delivery: Delivery::Idea,
            runtime: mail.runtime,
            mode: mail.mode,
            state: OutgoingState::Queued,
        };
        let (_, kept) = self.outbox.keep(project.host, mail.client_id, outgoing);
        self.keep_outgoing(project.host, Some(project.message), kept, cx);
    }

    fn refuse_reply(&mut self, endpoint: EndpointId, reason: String, cx: &mut Context<Self>) {
        let echo = format!("reply not sent: {}", rho_agent_types::one_line(&reason));
        let (_, kept) = self.outbox.refuse(endpoint, reason);
        self.cache.write_outbox(endpoint, kept, None);
        self.changed(endpoint, cx);
        self.frame.echo(echo, StyleClass::SystemImportant, cx);
        self.send_unsent(endpoint, cx);
    }

    /// The host would not start the idea (`SPEC-project-delivery`). It
    /// stays in the outbox as refused: readable by name in `Places`,
    /// never posted again, and taken back into the draft from its screen.
    fn refuse_idea(&mut self, endpoint: EndpointId, reason: String, cx: &mut Context<Self>) {
        let echo = format!("idea not sent: {}", rho_agent_types::one_line(&reason));
        let (_, kept) = self.outbox.refuse(endpoint, reason);
        self.cache.write_outbox(endpoint, kept, None);
        self.changed(endpoint, cx);
        self.frame.echo(echo, StyleClass::SystemImportant, cx);
        self.send_unsent(endpoint, cx);
    }

    /// A refused idea as the draft takes it back.
    fn refused_idea(&self, id: PendingIdeaId) -> Option<IdeaMail> {
        let (host, _, outgoing) = self.outbox.ideas().find(|(_, client_id, outgoing)| {
            *client_id == id.0 && matches!(outgoing.state, OutgoingState::Refused { .. })
        })?;
        Some(IdeaMail {
            client_id: id.0,
            project: ThreadId {
                host,
                message: outgoing.parent?,
            },
            text: rho_agent_types::content_text(&outgoing.content),
            runtime: outgoing.runtime.clone(),
            mode: outgoing.mode,
        })
    }

    /// The draft took the refused idea back: it leaves the outbox.
    fn withdraw_refused_idea(&mut self, id: PendingIdeaId, cx: &mut Context<Self>) {
        if self.withdraw_refused(id.0, cx) {
            let pending = id.address();
            self.frame.close_all(move |address| address == &pending, cx);
        }
    }

    /// The newest reply in `thread` that its host refused, as the thread's
    /// draft takes it back: its client id and its words.
    fn refused_reply(&self, thread: ThreadId) -> Option<(u64, String)> {
        let (id, outgoing) = self.outbox.refused_reply(thread.host, thread.message)?;
        Some((id, rho_agent_types::content_text(&outgoing.content)))
    }

    /// A draft took a refused idea or reply back to write it again: it
    /// leaves the outbox, and the thread no longer shows it as not sent.
    fn withdraw_refused(&mut self, client_id: u64, cx: &mut Context<Self>) -> bool {
        let Some((endpoint, kept)) = self.outbox.withdraw_refused(client_id) else {
            return false;
        };
        self.cache.write_outbox(endpoint, kept, None);
        self.changed(endpoint, cx);
        true
    }

    /// Posts the host's oldest unsent message, unless one is on its way.
    /// The rest go one by one as each is answered; a retry carries the
    /// same client id, so the host keeps one copy.
    fn send_unsent(&mut self, endpoint: EndpointId, cx: &mut Context<Self>) {
        let Some(link) = self.link_for(endpoint) else {
            return;
        };
        let Some((client_id, outgoing)) = self.outbox.next(endpoint) else {
            return;
        };
        let host_label = self.host_name(endpoint);
        let delivery = outgoing.delivery;
        let reply = link.run(move |dialer| async move {
            match delivery {
                Delivery::Mail => crate::stream::query(
                    &dialer,
                    Post {
                        client_id,
                        parent: outgoing.parent,
                        settles: outgoing.settles,
                        content: outgoing.content,
                    },
                )
                .await
                .map(PostAnswer::mail),
                Delivery::Idea => {
                    let project = ProjectId(outgoing.parent.expect("an idea is under a project"));
                    crate::stream::call(
                        &dialer,
                        PostIdea {
                            client_id,
                            project,
                            runtime: outgoing.runtime,
                            mode: outgoing.mode,
                            content: outgoing.content,
                            host_label,
                        },
                    )
                    .await
                    .map(|reply| match reply {
                        PostIdeaReply::Posted(message) => PostAnswer::Posted(message),
                        PostIdeaReply::RuntimeRefused(reason) => PostAnswer::IdeaRefused(reason),
                    })
                }
                Delivery::Retry => crate::stream::query(
                    &dialer,
                    RetryIdea {
                        client_id,
                        failed_start: outgoing
                            .failed_start
                            .expect("a retry names its failed start"),
                        content: outgoing.content,
                        host_label,
                    },
                )
                .await
                .map(PostAnswer::mail),
            }
        });
        cx.spawn(async move |this, cx| {
            let reply = reply.await;
            let _ = this.update(cx, |this, cx| match reply {
                Ok(PostAnswer::Posted(message)) => {
                    this.outbox.posted(endpoint, message);
                    this.finish_posted(endpoint, cx);
                }
                Ok(PostAnswer::IdeaRefused(reason)) => this.refuse_idea(endpoint, reason, cx),
                Ok(PostAnswer::ReplyRefused(reason)) => this.refuse_reply(endpoint, reason, cx),
                Err(error) => {
                    this.outbox.failed(endpoint);
                    // A transport error is not a refusal. Retain the
                    // exact same client id and observation for the retry.
                    tracing::debug!(%error, "outgoing message waits for reconnect");
                }
            });
        })
        .detach();
    }

    /// One call to a host's mail service; `done` hears the answer.
    pub fn call<C>(
        &self,
        endpoint: EndpointId,
        call: C,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut Self, anyhow::Result<C::Reply>, &mut Context<Self>) + 'static,
    ) where
        C: rho_rpc::protocol::Call + Send + 'static,
        C::Reply: Send + 'static,
    {
        let Some(link) = self.link_for(endpoint) else {
            self.frame.echo(
                format!("{}: not connected", self.host_name(endpoint)),
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        let reply = link.run(move |dialer| async move { crate::stream::call(&dialer, call).await });
        cx.spawn(async move |this, cx| {
            let reply = reply.await;
            let _ = this.update(cx, |this, cx| done(this, reply, cx));
        })
        .detach();
    }

    pub fn thread_of(&self, address: &Address) -> Option<ThreadId> {
        (address.kind() == ThreadId::KIND)
            .then(|| ThreadId::parse(address.rest()))
            .flatten()
    }

    fn with_host<R>(
        &self,
        endpoint: EndpointId,
        names: &ProjectNames,
        _cx: &App,
        read: impl FnOnce(&cards::Host<'_>) -> R,
    ) -> Option<R> {
        let chat = self.logs.get(&endpoint)?;
        let name = self.host_name(endpoint);
        let host = cards::Host {
            endpoint,
            chat,
            name: &name,
            names,
        };
        Some(read(&host))
    }
}

impl ChatSystem {
    fn pending_ideas(&self) -> impl Iterator<Item = (PendingIdeaId, String, &Outgoing)> {
        let names = self.project_names();
        self.outbox.ideas().filter_map(move |(host, id, outgoing)| {
            let project = outgoing.parent?;
            let chat = self.host_chat(host)?;
            let alias = self.host_name(host);
            let project = chat.project(project)?;
            Some((
                PendingIdeaId(id),
                names.address(&project.name, &alias),
                outgoing,
            ))
        })
    }
}

impl ChatSystem {
    fn surface_for(
        &mut self,
        address: &Address,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnySurface> {
        if *address == idea::address() {
            return Some(AnySurface::new(self.idea_view(window, cx)));
        }
        if let Some(id) = PendingIdeaId::from_address(address) {
            let (_, project, outgoing) = self.pending_ideas().find(|(queued, ..)| *queued == id)?;
            let content = rho_agent_types::content_text(&outgoing.content);
            let system = cx.entity();
            return Some(AnySurface::new(cx.new(|cx| {
                PendingIdea::new(system, id, project, content, window, cx)
            })));
        }
        let thread = self.thread_of(address)?;
        let chat = self.logs.get(&thread.host)?;
        chat.message(thread.message)?;
        // A project is an address to write to, with no screen of its own.
        if chat.is_project(thread.message) {
            return None;
        }
        let system = cx.entity();
        let view = cx.new(|cx| ThreadView::new(self, system, thread, window, cx));
        Some(AnySurface::new(view))
    }
}

impl System for ChatSystem {
    fn kinds(&self) -> &'static [&'static str] {
        &[ThreadId::KIND, PendingIdeaId::KIND, idea::KIND]
    }

    /// A thread opens from the copy this window keeps of its host's log,
    /// so it never waits on the host.
    fn open(&mut self, address: &Address, window: &mut Window, cx: &mut Context<Self>) -> Opening {
        self.surface_for(address, window, cx).into()
    }

    fn streams(
        &mut self,
        host: HostId,
        name: &str,
        endpoint: Option<EndpointId>,
        link: &Link,
        _cx: &mut Context<Self>,
    ) -> Vec<Arc<dyn HostStream>> {
        let slot = Slot {
            name: name.to_owned(),
            link: link.clone(),
            endpoint,
        };
        self.hosts.insert(host, slot);
        vec![
            Arc::new(MailStream::new(
                host,
                self.cursors.clone(),
                self.events.clone(),
            )),
            self.git.streams.stream(host),
        ]
    }

    /// The host's threads and their screens go from the window with it. Its
    /// log is a copy the host still has, so the copy goes too and is read
    /// again if the host is attached again; what the user wrote and has
    /// not sent stays for then.
    fn detached(&mut self, host: HostId, cx: &mut Context<Self>) {
        self.git.forget_host(host);
        let Some(endpoint) = self.hosts.remove(&host).and_then(|slot| slot.endpoint) else {
            cx.notify();
            return;
        };
        self.logs.remove(&endpoint);
        self.runtimes.remove(&endpoint);
        self.creating_projects
            .retain(|(creation, _)| creation.host != endpoint);
        self.cursors.reset(endpoint);
        self.cache.reset(endpoint);
        self.frame.close_all(
            move |address| {
                address.kind() == ThreadId::KIND
                    && ThreadId::parse(address.rest()).is_some_and(|thread| thread.host == endpoint)
            },
            cx,
        );
        self.changed(endpoint, cx);
    }

    fn commands(
        &mut self,
        point: &Point,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Command> {
        let this = cx.entity();
        let mut commands = self.project_commands(point, cx);
        // The name belongs to the open thread, wherever the point is in it.
        let here = self.thread_of(&point.surface).and_then(|thread| {
            let chat = self.logs.get(&thread.host)?;
            Some((thread, chat.name(thread.message)?))
        });
        let target = here
            .as_ref()
            .map_or_else(|| "pick thread…".to_owned(), |(_, name)| name.clone());
        commands.push(
            Command::new(
                &RENAME_THREAD,
                move |frame, named, window, cx| match &here {
                    Some((thread, current)) => {
                        Self::rename_thread(&this, *thread, current, named, frame, window, cx)
                    }
                    None => Self::pick_thread_to_rename(this.clone(), named, frame, window, cx),
                },
            )
            .target(target),
        );
        commands
    }

    fn claims(
        &self,
        marks: &Marks,
        now: Timestamp,
        mut trace: Option<&mut Trace>,
        cx: &App,
    ) -> Vec<Claim> {
        let mut claims = Vec::new();
        let names = self.project_names();
        let mut seeds: Vec<EndpointId> = self.logs.keys().copied().collect();
        seeds.sort_unstable();
        for seed in seeds {
            claims.extend(
                self.with_host(seed, &names, cx, |host| {
                    cards::claims(
                        host,
                        marks,
                        now,
                        trace.as_deref_mut(),
                        self.outbox.settlements(seed),
                    )
                })
                .unwrap_or_default(),
            );
        }
        self.git_claims(&mut claims, cx);
        claims
    }

    /// A thread with a push or fetch that waits takes its answers in place
    /// of done and mute.
    fn verdicts(&mut self, address: &Address, cx: &mut Context<Self>) -> Option<Vec<Row>> {
        self.git_verdicts(self.thread_of(address)?, cx)
    }

    fn name(
        &self,
        address: &Address,
        _marks: &rho_dealer::Marks,
        cx: &App,
    ) -> Option<(String, String)> {
        self.thread_name(self.thread_of(address)?, cx)
    }

    /// A message is read in the thread it was posted under, where a private
    /// reply about it shows; a thread directly under a project is read in
    /// its own screen, under its head.
    fn shown_in(&self, address: &Address, _cx: &App) -> Option<Address> {
        let thread = self.thread_of(address)?;
        let chat = self.logs.get(&thread.host)?;
        let parent = chat.message(thread.message)?.parent?;
        let in_channel = chat.is_project(parent);
        let message = if in_channel { thread.message } else { parent };
        Some(
            ThreadId {
                host: thread.host,
                message,
            }
            .address(),
        )
    }

    /// What is unread moved: a root's list reads the marks on its next
    /// draw, which this asks for.
    fn marks_moved(&mut self, _touched: &BTreeSet<Address>, cx: &mut Context<Self>) {
        cx.notify();
    }

    fn desk_context(&self, address: &Address, _cx: &App) -> Option<String> {
        let card = self.thread_of(address)?;
        let chat = self.host_chat(card.host)?;
        chat.latest_flash(chat.subthread_of(card.message))
            .map(rho_agent_types::Message::headline)
    }

    /// A thread lives on the host whose log holds it.
    fn host(&self, address: &Address, _cx: &App) -> Option<HostId> {
        let thread = self.thread_of(address)?;
        self.hosts
            .iter()
            .find(|(_, slot)| slot.endpoint == Some(thread.host))
            .map(|(host, _)| *host)
    }

    fn project(&self, address: &Address, _cx: &App) -> Option<Project> {
        let project = self.idea_project_of(address)?;
        Some(Project {
            address: project.thread.address(),
            name: project.name,
        })
    }

    fn text_sources(&self, search: &mut TextSearch, current: &Address, _cx: &App) {
        if !search.includes(TextKind::Chat) {
            return;
        }
        if let Some(thread) = self.thread_of(current) {
            search.add_draft(
                &self.drafts,
                &(thread.host, Some(thread.message)),
                current.clone(),
            );
        }
        for (host, _) in self.hosts() {
            search.add(self.logs[&host].text.clone(), move |message| {
                TextTarget::Message { host, message }
            });
        }
    }

    fn text_presenter(&self, _cx: &App) -> Option<TextPresenter<'_>> {
        let mut hosts = HashMap::new();
        let mut threads = HashMap::new();
        let mut projects = HashMap::new();
        let names = self.project_names();
        Some(Box::new(move |hit| {
            let id = match &hit.target {
                TextTarget::Draft(address) => {
                    let id = self.thread_of(address)?;
                    return hit
                        .is_from(&self.drafts, &(id.host, Some(id.message)))
                        .then(|| FoundText {
                            address: address.clone(),
                            title: "Draft".into(),
                            line: hit.line().to_owned(),
                            point: SearchPoint::Draft(hit.row),
                        });
                }
                &TextTarget::Message { host, message } => ThreadId { host, message },
                TextTarget::Note(_) => return None,
            };
            let chat = self.logs.get(&id.host)?;
            if !hit.is_from(&chat.text, &id.message) {
                return None;
            }
            let (host_key, host_alias) = hosts.entry(id.host).or_insert_with(|| {
                let key = id.host.to_string();
                let alias = self.attached_alias(id.host).unwrap_or(&key).to_owned();
                (key, alias)
            });
            let address = |id| Address::new(ThreadId::KIND, ThreadId::key_under(host_key, id));
            let thread = chat.subthread_of(id.message);
            // A project root is no place to read.
            if chat.is_project(thread) {
                return None;
            }
            let thread_id = ThreadId {
                message: thread,
                ..id
            };
            let (owner, title) = match threads.entry(thread_id) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let subject = chat.name(chat.thread_of(thread))?;
                    let root = chat.root_of(thread);
                    let project = projects
                        .entry(ThreadId {
                            message: root,
                            ..id
                        })
                        .or_insert_with(|| {
                            let host = cards::Host {
                                endpoint: id.host,
                                chat,
                                name: host_alias,
                                names: &names,
                            };
                            cards::context_at_root(&host, root)
                        });
                    let title = if project.is_empty() {
                        format!("Message · {subject}")
                    } else {
                        format!("Message · {subject} · {project}")
                    };
                    entry.insert((address(thread), title))
                }
            };
            Some(FoundText {
                address: owner.clone(),
                title: title.clone(),
                line: hit.line().to_owned(),
                point: SearchPoint::Message {
                    message: address(id.message),
                    line: hit.row,
                },
            })
        }))
    }

    fn find(&self, _marks: &Marks, _cx: &App) -> Vec<Found> {
        let mut found = self
            .pending_ideas()
            .map(|(id, project, outgoing)| Found {
                address: id.address(),
                title: pending::title(&rho_agent_types::content_text(&outgoing.content)),
                description: project,
                thread: Some(FoundThread {
                    status: Some(pending::status(&outgoing.state).to_owned()),
                    work: None,
                }),
                recency: outgoing.at.0,
                also: Vec::new(),
            })
            .collect::<Vec<_>>();
        let names = self.project_names();
        for (host, alias) in self.hosts() {
            let Some(chat) = self.logs.get(&host) else {
                continue;
            };
            let place = cards::Host {
                endpoint: host,
                chat,
                name: &alias,
                names: &names,
            };
            let mut seen = BTreeSet::new();
            let ids = chat.roots().iter().flat_map(|&root| {
                std::iter::once(root).chain(chat.children(Some(root)).iter().copied())
            });
            for id in ids.filter(|id| seen.insert(*id)) {
                let Some(mail) = chat.message(id) else {
                    continue;
                };
                if mail.revision == rho_agent_types::Revision::Deleted || chat.agent_only(id) {
                    continue;
                }
                let Some(subject) = chat.name(id) else {
                    continue;
                };
                let project = cards::context(&place, id);
                // `tide@lab` finds what shows as `tide`.
                let qualified = chat
                    .project(chat.root_of(id))
                    .map(|held| names::qualified(&held.name, &alias))
                    .filter(|qualified| *qualified != project);
                let is_project = chat.is_project(id);
                let (thread, recency) = if is_project {
                    (None, 0)
                } else {
                    let (words, latest) = chat.thread_activity(id);
                    (
                        Some(FoundThread {
                            status: words,
                            work: chat.work(id),
                        }),
                        latest.map_or(0, |at| at.0),
                    )
                };
                found.push(Found {
                    address: ThreadId { host, message: id }.address(),
                    // A project row says where and how its agents work.
                    description: if is_project {
                        self.project_details(ThreadId { host, message: id })
                            .unwrap_or_default()
                    } else {
                        project.clone()
                    },
                    title: if is_project { project } else { subject },
                    thread,
                    recency,
                    also: qualified.into_iter().collect(),
                });
            }
        }
        found
    }
}

/// How long ago, in the deal bar's units.
pub(crate) fn ago(at_ms: u64, now: Timestamp) -> String {
    let at = Timestamp::from_millisecond(at_ms as i64).unwrap_or(Timestamp::UNIX_EPOCH);
    let minutes = now.duration_since(at).as_mins();
    match minutes {
        ..=0 => "now".to_owned(),
        1..60 => format!("{minutes}m"),
        60..1440 => format!("{}h", minutes / 60),
        _ => format!("{}d", minutes / 1440),
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{ProjectMode, Voice};

    use super::*;

    /// `fork thread` on a message: the tangent's draft quotes it, and
    /// words the user already wrote there stay as they are.
    #[gpui::test]
    fn a_tangent_draft_quotes_its_message_once(cx: &mut gpui::TestAppContext) {
        let endpoint = EndpointId([3; 32]);
        let agent = AgentId::from_counter(7, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let mut log = HostChat::default();
        for (seq, parent, from, text) in [
            (1, None, Party::User, "tide"),
            (
                2,
                Some(1),
                Party::Agent(agent),
                "The clock drifts.\n\nShould I pin it?\n",
            ),
        ] {
            log.tell(&rho_agent_types::ChatRecord {
                seq: ChatSeq(seq),
                at: UnixMs(seq),
                event: rho_agent_types::ChatEvent::Posted {
                    parent: parent.map(|parent| MessageId(ChatSeq(parent))),
                    retry_of: None,
                    thread_reply: None,
                    from,
                    voice: Voice::Message,
                    content: vec![ContentPart::Text { text: text.into() }],
                },
            });
        }
        let (_, mail, _db) = search_bench::fixture(cx, log);
        let tangent = ThreadId {
            host: endpoint,
            message: MessageId(ChatSeq(2)),
        };
        cx.update(|cx| {
            mail.update(cx, |mail, _| {
                mail.start_tangent(tangent);
                assert_eq!(
                    mail.draft(endpoint, Some(tangent.message)),
                    "> The clock drifts.\n> \n> Should I pin it?\n"
                );
                mail.set_draft(endpoint, Some(tangent.message), "Pin it.".into());
                mail.start_tangent(tangent);
                assert_eq!(mail.draft(endpoint, Some(tangent.message)), "Pin it.");
            })
        });
    }

    #[gpui::test]
    fn a_queued_idea_is_findable_until_its_post_is_in_the_log(cx: &mut gpui::TestAppContext) {
        let endpoint = EndpointId([3; 32]);
        let project = MessageId(ChatSeq(1));
        let mut log = HostChat::default();
        log.tell(&rho_agent_types::ChatRecord {
            seq: project.0,
            at: UnixMs(1),
            event: rho_agent_types::ChatEvent::Posted {
                parent: None,
                retry_of: None,
                thread_reply: None,
                from: Party::User,
                voice: Voice::Message,
                content: vec![ContentPart::Text {
                    text: "tide".into(),
                }],
            },
        });
        log.tell(&project_record(ChatSeq(2), project));
        let (_, mail, _db) = search_bench::fixture(cx, log);
        let title = "Test the thermocouple offline.";
        cx.update(|cx| {
            mail.update(cx, |mail, cx| {
                let id = 77;
                mail.send_idea(
                    IdeaMail {
                        client_id: id,
                        project: ThreadId {
                            host: endpoint,
                            message: project,
                        },
                        text: title.into(),
                        runtime: None,
                        mode: None,
                    },
                    cx,
                );
                let pending = PendingIdeaId(id).address();
                let queued = |mail: &ChatSystem, cx: &App| {
                    mail.find(&Marks::default(), cx)
                        .into_iter()
                        .filter(|row| row.title == title)
                        .collect::<Vec<_>>()
                };
                let check_queued = |mail: &ChatSystem, cx: &App| {
                    let matches = queued(mail, cx);
                    assert_eq!(matches.len(), 1);
                    assert_eq!(&matches[0].address, &pending);
                    assert_eq!(
                        matches[0].thread.as_ref().unwrap().status.as_deref(),
                        Some("queued")
                    );
                };
                check_queued(mail, cx);
                let (client_id, post) = mail.outbox.next(endpoint).expect("the idea is durable");
                assert_eq!(client_id, id);
                let posted = MessageId(ChatSeq(mail.logs[&endpoint].head.0 + 1));
                mail.outbox.posted(endpoint, posted);
                mail.finish_posted(endpoint, cx);
                check_queued(mail, cx);
                mail.logs
                    .get_mut(&endpoint)
                    .unwrap()
                    .tell(&rho_agent_types::ChatRecord {
                        seq: posted.0,
                        at: UnixMs(12),
                        event: rho_agent_types::ChatEvent::Posted {
                            parent: Some(project),
                            retry_of: None,
                            thread_reply: None,
                            from: Party::User,
                            voice: Voice::Message,
                            content: post.content,
                        },
                    });
                mail.finish_posted(endpoint, cx);
                let matches = queued(mail, cx);
                assert_eq!(matches.len(), 1);
                assert_eq!(
                    matches[0].address,
                    ThreadId {
                        host: endpoint,
                        message: posted
                    }
                    .address()
                );
                assert!(!mail.outbox.contains(id));
            })
        });
    }

    /// `SPEC-project-delivery`: a refused idea is not posted again and is
    /// not lost; it waits, readable, until the draft takes it back.
    #[gpui::test]
    fn a_refused_idea_stays_until_the_draft_takes_it_back(cx: &mut gpui::TestAppContext) {
        let endpoint = EndpointId([3; 32]);
        let project = MessageId(ChatSeq(1));
        let mut log = HostChat::default();
        log.tell(&rho_agent_types::ChatRecord {
            seq: project.0,
            at: UnixMs(1),
            event: rho_agent_types::ChatEvent::Posted {
                parent: None,
                retry_of: None,
                thread_reply: None,
                from: Party::User,
                voice: Voice::Message,
                content: vec![ContentPart::Text {
                    text: "tide".into(),
                }],
            },
        });
        log.tell(&project_record(ChatSeq(2), project));
        let (_, mail, _db) = search_bench::fixture(cx, log);
        let title = "Test the thermocouple on sol-high.";
        let id = PendingIdeaId(78);
        let address = ThreadId {
            host: endpoint,
            message: project,
        };
        let runtime = rho_agent_types::RuntimeLabel::parse("sol-high").unwrap();
        cx.update(|cx| {
            mail.update(cx, |mail, cx| {
                mail.send_idea(
                    IdeaMail {
                        client_id: id.0,
                        project: address,
                        text: title.into(),
                        runtime: Some(runtime.clone()),
                        mode: Some(ProjectMode::PerIdea),
                    },
                    cx,
                );
                assert_eq!(mail.refused_idea(id).map(|idea| idea.text), None);
                mail.outbox.next(endpoint).expect("the idea is posted");
                mail.refuse_idea(endpoint, "eng-ka35 cannot change kind".into(), cx);
                let status = |mail: &ChatSystem, cx: &App| {
                    mail.find(&Marks::default(), cx)
                        .into_iter()
                        .filter(|row| row.address == id.address())
                        .map(|row| row.thread.unwrap().status.unwrap())
                        .collect::<Vec<_>>()
                };
                assert_eq!(status(mail, cx), ["not sent"]);
                assert!(mail.outbox.next(endpoint).is_none(), "never posted again");
                let idea = mail.refused_idea(id).expect("the words are kept");
                assert_eq!(
                    (idea.client_id, &idea.project, idea.text.as_str()),
                    (id.0, &address, title)
                );
                assert_eq!(idea.runtime, Some(runtime.clone()));
                assert_eq!(
                    idea.mode,
                    Some(ProjectMode::PerIdea),
                    "the `in:` line is kept"
                );
                mail.withdraw_refused_idea(id, cx);
                assert!(!mail.outbox.contains(id.0));
                assert_eq!(status(mail, cx), Vec::<String>::new());
            })
        });
    }

    #[gpui::test]
    fn queuing_a_reply_settles_the_thread_through_what_it_saw_on_this_device(
        cx: &mut gpui::TestAppContext,
    ) {
        use rho_agent_types::{ChatEvent, ChatRecord};
        let host = EndpointId([3; 32]);
        let thread = ThreadId {
            host,
            message: MessageId(ChatSeq(2)),
        };
        let first = MessageId(ChatSeq(3));
        let second = MessageId(ChatSeq(4));
        // The card as it was read: both asks in view, the newer answered.
        let read = MessageVersion {
            message: second,
            version: second.0,
        };
        let third = MessageId(ChatSeq(6));
        let agent = AgentId::from_counter(7, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let record = |id: MessageId, from: Party, parent, thread_reply| ChatRecord {
            seq: id.0,
            at: UnixMs(id.0.0),
            event: ChatEvent::Posted {
                parent,
                retry_of: None,
                thread_reply,
                from,
                voice: Voice::Message,
                content: vec![ContentPart::Text {
                    text: format!("Mail #{}", id.0.0),
                }],
            },
        };
        let mut log = HostChat::default();
        log.tell(&record(MessageId(ChatSeq(1)), Party::User, None, None));
        log.tell(&record(
            thread.message,
            Party::User,
            Some(MessageId(ChatSeq(1))),
            None,
        ));
        for id in [first, second] {
            log.tell(&record(id, Party::Agent(agent), Some(thread.message), None));
        }
        let (_, mail, _db) = search_bench::fixture(cx, log);
        cx.update(|cx| {
            mail.update(cx, |mail, cx| {
                // How far the waiting card reads the thread, if one waits.
                let dealt = |mail: &ChatSystem, cx: &App| {
                    let claims = mail.claims(&Marks::default(), Timestamp::now(), None, cx);
                    assert!(claims.len() <= 1);
                    claims.first().map(|claim| claim.seen)
                };
                let waiting = Some(rho_dealer::facts::Seen::Topic(second.0.0));
                assert_eq!(dealt(mail, cx), waiting);
                let words = "Keep the reference clock.";
                let id = mail.reply(thread, Some(read), words.into(), cx);
                assert_eq!(dealt(mail, cx), None, "both asks were in view");
                assert_eq!(mail.outbox.next(host).unwrap().0, id);
                mail.outbox.failed(host);
                assert_eq!(dealt(mail, cx), None, "a lost connection is not a refusal");
                assert_eq!(mail.outbox.next(host).unwrap().0, id);
                let refusal = RpcAnswer::Failed {
                    reason: "the thread moved".into(),
                };
                let PostAnswer::ReplyRefused(reason) = PostAnswer::mail(refusal) else {
                    panic!("a host refusal must not be retried as a transport error");
                };
                mail.refuse_reply(host, reason, cx);
                assert_eq!(dealt(mail, cx), waiting);
                let retained = mail.local_under(host, Some(thread.message));
                assert_eq!(retained.len(), 1);
                assert_eq!(rho_agent_types::content_text(&retained[0].content), words);
                assert_eq!(
                    retained[0].state,
                    OutgoingState::Refused {
                        reason: "the thread moved".into()
                    }
                );
                assert!(mail.outbox.next(host).is_none());
                // The successful path must not briefly redeal the same mail
                // when the receipt arrives before the host's log.
                let second_attempt = mail.reply(thread, Some(read), words.into(), cx);
                assert_eq!(mail.outbox.next(host).unwrap().0, second_attempt);
                let logged = MessageId(ChatSeq(5));
                mail.outbox.posted(host, logged);
                mail.finish_posted(host, cx);
                assert_eq!(dealt(mail, cx), None);
                mail.logs.get_mut(&host).unwrap().tell(&record(
                    logged,
                    Party::User,
                    Some(thread.message),
                    Some(rho_agent_types::ThreadReply {
                        settles: Some(read),
                    }),
                ));
                mail.finish_posted(host, cx);
                assert_eq!(dealt(mail, cx), None);
                assert_eq!(mail.local_under(host, Some(thread.message)).len(), 1);
                // Mail after what the reply saw deals again.
                mail.logs.get_mut(&host).unwrap().tell(&record(
                    third,
                    Party::Agent(agent),
                    Some(thread.message),
                    None,
                ));
                assert_eq!(
                    dealt(mail, cx),
                    Some(rho_dealer::facts::Seen::Topic(third.0.0))
                );
            })
        });
    }

    /// The device sends a reply to a refused start as its retry, also after
    /// the user changed where the project works; any other reply, to a
    /// stopped or failed agent too, is mail its agents read, and the host
    /// wakes them with it.
    #[gpui::test]
    fn a_reply_retries_a_refused_start_and_is_mail_everywhere_else(cx: &mut gpui::TestAppContext) {
        use rho_agent_types::{ChatEvent, ChatRecord, Stop};
        let host = EndpointId([3; 32]);
        let project = MessageId(ChatSeq(1));
        let refused = MessageId(ChatSeq(3));
        let stop = MessageId(ChatSeq(4));
        let working = MessageId(ChatSeq(6));
        let agent = AgentId::from_counter(7, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let record = |id: MessageId, from: Party, parent: Option<MessageId>| ChatRecord {
            seq: id.0,
            at: UnixMs(id.0.0),
            event: ChatEvent::Posted {
                parent,
                retry_of: None,
                thread_reply: None,
                from,
                voice: Voice::Message,
                content: vec![ContentPart::Text {
                    text: format!("Mail #{}", id.0.0),
                }],
            },
        };
        let mut log = HostChat::default();
        log.tell(&record(project, Party::User, None));
        log.tell(&project_record(ChatSeq(2), project));
        log.tell(&record(refused, Party::User, Some(project)));
        log.tell(&record(stop, Party::StartFailed, Some(refused)));
        // `set project place` after the refusal: the host reads it when
        // the retry comes, so the device sends the same retry.
        let mut moved = project_record(ChatSeq(5), project);
        if let ChatEvent::Project { config, .. } = &mut moved.event {
            config.place = rho_agent_types::ProjectPlace::parse("~/src/tide-fixed").unwrap();
        }
        log.tell(&moved);
        log.tell(&record(working, Party::User, Some(project)));
        log.tell(&record(
            MessageId(ChatSeq(7)),
            Party::Agent(agent),
            Some(working),
        ));
        log.tell(&record(
            MessageId(ChatSeq(8)),
            Party::System {
                agent,
                stop: Stop::Failed,
                since: UnixMs(8),
            },
            Some(working),
        ));
        let (_, mail, _db) = search_bench::fixture(cx, log);
        cx.update(|cx| {
            mail.update(cx, |mail, cx| {
                let sent = |mail: &ChatSystem, thread: MessageId| {
                    mail.local_under(host, Some(thread))
                        .last()
                        .map(|outgoing| (outgoing.delivery, outgoing.failed_start))
                        .expect("the reply is queued")
                };
                let thread = |message| ThreadId { host, message };
                let card = MessageVersion {
                    message: stop,
                    version: stop.0,
                };
                // Dealt from its card, and opened by name.
                for settles in [Some(card), None] {
                    mail.reply(thread(refused), settles, "Try the fixed place.".into(), cx);
                    assert_eq!(
                        sent(mail, refused),
                        (Delivery::Retry, Some(stop)),
                        "{settles:?}"
                    );
                }
                mail.reply(thread(working), None, "Go on after the failure.".into(), cx);
                assert_eq!(sent(mail, working), (Delivery::Mail, None));
            })
        });
    }

    fn project_record(seq: ChatSeq, root: MessageId) -> rho_agent_types::ChatRecord {
        rho_agent_types::ChatRecord {
            seq,
            at: UnixMs(seq.0),
            event: rho_agent_types::ChatEvent::Project {
                project: rho_agent_types::ProjectId(root),
                config: rho_agent_types::ProjectConfig {
                    place: rho_agent_types::ProjectPlace::parse("~/src/tide").unwrap(),
                    mode: rho_agent_types::ProjectMode::One,
                    runtime: rho_agent_types::RuntimeLabel::default(),
                },
            },
        }
    }

    #[test]
    fn ago_reads_in_the_bars_units() {
        let now = Timestamp::from_millisecond(10_000_000_000).unwrap();
        let ms = |mins: i64| (now - jiff::SignedDuration::from_mins(mins)).as_millisecond() as u64;
        assert_eq!(ago(ms(0), now), "now");
        assert_eq!(ago(ms(5), now), "5m");
        assert_eq!(ago(ms(130), now), "2h");
        assert_eq!(ago(ms(3000), now), "2d");
    }
}

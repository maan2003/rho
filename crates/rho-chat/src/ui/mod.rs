//! The chat as the frame shows it.
//!
//! [`ChatSystem`] is the one entity behind every chat surface: each host's
//! forest, the disk copy, the outbox, and the session to each host. It
//! plugs into the frame as a [`System`] under the `thread` kind: a root
//! message is a channel, a reply to a root is a thread, and every one of
//! them opens as a [`ThreadView`]. What of the chat wants the user is
//! [`crate::cards`]'s to say; its mail projects, moving threads and `find`'s
//! rows are made here.

mod thread;

use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Arc;

use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Task, Window};
use jiff::Timestamp;
use rho_agent_hosts::{HostId, HostStream, Link};
use rho_agent_types::{
    Address as Party, AgentId, ContentPart, Doing, EndpointId, MessageId, ThreadStatus,
    content_text, one_line,
};
use rho_dealer::facts::Seen;
use rho_dealer::rank::{Hand, Trace};
use rho_dealer::{Address, Addressed as _, Claim, Marks};
use rho_frame::desk::MovingThread;
use rho_frame::idea::{IdeaMail, IdeaProject};
use rho_frame::{AnySurface, Candidate, Found, FrameLink, Point, Row, System};
use rho_window::style::StyleClass;
pub use thread::ThreadView;

use crate::cache::Cache;
use crate::fold::HostChat;
use crate::outbox::Outbox;
use crate::protocol::Name;
use crate::stream::{ChatEvent, ChatStream, Cursors};
use crate::{AgentRef, ThreadId, cards};

/// What the chat tells whoever listens: the agents system starts an
/// agent from a message once the host has logged it.
pub enum Event {
    /// The user sent the draft under `parent`, a channel, as an idea: the
    /// agents system says where an agent starts from it, then sends it.
    /// The draft stays until then.
    Idea {
        endpoint: EndpointId,
        parent: MessageId,
        source: IdeaSource,
    },
    /// The user confirmed sending the draft from a project start prompt.
    DraftSent(ThreadId),
    /// The host logged a message the user wrote, under the id this client
    /// gave it when it was written.
    Delivered {
        endpoint: EndpointId,
        client_id: u64,
        message: MessageId,
    },
    /// A host's forest changed.
    Changed { endpoint: EndpointId },
}

/// Which box supplied the idea. Each has a separate draft; a first
/// start's question must not swap one draft for the other.
#[derive(Clone)]
pub enum IdeaSource {
    ChannelDraft,
    Box(IdeaMail),
}

/// One attached host: what the user calls it, how to call it, and whose
/// log it carries once its session has said.
struct Slot {
    name: String,
    link: Link,
    endpoint: Option<EndpointId>,
    view: tokio::sync::watch::Sender<Option<MessageId>>,
}

/// An agent of a thread as the agents system tells the chat: what to
/// call it, and what it is doing as its host last said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Participant {
    pub label: String,
    /// `None` while its host has said nothing of it.
    pub doing: Option<Doing>,
    /// Why it stopped, in its host's words, while a failure stopped it.
    pub stopped_because: Option<String>,
}

/// What the chat knows of an agent of the host with this identity, and
/// `None` once it is gone; the agents system answers, the chat only asks.
pub type Participants = Rc<dyn Fn(EndpointId, AgentId, &App) -> Option<Participant>>;

pub struct ChatSystem {
    cache: Cache,
    hosts: HashMap<HostId, Slot>,
    /// Each log's forest, by the identity of the host that keeps it.
    chats: HashMap<EndpointId, HostChat>,
    cursors: Cursors,
    outbox: Outbox,
    last_project: Option<ThreadId>,
    view: Option<ThreadId>,
    drafts: HashMap<(EndpointId, Option<MessageId>), String>,
    /// Threads whose agent did not start from this window, and why, in the
    /// host's words, while this window runs.
    failed_starts: HashMap<ThreadId, String>,
    events: UnboundedSender<ChatEvent>,
    frame: FrameLink,
    /// Whether an agent still exists on a host, its label, and what it is
    /// doing; the agents system knows.
    participants: Participants,
    _pump: Task<()>,
}

impl EventEmitter<Event> for ChatSystem {}

impl ChatSystem {
    pub fn new(db: rho_db::RhoDb, frame: FrameLink, cx: &mut Context<Self>) -> Self {
        let cache = Cache::open_on(db).expect("the chat copy opens");
        let loaded = cache.load();
        let mut chats = HashMap::new();
        for (seed, _) in &loaded.cursors {
            let mut chat = HostChat::default();
            for record in cache.read_log(*seed) {
                chat.tell(&record);
            }
            chats.insert(*seed, chat);
        }
        let (events, mut incoming) = unbounded::<ChatEvent>();
        let pump = cx.spawn(async move |this, cx| {
            while let Some(event) = incoming.next().await {
                if this
                    .update(cx, |this, cx| this.chat_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            cursors: Cursors::new(loaded.cursors),
            outbox: Outbox::load(loaded.outbox),
            last_project: loaded.last_project,
            drafts: loaded
                .drafts
                .into_iter()
                .map(|(seed, parent, text)| ((seed, parent), text))
                .collect(),
            cache,
            view: None,
            failed_starts: HashMap::new(),
            hosts: HashMap::new(),
            chats,
            events,
            frame,
            participants: Rc::new(|_, agent, _| {
                Some(Participant {
                    label: agent.encoded(),
                    doing: None,
                    stopped_because: None,
                })
            }),
            _pump: pump,
        }
    }

    /// Tells chat which agents still exist and how their transcripts are named.
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

    /// Reports which mail thread is open in the focused place, if any.
    /// A focus change to a non-mail screen sends `None`; neither opens
    /// nor closes change a verdict or read mark.
    pub fn set_view(&mut self, thread: Option<ThreadId>) {
        self.view = thread;
        for slot in self.hosts.values() {
            Self::sync_view(slot, thread);
        }
    }

    fn sync_view(slot: &Slot, view: Option<ThreadId>) {
        let next = view
            .filter(|view| Some(view.host) == slot.endpoint)
            .map(|view| view.message);
        slot.view.send_if_modified(|current| {
            let changed = *current != next;
            *current = next;
            changed
        });
    }

    pub fn status(&self, thread: ThreadId) -> Option<&ThreadStatus> {
        self.chat(thread.host)?.status(thread.message)
    }

    pub fn project_address(&self, channel: ThreadId) -> Option<String> {
        let alias = &self
            .hosts
            .values()
            .find(|slot| slot.endpoint == Some(channel.host))?
            .name;
        self.chat(channel.host)?
            .project(channel.message)
            .map(|project| project.address(alias))
    }

    /// Resolves a project address against aliases this device owns and the
    /// channels the host has sent us.
    pub fn project_named(&self, address: &str) -> Option<ThreadId> {
        let (project, host_alias) = address.rsplit_once('@')?;
        let endpoint = self
            .hosts
            .values()
            .find(|slot| slot.name == host_alias)?
            .endpoint?;
        let message = self.chat(endpoint)?.root_named(project)?;
        Some(ThreadId {
            host: endpoint,
            message,
        })
    }

    pub fn chat(&self, endpoint: EndpointId) -> Option<&HostChat> {
        self.chats.get(&endpoint)
    }

    /// The hosts whose logs are held, with their names: those attached
    /// under the name the user gave them, the rest by identity.
    pub fn hosts(&self) -> Vec<(EndpointId, String)> {
        let mut hosts: Vec<(EndpointId, String)> = self
            .chats
            .keys()
            .map(|seed| (*seed, self.host_name(*seed)))
            .collect();
        hosts.sort_by(|a, b| a.1.cmp(&b.1));
        hosts
    }

    pub fn host_name(&self, endpoint: EndpointId) -> String {
        self.hosts
            .values()
            .find(|slot| slot.endpoint == Some(endpoint))
            .map(|slot| slot.name.clone())
            .unwrap_or_else(|| format!("{endpoint}"))
    }

    fn project_of(&self, thread: ThreadId) -> String {
        let host = self.host_name(thread.host);
        self.chat(thread.host).map_or_else(
            || host.clone(),
            |chat| cards::context(chat, thread.message, &host),
        )
    }

    pub fn draft(&self, endpoint: EndpointId, parent: Option<MessageId>) -> &str {
        self.drafts
            .get(&(endpoint, parent))
            .map_or("", String::as_str)
    }

    pub fn set_draft(&mut self, endpoint: EndpointId, parent: Option<MessageId>, text: String) {
        self.cache.write_draft(endpoint, parent, text.clone());
        match text.is_empty() {
            true => self.drafts.remove(&(endpoint, parent)),
            false => self.drafts.insert((endpoint, parent), text),
        };
    }

    /// What the user wrote under `parent` that the host has not logged.
    pub fn waiting_under(
        &self,
        endpoint: EndpointId,
        parent: Option<MessageId>,
    ) -> Vec<&crate::outbox::Outgoing> {
        self.outbox.waiting_under(endpoint, parent)
    }

    fn chat_event(&mut self, event: ChatEvent, cx: &mut Context<Self>) {
        match event {
            ChatEvent::Hello {
                host,
                endpoint,
                fresh,
                ..
            } => {
                if let Some(slot) = self.hosts.get_mut(&host) {
                    slot.endpoint = Some(endpoint);
                    Self::sync_view(slot, self.view);
                }
                if fresh {
                    self.cache.reset(endpoint);
                    self.chats.insert(endpoint, HostChat::default());
                }
                self.chats.entry(endpoint).or_default();
                self.send_unsent(endpoint, cx);
                self.changed(endpoint, cx);
            }
            ChatEvent::Records { host, records } => {
                let Some(endpoint) = self.hosts.get(&host).and_then(|slot| slot.endpoint) else {
                    return;
                };
                let chat = self.chats.entry(endpoint).or_default();
                for record in &records {
                    chat.tell(record);
                }
                if let Some(last) = records.last() {
                    self.cursors.advance(endpoint, last.seq);
                }
                self.cache.write_records(endpoint, records);
                self.changed(endpoint, cx);
            }
            ChatEvent::Statuses { host, statuses } => {
                if let Some(endpoint) = self.hosts.get(&host).and_then(|slot| slot.endpoint) {
                    self.chats
                        .entry(endpoint)
                        .or_default()
                        .set_statuses(statuses);
                    self.changed(endpoint, cx);
                }
            }
            ChatEvent::Closed { host } => {
                if let Some(endpoint) = self.hosts.get(&host).and_then(|slot| slot.endpoint)
                    && let Some(chat) = self.chats.get_mut(&endpoint)
                {
                    chat.set_statuses(Vec::new());
                    self.changed(endpoint, cx);
                }
            }
        }
    }

    fn changed(&mut self, endpoint: EndpointId, cx: &mut Context<Self>) {
        cx.emit(Event::Changed { endpoint });
        cx.notify();
        self.frame.changed(cx);
    }

    /// Writes a message: into the outbox and onto disk now, to the host
    /// when it can be reached. Answers the id the client gave it, which
    /// [`Event::Delivered`] names once the host has it.
    pub fn post(
        &mut self,
        endpoint: EndpointId,
        parent: Option<MessageId>,
        text: String,
        mentions: Vec<AgentId>,
        cx: &mut Context<Self>,
    ) -> u64 {
        self.queue_mail(endpoint, parent, None, text, mentions, cx)
    }

    /// Writes in `thread` and answers one particular mail, even if its writer
    /// is no longer a thread member or the client is offline.
    pub fn reply(
        &mut self,
        thread: ThreadId,
        to: MessageId,
        text: String,
        cx: &mut Context<Self>,
    ) -> u64 {
        self.queue_mail(
            thread.host,
            Some(thread.message),
            Some(to),
            text,
            Vec::new(),
            cx,
        )
    }

    fn queue_mail(
        &mut self,
        endpoint: EndpointId,
        parent: Option<MessageId>,
        reply_to: Option<MessageId>,
        text: String,
        mentions: Vec<AgentId>,
        cx: &mut Context<Self>,
    ) -> u64 {
        let project = parent.and_then(|message| {
            self.project_root(ThreadId {
                host: endpoint,
                message,
            })
        });
        if let Some(project) = project {
            self.last_project = Some(project);
            self.frame.changed(cx);
        }
        let content = vec![ContentPart::Text { text }];
        let (id, kept) = self
            .outbox
            .push(endpoint, parent, reply_to, mentions, content);
        self.cache
            .write_outbox(endpoint, kept, project.map(|project| project.message));
        self.send_unsent(endpoint, cx);
        cx.notify();
        id
    }

    /// The agent asked for in `thread` did not start, for `reason`: the
    /// thread says so until an agent is in it.
    pub fn note_failed_start(&mut self, thread: ThreadId, reason: String, cx: &mut Context<Self>) {
        self.failed_starts.insert(thread, reason);
        cx.notify();
    }

    /// Why the agent asked for in `thread` did not start, when it did not.
    pub fn failed_start(&self, thread: ThreadId) -> Option<&str> {
        self.failed_starts.get(&thread).map(String::as_str)
    }

    /// Posts the project's draft after the start prompt is confirmed.
    fn send_project_draft(&mut self, project: ThreadId, cx: &mut Context<Self>) -> Option<u64> {
        let parent = Some(project.message);
        let text = self.draft(project.host, parent).trim().to_owned();
        if text.is_empty() {
            return None;
        }
        self.set_draft(project.host, parent, String::new());
        let id = self.post(project.host, parent, text, Vec::new(), cx);
        cx.emit(Event::DraftSent(project));
        Some(id)
    }

    /// Post the selected idea, independent of the box that owns its
    /// draft. The agent still starts from the logged mail, not its text.
    pub fn post_idea(
        &mut self,
        project: ThreadId,
        source: &IdeaSource,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        match source {
            IdeaSource::ChannelDraft => self.send_project_draft(project, cx),
            IdeaSource::Box(idea) => {
                let name = self.project_address(project)?;
                let id = self.post(
                    project.host,
                    Some(project.message),
                    idea.text.trim().to_owned(),
                    Vec::new(),
                    cx,
                );
                (idea.sent)(name, cx);
                Some(id)
            }
        }
    }

    fn project_root(&self, thread: ThreadId) -> Option<ThreadId> {
        let chat = self.chat(thread.host)?;
        chat.message(thread.message)?;
        let root = chat.root_of(thread.message);
        chat.project(root)?;
        Some(ThreadId {
            message: root,
            ..thread
        })
    }

    fn idea_project_at(&self, project: ThreadId) -> Option<IdeaProject> {
        let name = self.project_address(project)?;
        // The project must be on an attached host. Its cached address and
        // outbox stay usable while the connection is interrupted.
        self.link_for(project.host)?;
        Some(IdeaProject {
            address: project.address(),
            name,
        })
    }

    fn link_for(&self, endpoint: EndpointId) -> Option<Link> {
        self.hosts
            .values()
            .find(|slot| slot.endpoint == Some(endpoint))
            .map(|slot| slot.link.clone())
    }

    /// Posts the host's oldest unsent message, unless one is on its way.
    /// The rest go one by one as each is answered; a retry carries the
    /// same client id, so the host keeps one copy.
    fn send_unsent(&mut self, endpoint: EndpointId, cx: &mut Context<Self>) {
        let Some(link) = self.link_for(endpoint) else {
            return;
        };
        let Some(post) = self.outbox.next(endpoint) else {
            return;
        };
        let reply = link.run(move |dialer| async move { crate::stream::call(&dialer, post).await });
        cx.spawn(async move |this, cx| {
            let reply = reply.await;
            let _ = this.update(cx, |this, cx| match reply {
                Ok(message) => {
                    if let Some((client_id, outgoing, kept)) = this.outbox.delivered(endpoint) {
                        this.cache.write_outbox(endpoint, kept, None);
                        if outgoing.parent.is_none() {
                            let name = one_line(&content_text(&outgoing.content));
                            let project = crate::protocol::Project {
                                channel: message,
                                name,
                            };
                            this.frame.echo(
                                format!(
                                    "created channel: {}",
                                    project.address(&this.host_name(endpoint))
                                ),
                                StyleClass::SystemInfo,
                                cx,
                            );
                        }
                        cx.emit(Event::Delivered {
                            endpoint,
                            client_id,
                            message,
                        });
                    }
                    cx.notify();
                    this.send_unsent(endpoint, cx);
                }
                Err(error) => {
                    this.outbox.failed(endpoint);
                    this.frame
                        .echo(format!("chat: {error:#}"), StyleClass::SystemImportant, cx);
                }
            });
        })
        .detach();
    }

    /// One call to a host's chat; `done` hears the answer.
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

    /// The messages of others in a thread the user has not read, newest
    /// first: those past what they settled through or wrote after.
    pub(crate) fn unread_ids_in(
        &self,
        chat: &HostChat,
        thread: ThreadId,
        marks: &Marks,
    ) -> Vec<MessageId> {
        let facts = marks.get(&thread.address()).facts();
        if facts.muted() {
            return Vec::new();
        }
        let seen = facts
            .seen_topic()
            .map(|seen| MessageId(rho_agent_types::ChatSeq(seen)));
        let mut wrote = None;
        for id in chat.children(Some(thread.message)) {
            if let Some(message) = chat.message(*id)
                && message.from == Party::User
            {
                wrote = Some(*id);
            }
        }
        let seen = seen.max(wrote);
        chat.others_in(thread.message)
            .into_iter()
            .filter(|id| seen.is_none_or(|seen| *id > seen))
            .collect()
    }

    fn with_host<R>(
        &self,
        endpoint: EndpointId,
        _cx: &App,
        read: impl FnOnce(&cards::Host<'_>) -> R,
    ) -> Option<R> {
        let chat = self.chats.get(&endpoint)?;
        let name = self.host_name(endpoint);
        let host = cards::Host {
            endpoint,
            chat,
            name: &name,
        };
        Some(read(&host))
    }

    /// Posts a root on `endpoint`: a new channel.
    fn prompt_new_root(
        this: Entity<Self>,
        endpoint: EndpointId,
        frame: &mut rho_frame::Frame,
        window: &mut Window,
        cx: &mut Context<rho_frame::Frame>,
    ) {
        frame.open_prompt(
            "new channel:",
            Rc::new(|_, _, _| Vec::new()),
            Rc::new(move |_frame, input, _, cx| {
                let text = input.trim().to_owned();
                if text.is_empty() {
                    return;
                }
                this.update(cx, |chat, cx| {
                    chat.post(endpoint, None, text, Vec::new(), cx);
                });
            }),
            window,
            cx,
        );
    }
}

impl System for ChatSystem {
    fn kinds(&self) -> &'static [&'static str] {
        &[ThreadId::KIND]
    }

    fn open(
        &mut self,
        address: &Address,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnySurface> {
        let thread = self.thread_of(address)?;
        self.chats.get(&thread.host)?.message(thread.message)?;
        let system = cx.entity();
        let view = cx.new(|cx| ThreadView::new(self, system, thread, window, cx));
        Some(AnySurface::new(view))
    }

    fn active_surface(&mut self, address: Option<&Address>, _cx: &mut Context<Self>) {
        let thread = address
            .and_then(|address| self.thread_of(address))
            .filter(|thread| {
                self.chat(thread.host)
                    .is_some_and(|chat| !chat.is_project(thread.message))
            });
        self.set_view(thread);
    }

    fn streams(
        &mut self,
        host: HostId,
        name: &str,
        endpoint: Option<EndpointId>,
        link: &Link,
        _cx: &mut Context<Self>,
    ) -> Vec<Arc<dyn HostStream>> {
        let (view, incoming_view) = tokio::sync::watch::channel(None);
        let slot = Slot {
            name: name.to_owned(),
            link: link.clone(),
            endpoint,
            view,
        };
        Self::sync_view(&slot, self.view);
        self.hosts.insert(host, slot);
        vec![Arc::new(ChatStream::new(
            host,
            self.cursors.clone(),
            self.events.clone(),
            incoming_view,
        ))]
    }

    /// The host's threads and their screens go from the window with it. Its
    /// log is a copy the host still has, so the copy goes too and is read
    /// again if the host is attached again; what the user wrote and has
    /// not sent stays for then.
    fn detached(&mut self, host: HostId, cx: &mut Context<Self>) {
        let Some(endpoint) = self.hosts.remove(&host).and_then(|slot| slot.endpoint) else {
            cx.notify();
            return;
        };
        self.chats.remove(&endpoint);
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

    fn rows(&mut self, point: &Point, _window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        let this = cx.entity();
        let mut rows = Vec::new();
        let hosts = self.hosts();
        // The agent's palette uses `c` to compact its context.
        let on_agent = point
            .subject
            .as_ref()
            .is_some_and(|subject| subject.kind() == AgentRef::KIND);
        if !hosts.is_empty() && !on_agent {
            let new_root = this.clone();
            rows.push(Row::run("c", "new channel", move |frame, window, cx| {
                let hosts = new_root.read(cx).hosts();
                match hosts.as_slice() {
                    [(seed, _)] => {
                        Self::prompt_new_root(new_root.clone(), *seed, frame, window, cx)
                    }
                    _ => {
                        let this = new_root.clone();
                        let choices = hosts.clone();
                        frame.open_prompt(
                            "on host:",
                            Rc::new(move |_, needle, _| {
                                choices
                                    .iter()
                                    .filter(|(_, name)| name.contains(needle))
                                    .map(|(_, name)| Candidate {
                                        value: name.clone(),
                                        description: "host".to_owned(),
                                    })
                                    .collect()
                            }),
                            Rc::new(move |frame, input, window, cx| {
                                let Some((seed, _)) = this
                                    .read(cx)
                                    .hosts()
                                    .into_iter()
                                    .find(|(_, name)| *name == input.trim())
                                else {
                                    frame.echo("no such host", StyleClass::SystemInfo, cx);
                                    return;
                                };
                                Self::prompt_new_root(this.clone(), seed, frame, window, cx);
                            }),
                            window,
                            cx,
                        );
                    }
                }
            }));
        }
        // The thread under the point, or, on a line that is no message, the
        // thread in view.
        let thread = match &point.subject {
            Some(subject) => self.thread_of(subject),
            None => self.thread_of(&point.surface),
        };
        if let Some(thread) = thread {
            let current = self
                .chats
                .get(&thread.host)
                .and_then(|chat| chat.name(thread.message))
                .unwrap_or_default();
            rows.push(
                Row::run("shift-n", "name this thread", move |frame, window, cx| {
                    let this = this.clone();
                    let current = current.clone();
                    frame.open_prompt(
                        format!("name ({current}):"),
                        Rc::new(|_, _, _| Vec::new()),
                        Rc::new(move |_frame, input, _, cx| {
                            let name = input.trim().to_owned();
                            this.update(cx, |chat, cx| {
                                chat.call(
                                    thread.host,
                                    Name {
                                        thread: thread.message,
                                        name,
                                    },
                                    cx,
                                    |chat, reply, cx| {
                                        if let Err(error) = reply {
                                            chat.frame.echo(
                                                format!("name: {error:#}"),
                                                StyleClass::SystemInfo,
                                                cx,
                                            );
                                        }
                                    },
                                );
                            });
                        }),
                        window,
                        cx,
                    );
                })
                .also("rename"),
            );
        }
        rows
    }

    fn claims(
        &self,
        marks: &Marks,
        now: Timestamp,
        mut trace: Option<&mut Trace>,
        cx: &App,
    ) -> Vec<Claim> {
        let mut claims = Vec::new();
        let mut seeds: Vec<EndpointId> = self.chats.keys().copied().collect();
        seeds.sort_unstable();
        for seed in seeds {
            claims.extend(
                self.with_host(seed, cx, |host| {
                    cards::claims(host, marks, now, trace.as_deref_mut())
                })
                .unwrap_or_default(),
            );
        }
        claims
    }

    fn name(
        &self,
        address: &Address,
        _marks: &rho_dealer::Marks,
        cx: &App,
    ) -> Option<(String, String)> {
        let thread = self.thread_of(address)?;
        self.with_host(thread.host, cx, |host| cards::name(host, thread.message))?
    }

    /// A message is read in the thread it was posted under, where a private
    /// reply about it shows; a thread straight under a channel is read in
    /// its own screen, under its head.
    fn shown_in(&self, address: &Address, _cx: &App) -> Option<Address> {
        let thread = self.thread_of(address)?;
        let chat = self.chats.get(&thread.host)?;
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

    /// Through its newest message, and through the stops its agents are
    /// in when any is: a done on a stop's card sees the stop.
    fn seen(&self, address: &Address, _cx: &App) -> Seen {
        self.thread_of(address)
            .and_then(|thread| self.chats.get(&thread.host)?.newest_in(thread.message))
            .map_or(Seen::Whole, |(newest, _)| Seen::Topic(newest.0.0))
    }

    fn moving_threads(&self, _cx: &App) -> Vec<MovingThread> {
        self.chats
            .iter()
            .flat_map(|(&host, chat)| {
                chat.statuses().filter_map(move |status| {
                    let thread = ThreadId {
                        host,
                        message: status.thread,
                    };
                    let project = self.project_root(thread)?;
                    (project != thread).then_some(())?;
                    Some(MovingThread {
                        thread: thread.address(),
                        subject: chat.name(thread.message)?,
                        project: self.project_address(project)?,
                        status: status.text.clone(),
                        changed: Timestamp::from_millisecond(status.at.0.try_into().ok()?).ok()?,
                    })
                })
            })
            .collect()
    }

    fn idea_projects(&self, _cx: &App) -> Vec<IdeaProject> {
        self.chats
            .iter()
            .flat_map(|(&host, chat)| {
                chat.roots().iter().filter_map(move |&message| {
                    let thread = ThreadId { host, message };
                    let project = self.idea_project_at(thread)?;
                    // Like project_named, only show the newest of roots with
                    // the same address. A contextual default knows its exact
                    // id.
                    (self.project_named(&project.name) == Some(thread)).then_some(project)
                })
            })
            .collect()
    }

    fn idea_project(&self, address: &Address, _cx: &App) -> Option<IdeaProject> {
        let project = self.project_root(self.thread_of(address)?)?;
        self.idea_project_at(project)
    }

    fn last_idea_project(&self, _cx: &App) -> Option<IdeaProject> {
        self.idea_project_at(self.last_project?)
    }

    fn send_idea(&mut self, mail: IdeaMail, cx: &mut Context<Self>) {
        let Some(project) = self.thread_of(&mail.project) else {
            return;
        };
        cx.emit(Event::Idea {
            endpoint: project.host,
            parent: project.message,
            source: IdeaSource::Box(mail),
        });
    }

    fn find(&self, _marks: &Marks, hand: &Hand, _cx: &App) -> Vec<Found> {
        let mut found = Vec::new();
        for (seed, name) in self.hosts() {
            let Some(chat) = self.chats.get(&seed) else {
                continue;
            };
            for &root in chat.roots() {
                let project = chat.project(root).map(|project| project.address(&name));
                for &id in std::iter::once(&root).chain(chat.children(Some(root))) {
                    let thread = ThreadId {
                        host: seed,
                        message: id,
                    };
                    let address = thread.address();
                    let (_, at) = chat
                        .newest_in(id)
                        .unwrap_or((id, rho_agent_types::UnixMs(0)));
                    let title = chat.name(id).unwrap_or_default();
                    let (title, description) = match project.as_deref() {
                        Some(project) if id == root => (project.to_owned(), "project".to_owned()),
                        project => {
                            let detail = hand
                                .cards
                                .iter()
                                .find(|card| card.address == address)
                                .map(|card| card.label.as_str())
                                .filter(|label| !label.is_empty())
                                .or_else(|| self.status(thread).map(|status| status.text.as_str()));
                            let context = project.unwrap_or(&name);
                            let description = match detail {
                                Some(detail) => format!("{context}  {detail}"),
                                None => context.to_owned(),
                            };
                            (title, description)
                        }
                    };
                    found.push(Found {
                        address,
                        title,
                        description,
                        recency: at.0,
                    });
                }
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
    use super::*;

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

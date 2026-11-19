//! The chat as the frame shows it.
//!
//! [`ChatSystem`] is the one entity behind every chat surface: each host's
//! forest, the disk copy, the outbox, and the session to each host. It
//! plugs into the frame as a [`System`] under the `thread` kind: a root
//! message is a channel, a reply to a root is a thread, and every one of
//! them opens as a [`ThreadView`]. What of the chat wants the user is
//! [`crate::cards`]'s to say; Home's rows and `find`'s are made here.

mod thread;

use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Arc;

use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Task, Window};
use jiff::Timestamp;
use rho_agent_hosts::{HostId, HostStream, Link};
use rho_agent_types::{Address as Party, AgentId, ContentPart, Doing, EndpointId, MessageId};
use rho_dealer::facts::Seen;
use rho_dealer::rank::Trace;
use rho_dealer::{Address, Addressed as _, Claim, Marks};
use rho_frame::attention::Verdict;
use rho_frame::{AnySurface, Candidate, Found, FrameLink, Point, RoomRow, Row, System};
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
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

/// One attached host: what the user calls it, how to call it, and whose
/// log it carries once its session has said.
struct Slot {
    name: String,
    link: Link,
    endpoint: Option<EndpointId>,
}

/// An agent of a thread as the agents system tells the chat: what to
/// call it, and what it is doing as its host last said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Participant {
    pub label: String,
    /// `None` while its host has said nothing of it.
    pub doing: Option<Doing>,
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
    drafts: HashMap<(EndpointId, Option<MessageId>), String>,
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
            drafts: loaded
                .drafts
                .into_iter()
                .map(|(seed, parent, text)| ((seed, parent), text))
                .collect(),
            cache,
            hosts: HashMap::new(),
            chats,
            events,
            frame,
            participants: Rc::new(|_, agent, _| {
                Some(Participant {
                    label: agent.encoded(),
                    doing: None,
                })
            }),
            _pump: pump,
        }
    }

    /// Tells the chat which agents still exist, what to call them, and
    /// what they are doing: a thread whose agents are all gone raises no
    /// card, one whose agent stopped unbidden raises one, and a thread's
    /// participants read by label and state.
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

    /// The user steered an agent that is doing `doing`: the stop it is
    /// in, if any, is seen, and its thread's card for it goes. What else
    /// waits in the thread still waits. The agents system tells what the
    /// agent does, as it is the one steering.
    pub fn steered(&self, endpoint: EndpointId, agent: AgentId, doing: Doing, cx: &mut App) {
        let now = rho_agent_types::UnixMs::now();
        if doing.stop(now).is_none() {
            return;
        }
        let Some(thread) = self
            .chats
            .get(&endpoint)
            .and_then(|chat| chat.agent_thread(agent))
        else {
            return;
        };
        let node = ThreadId {
            host: endpoint,
            message: thread,
        }
        .address();
        self.frame.with(cx, move |frame, _, cx| {
            let message = frame.marks().get(&node).facts().seen_topic().unwrap_or(0);
            let seen = Seen::Thread {
                message,
                stopped: doing.since.0,
            };
            frame.write_marks(
                vec![rho_frame::attention::Write::Fact(
                    rho_dealer::facts::Fact::Read { node, seen },
                )],
                cx,
            );
        });
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

    /// Whether the window holds more than one host: when it does, a
    /// thread says which.
    fn many_hosts(&self) -> bool {
        self.chats.len() > 1
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
            ChatEvent::Closed { .. } => {}
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
        let content = vec![ContentPart::Text { text }];
        let (id, kept) = self.outbox.push(endpoint, parent, mentions, content);
        self.cache.write_outbox(endpoint, kept);
        self.send_unsent(endpoint, cx);
        cx.notify();
        id
    }

    /// Posts what the user has written under `parent` and clears the
    /// draft; the composer follows. `None` when nothing is written.
    pub fn send_draft(
        &mut self,
        endpoint: EndpointId,
        parent: Option<MessageId>,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let text = self.draft(endpoint, parent).trim().to_owned();
        if text.is_empty() {
            return None;
        }
        self.set_draft(endpoint, parent, String::new());
        Some(self.post(endpoint, parent, text, Vec::new(), cx))
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
                    if let Some((client_id, _, kept)) = this.outbox.delivered(endpoint) {
                        this.cache.write_outbox(endpoint, kept);
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

    /// How many messages of others in a thread the user has not read:
    /// those past what they settled through or wrote after.
    fn unread_in(&self, chat: &HostChat, thread: ThreadId, marks: &Marks) -> usize {
        self.unread_ids_in(chat, thread, marks).len()
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
        cx: &App,
        read: impl FnOnce(&cards::Host<'_>) -> R,
    ) -> Option<R> {
        let chat = self.chats.get(&endpoint)?;
        let name = self.host_name(endpoint);
        let alive = |agent: AgentId| self.participant(endpoint, agent, cx).is_some();
        let doing = |agent: AgentId| self.participant(endpoint, agent, cx)?.doing;
        let host = cards::Host {
            endpoint,
            chat,
            name: self.many_hosts().then_some(name.as_str()),
            alive: &alive,
            doing: &doing,
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

    fn streams(
        &mut self,
        host: HostId,
        name: &str,
        endpoint: Option<EndpointId>,
        link: &Link,
        _cx: &mut Context<Self>,
    ) -> Vec<Arc<dyn HostStream>> {
        self.hosts.insert(
            host,
            Slot {
                name: name.to_owned(),
                link: link.clone(),
                endpoint,
            },
        );
        vec![Arc::new(ChatStream::new(
            host,
            self.cursors.clone(),
            self.events.clone(),
        ))]
    }

    fn detached(&mut self, host: HostId, cx: &mut Context<Self>) {
        self.hosts.remove(&host);
        cx.notify();
    }

    fn rows(&mut self, point: &Point, _window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        let this = cx.entity();
        let mut rows = Vec::new();
        let hosts = self.hosts();
        // On an agent in a thread's `with` line the palette is the
        // agent's, whose `c` compacts its context.
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
        if let Some(thread) = point.subject.as_ref().and_then(|a| self.thread_of(a)) {
            let current = self
                .chats
                .get(&thread.host)
                .and_then(|chat| chat.name(thread.message))
                .unwrap_or_default();
            rows.push(Row::run(
                "shift-n",
                "name this thread",
                move |frame, window, cx| {
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
                },
            ));
        }
        // A verdict is about the card; about one message of it alone is
        // rare, so it is reached by name from the palette and has no binding.
        if let Some(message) = point
            .subject
            .clone()
            .filter(|subject| Some(subject) != point.card.as_ref())
            .filter(|subject| self.thread_of(subject).is_some())
        {
            let done = message.clone();
            rows.push(Row::run(
                "d",
                "done with this message",
                move |frame, window, cx| {
                    frame.verdict(&done, Verdict::Done, "done".to_owned(), window, cx)
                },
            ));
            rows.push(Row::run(
                "m",
                "mute this message",
                move |frame, window, cx| {
                    frame.verdict(&message, Verdict::Mute, "muted".to_owned(), window, cx)
                },
            ));
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

    fn name(&self, address: &Address, cx: &App) -> Option<(String, String)> {
        let thread = self.thread_of(address)?;
        self.with_host(thread.host, cx, |host| cards::name(host, thread.message))?
    }

    /// What is unread moved: a root's list reads the marks on its next
    /// draw, which this asks for.
    fn marks_moved(&mut self, _touched: &BTreeSet<Address>, cx: &mut Context<Self>) {
        cx.notify();
    }

    /// Through its newest message, and through the stops its agents are
    /// in when any is: a done on a stop's card sees the stop.
    fn seen(&self, address: &Address, cx: &App) -> Seen {
        let Some(thread) = self.thread_of(address) else {
            return Seen::Whole;
        };
        let Some((newest, _)) = self
            .chats
            .get(&thread.host)
            .and_then(|chat| chat.newest_in(thread.message))
        else {
            return Seen::Whole;
        };
        let stopped = self
            .with_host(thread.host, cx, |host| {
                cards::stops(host, Timestamp::now()).remove(&thread.message)
            })
            .flatten()
            .and_then(|stops| stops.iter().map(|stop| stop.since.0).max());
        match stopped {
            Some(stopped) => Seen::Thread {
                message: newest.0.0,
                stopped,
            },
            None => Seen::Topic(newest.0.0),
        }
    }

    /// Every root on every host, a room each: how many of its threads hold
    /// something unread, and when it last moved.
    fn rooms(&self, marks: &Marks, _cx: &App) -> Vec<RoomRow> {
        let now = Timestamp::now();
        let mut rows = Vec::new();
        for (seed, name) in self.hosts() {
            let Some(chat) = self.chats.get(&seed) else {
                continue;
            };
            for root in chat.roots() {
                let unread = chat
                    .children(Some(*root))
                    .iter()
                    .filter(|id| {
                        self.unread_in(
                            chat,
                            ThreadId {
                                host: seed,
                                message: **id,
                            },
                            marks,
                        ) > 0
                    })
                    .count();
                let (_, at) = chat
                    .newest_in(*root)
                    .unwrap_or((*root, rho_agent_types::UnixMs(0)));
                rows.push(RoomRow {
                    address: ThreadId {
                        host: seed,
                        message: *root,
                    }
                    .address(),
                    name: chat.name(*root).unwrap_or_default(),
                    tag: name.clone(),
                    waiting: match unread {
                        0 => String::new(),
                        n => format!("{n} unread · {}", ago(at.0, now)),
                    },
                    at: at.0,
                });
            }
        }
        rows
    }

    fn find(&self, _marks: &Marks, _cx: &App) -> Vec<Found> {
        let mut found = Vec::new();
        for (seed, name) in self.hosts() {
            let Some(chat) = self.chats.get(&seed) else {
                continue;
            };
            for root in chat.roots() {
                let root_name = chat.name(*root).unwrap_or_default();
                let mut threads = vec![*root];
                threads.extend(chat.children(Some(*root)).iter().copied());
                for id in threads {
                    let (_, at) = chat
                        .newest_in(id)
                        .unwrap_or((id, rho_agent_types::UnixMs(0)));
                    found.push(Found {
                        address: ThreadId {
                            host: seed,
                            message: id,
                        }
                        .address(),
                        title: chat.name(id).unwrap_or_default(),
                        description: match id == *root {
                            true => format!("{name} · channel"),
                            false => format!("{name} › {root_name}"),
                        },
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

/// A message's time on the user's clock: the hour when it is today, the
/// day in front of it when it is not.
pub(crate) fn when(at_ms: u64) -> String {
    let at = Timestamp::from_millisecond(at_ms as i64)
        .unwrap_or(Timestamp::UNIX_EPOCH)
        .to_zoned(jiff::tz::TimeZone::system());
    let now = jiff::Zoned::now();
    match at.date() == now.date() {
        true => at.strftime("%H:%M").to_string(),
        false => at.strftime("%a %-d %b %H:%M").to_string(),
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

//! One agent: its log, its notebook, and the loop that wakes the model.
//!
//! The loop does three things, over and over: record what arrives (messages
//! from outside, and what the notebook sends out), ask [`wake::decide`]
//! whether the model should look, and if so wake it with a report and run
//! the cell it answers with.

use std::sync::Arc;
use std::time::Duration;

use rho_agent_types::UnixMs;
use rho_inference2::{CacheKey, Call, CallId, Carry, Image, Model, Stream, Usage};
use rho_notebook2::{CellHandle, Notebook};
use tokio::sync::{Notify, broadcast, mpsc, oneshot, watch};

use crate::chat::{self, ChatEvent};
use crate::human::{Agent2HostCall, Mailroom, Outbound};
use crate::log::{AgentId, Block, Entry, Log, MessageId, Notice, Party, Wake};
use crate::wake::{self, Decision, Facts};

/// Failed model requests in a row before the agent stops until the human
/// writes.
const MAX_FAILURES: u32 = 3;
/// Steps in a row without a call before the same.
const MAX_PROSE: u32 = 3;

/// A message from outside: the human, or another agent.
#[derive(Clone, Debug)]
pub struct Inbound {
    pub from: Party,
    pub body: Vec<Block>,
}

/// What a side pane can watch live: the model's work, which is never synced.
#[derive(Clone, Debug)]
pub enum Trace {
    Woken {
        why: Wake,
        report: String,
    },
    Step {
        code: Option<String>,
        prose: String,
    },
    ArchiveState {
        archived: bool,
    },
    /// An interrupted turn stopped without a model step.
    Settled,
}

/// Talks to a running agent. Dropping every handle stops it.
#[derive(Clone)]
pub struct AgentHandle {
    inbox: mpsc::UnboundedSender<Inbound>,
    control: mpsc::UnboundedSender<Control>,
    stop: watch::Sender<bool>,
    cancel: watch::Sender<u64>,
    chat: broadcast::Sender<ChatEvent>,
    trace: broadcast::Sender<Trace>,
    mailroom: Arc<Mailroom>,
}

impl AgentHandle {
    pub fn send(&self, inbound: Inbound) -> anyhow::Result<()> {
        self.inbox
            .send(inbound)
            .map_err(|_| anyhow::anyhow!("the agent has stopped"))
    }

    /// Chat events from now on; the chat so far is [`Agent::chat`].
    pub fn chat(&self) -> broadcast::Receiver<ChatEvent> {
        self.chat.subscribe()
    }

    pub fn archive(&self) {
        self.mailroom.archive();
    }

    /// Stop the loop and let it drain its notebook before worker shutdown.
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }

    /// Interrupt this turn and its running notebook work; later messages may
    /// resume it.
    pub fn cancel(&self) {
        self.cancel.send_modify(|generation| *generation += 1);
    }

    /// Record and deliver a message from this agent through its own chat log.
    pub fn send_to(&self, to: Party, text: String) -> anyhow::Result<()> {
        self.mailroom.send_to(to, text)
    }

    /// Branch before the Nth last human message; notebook state remains.
    pub async fn rewind(&self, turns: u32) -> anyhow::Result<()> {
        let (reply, result) = oneshot::channel();
        self.control
            .send(Control::Rewind { turns, reply })
            .map_err(|_| anyhow::anyhow!("the agent has stopped"))?;
        result
            .await
            .map_err(|_| anyhow::anyhow!("the agent has stopped"))?
    }

    pub fn trace(&self) -> broadcast::Receiver<Trace> {
        self.trace.subscribe()
    }
}

enum Control {
    Rewind {
        turns: u32,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
}

pub struct Agent {
    id: AgentId,
    log: Log,
    instructions: Arc<str>,
    model: Arc<Model>,
    notebook: Notebook,
    shell: rho_tool_shell::ShellTools,
    archived: bool,
    fresh: bool,
    responding: bool,
    mailroom: Arc<Mailroom>,
    outbox: mpsc::UnboundedReceiver<Outbound>,
    inbox: mpsc::UnboundedReceiver<Inbound>,
    control: mpsc::UnboundedReceiver<Control>,
    stop: watch::Receiver<bool>,
    cancel: watch::Receiver<u64>,
    wake: Arc<Notify>,
    chat: broadcast::Sender<ChatEvent>,
    trace: broadcast::Sender<Trace>,
    /// The latest cell. Older ones live on in the notebook's sources.
    cell: Option<CellHandle>,
    /// The latest step was cut off part-way through its cell.
    interrupted: bool,
    /// The model has been told the latest task finished.
    told_returned: bool,
    /// Messages the model has not seen, oldest first.
    unread: Vec<(MessageId, Party, UnixMs)>,
    last_step: Option<UnixMs>,
    awaiting: bool,
    prose: u32,
    restarted: bool,
    rewound: bool,
    stopped: bool,
    cache_key: CacheKey,
}

/// The call of the step in progress, as its code arrives.
struct Streaming {
    id: CallId,
    code: String,
}

pub struct Config {
    pub id: AgentId,
    pub log: Log,
    pub model: Arc<Model>,
    pub shell: rho_tool_shell::ShellTools,
    pub instructions: Arc<str>,
    pub agent_tools: Option<Agent2HostCall>,
}

impl Agent {
    /// An agent over `config.log`, carrying on from whatever it holds. Must
    /// be called inside a Tokio runtime.
    pub fn new(config: Config) -> anyhow::Result<(Self, AgentHandle)> {
        let (mailroom, outbox) = Mailroom::new(config.agent_tools);
        let wake = Arc::new(Notify::new());
        let notebook = Notebook::new(config.shell.clone(), mailroom.exports(), Arc::clone(&wake))
            .map_err(|error| anyhow::anyhow!("the notebook failed to start: {error}"))?;
        let (inbox_tx, inbox) = mpsc::unbounded_channel();
        let (control_tx, control) = mpsc::unbounded_channel();
        let (stop_tx, stop) = watch::channel(false);
        let (cancel_tx, cancel) = watch::channel(0);
        let (chat, _) = broadcast::channel(1024);
        let (trace, _) = broadcast::channel(1024);
        let mut agent = Self {
            id: config.id,
            log: config.log,
            instructions: config.instructions,
            model: config.model,
            notebook,
            shell: config.shell,
            archived: false,
            fresh: false,
            responding: false,
            mailroom,
            outbox,
            inbox,
            control,
            stop,
            cancel,
            wake,
            chat: chat.clone(),
            trace: trace.clone(),
            cell: None,
            interrupted: false,
            told_returned: false,
            unread: Vec::new(),
            last_step: None,
            awaiting: false,
            prose: 0,
            restarted: false,
            rewound: false,
            stopped: false,
            cache_key: CacheKey::new(),
        };
        agent.resume()?;
        let handle_mailroom = Arc::clone(&agent.mailroom);
        Ok((
            agent,
            AgentHandle {
                inbox: inbox_tx,
                control: control_tx,
                stop: stop_tx,
                cancel: cancel_tx,
                chat,
                trace,
                mailroom: handle_mailroom,
            },
        ))
    }

    /// Pick up from the log: what the model has not seen, and whether there
    /// was a notebook that is now gone.
    fn resume(&mut self) -> anyhow::Result<()> {
        let mut delivered = std::collections::HashSet::new();
        // Any wake may have run code: streamed code runs before its step is
        // logged.
        let mut woken = false;
        let mut awaiting = false;
        let visible = self.log.visible_positions();
        for &position in &visible {
            let entry = &self.log.entries()[position];
            match entry {
                Entry::Created { cache_key, .. } => self.cache_key = *cache_key,
                Entry::Woken { messages, .. } => {
                    woken = true;
                    delivered.extend(messages.iter().copied());
                }
                Entry::Awaiting { since, .. } => awaiting = since.is_some(),
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
        for &position in &visible {
            let entry = &self.log.entries()[position];
            if let Entry::Received { at, id, from, .. } = entry
                && !delivered.contains(id)
            {
                self.unread.push((*id, from.clone(), *at));
                if *from == Party::Human {
                    self.mailroom.received();
                }
            }
        }
        if self.log.entries().is_empty() {
            self.cache_key = CacheKey::new();
            self.append(Entry::Created {
                at: UnixMs::now(),
                cache_key: self.cache_key,
            })?;
        } else if woken && !self.archived {
            self.restarted = true;
            self.append(Entry::Notice {
                at: UnixMs::now(),
                notice: Notice::Restarted,
            })?;
        }
        if awaiting {
            // Whatever awaited the human went with the old notebook.
            self.append(Entry::Awaiting {
                at: UnixMs::now(),
                since: None,
            })?;
        }
        Ok(())
    }

    /// The chat so far.
    pub fn chat(&self) -> Vec<ChatEvent> {
        chat::chat(&self.id, self.log.entries())
    }

    /// The requested history branch is represented by a new physical log row,
    /// never by deleting the abandoned branch (DECISION-history-only-branches).
    fn rewind(&mut self, turns: u32) -> anyhow::Result<()> {
        anyhow::ensure!(turns > 0, "rewind turns must be greater than zero");
        self.drain()?;
        let human_positions: Vec<_> = self
            .log
            .visible_positions()
            .into_iter()
            .filter(|&position| {
                matches!(
                    self.log.entries()[position],
                    Entry::Received {
                        from: Party::Human,
                        ..
                    }
                )
            })
            .collect();
        anyhow::ensure!(!human_positions.is_empty(), "nothing to rewind");
        let index = human_positions.len().saturating_sub(turns as usize);
        let to = human_positions[index] as u64;
        self.append(Entry::Rewound {
            at: UnixMs::now(),
            to,
        })?;
        self.unread.clear();
        self.last_step = None;
        self.cell = None;
        self.told_returned = false;
        self.interrupted = false;
        self.responding = false;
        self.awaiting = false;
        self.prose = 0;
        self.stopped = false;
        self.restarted = false;
        self.rewound = true;
        // The notebook, its Python globals, and any side effects are retained.
        // Rewound human messages are intentionally absent, as on the old branch.
        Ok(())
    }

    pub fn log(&self) -> &Log {
        &self.log
    }

    /// Run until every handle is dropped.
    pub async fn run(mut self) -> anyhow::Result<()> {
        loop {
            if *self.stop.borrow() {
                break;
            }
            // A cell can archive itself as it completes. Apply its outbound
            // notice before deciding whether its completion warrants a wake.
            self.drain()?;
            match wake::decide(&self.facts(), UnixMs::now()) {
                Decision::Now(why) => self.wake_model(why).await?,
                Decision::Later(recheck) => {
                    let sleep = async {
                        match recheck {
                            Some(at) => tokio::time::sleep(until(at)).await,
                            None => std::future::pending().await,
                        }
                    };
                    tokio::select! {
                        _ = self.stop.changed() => {},
                        _ = self.cancel.changed() => self.interrupt(None)?,
                        control = self.control.recv() => match control {
                            Some(Control::Rewind { turns, reply }) => { let _ = reply.send(self.rewind(turns)); },
                            None => break,
                        },
                        inbound = self.inbox.recv() => match inbound {
                            Some(inbound) => self.receive(inbound)?,
                            None => break,
                        },
                        Some(outbound) = self.outbox.recv() => self.outbound(outbound)?,
                        () = self.wake.notified() => {}
                        () = sleep => {}
                    }
                }
            }
        }
        self.notebook.shutdown().await.map_err(anyhow::Error::msg)
    }

    fn append(&mut self, entry: Entry) -> anyhow::Result<()> {
        let seq = self.log.entries().len() as u64;
        let event = chat::project(&self.id, seq, &entry);
        let archived = match &entry {
            Entry::Notice {
                notice: Notice::Archived,
                ..
            } => Some(true),
            Entry::Notice {
                notice: Notice::FreshNotebook,
                ..
            } => Some(false),
            _ => None,
        };
        self.log.append(entry)?;
        if let Some(archived) = archived {
            let _ = self.trace.send(Trace::ArchiveState { archived });
        }
        if let Some(event) = event {
            let _ = self.chat.send(event);
        }
        Ok(())
    }

    fn receive(&mut self, inbound: Inbound) -> anyhow::Result<()> {
        let at = UnixMs::now();
        let id = MessageId::new();
        if inbound.from == Party::Human {
            if self.archived && !self.responding {
                self.fresh_notebook(at)?;
            }
            self.mailroom.received();
            self.stopped = false;
        } else {
            self.mailroom.agent_received();
        }
        self.unread.push((id, inbound.from.clone(), at));
        self.append(Entry::Received {
            at,
            id,
            from: inbound.from,
            body: inbound.body,
        })
    }

    fn fresh_notebook(&mut self, at: UnixMs) -> anyhow::Result<()> {
        self.notebook = Notebook::new(
            self.shell.clone(),
            self.mailroom.exports(),
            Arc::clone(&self.wake),
        )
        .map_err(anyhow::Error::msg)?;
        self.archived = false;
        self.fresh = true;
        self.cell = None;
        self.told_returned = false;
        self.append(Entry::Notice {
            at,
            notice: Notice::FreshNotebook,
        })
    }

    fn outbound(&mut self, outbound: Outbound) -> anyhow::Result<()> {
        let at = UnixMs::now();
        match outbound {
            Outbound::Send { to, text } => self.append(Entry::Sent {
                at,
                id: MessageId::new(),
                to,
                text,
            }),
            Outbound::Status(text) => self.append(Entry::Status { at, text }),
            Outbound::Archive => {
                self.archived = true;
                self.notebook.cancel();
                self.append(Entry::Notice {
                    at,
                    notice: Notice::Archived,
                })
            }
            Outbound::Awaiting(awaiting) if awaiting != self.awaiting => {
                self.awaiting = awaiting;
                self.append(Entry::Awaiting {
                    at,
                    since: awaiting.then_some(at),
                })
            }
            Outbound::Awaiting(_) => Ok(()),
        }
    }

    fn drain(&mut self) -> anyhow::Result<()> {
        while let Ok(outbound) = self.outbox.try_recv() {
            self.outbound(outbound)?;
        }
        while let Ok(inbound) = self.inbox.try_recv() {
            self.receive(inbound)?;
        }
        Ok(())
    }

    fn facts(&self) -> Facts {
        let sources = self.notebook.facts();
        let latest = self
            .cell
            .as_ref()
            .and_then(|cell| sources.iter().find(|s| s.session_id == cell.session_id()));
        let (wait, wake_on_tools) = self.notebook.checkin();
        let finished = latest
            .and_then(|facts| facts.finished)
            .filter(|end| !end.failed && !self.told_returned)
            .map(|end| end.at);
        Facts {
            human: self
                .unread
                .iter()
                .find(|(_, from, _)| *from == Party::Human)
                .map(|(_, _, at)| *at),
            agent: self
                .unread
                .iter()
                .find(|(_, from, _)| *from != Party::Human)
                .map(|(_, _, at)| *at),
            finished,
            notified: sources
                .iter()
                .filter_map(|facts| facts.notified_at.into_iter().chain(facts.paged_at).min())
                .min(),
            failure: sources
                .iter()
                .filter(|facts| !facts.delivered && facts.finished.is_some_and(|end| end.failed))
                .filter_map(|facts| facts.finished.map(|end| end.at))
                .min(),
            checkin: self.last_step.map(|at| at + wait),
            response_finished: self.last_step,
            wake_on_tools,
            prose: self.prose > 0,
            restarted: self.restarted,
            rewound: self.rewound,
            archived: self.archived,
            prose_silenced: self.stopped,
        }
    }

    async fn wake_model(&mut self, why: Wake) -> anyhow::Result<()> {
        self.drain()?;
        if self.archived {
            return Ok(());
        }
        let mut lines = Vec::new();
        match why {
            Wake::Rewound => lines.push(
                "The human rewound your visible history. Your Python notebook, running work, and side effects were not rewound. Check the current state before continuing."
                    .to_owned(),
            ),
            Wake::Restarted => lines.push(
                "rho restarted. Your notebook and everything running in it are gone, and \
                 their side effects may remain. Check the current state before carrying on."
                    .to_owned(),
            ),
            Wake::Prose => lines.push(
                "Your last response had no exec call. Text outside a call reaches nobody: \
                 speak with human.send()."
                    .to_owned(),
            ),
            _ => {}
        }
        if std::mem::take(&mut self.fresh) {
            lines.push("This agent was archived. You have a fresh notebook; earlier Python state and running work are gone.".to_owned());
        }
        if std::mem::take(&mut self.interrupted) {
            lines.push(
                "Your response was cut off while you were writing its cell; only the code \
                 shown ran. Carry on from the notebook's state without replaying it."
                    .to_owned(),
            );
        }
        let mut images = Vec::new();
        if let Some(report) = self.notebook.report() {
            lines.push(report.text);
            images.extend(report.images.into_iter().map(|image| Image {
                media_type: image.media_type,
                data: image.data,
            }));
        }
        self.wake_with(why, lines, images).await
    }

    async fn wake_with(
        &mut self,
        why: Wake,
        mut lines: Vec<String>,
        images: Vec<Image>,
    ) -> anyhow::Result<()> {
        if self.cell.is_some() {
            self.told_returned = true;
        }
        let messages = std::mem::take(&mut self.unread);
        let humans = messages
            .iter()
            .filter(|(_, from, _)| *from == Party::Human)
            .count();
        self.mailroom.read(humans as u64);
        if lines.is_empty() {
            lines.push(
                if !messages.is_empty() {
                    "New messages below."
                } else if why == Wake::Checkin {
                    "Check-in: nothing new."
                } else {
                    "Nothing new."
                }
                .to_owned(),
            );
        }
        let report = lines.join("\n\n");
        let _ = self.trace.send(Trace::Woken {
            why: why.clone(),
            report: report.clone(),
        });
        self.append(Entry::Woken {
            at: UnixMs::now(),
            why,
            report,
            images,
            messages: messages.into_iter().map(|(id, _, _)| id).collect(),
        })?;
        self.restarted = false;
        self.rewound = false;
        self.notebook.reset_checkin();
        let request = crate::context::request(
            Arc::clone(&self.instructions),
            self.log.entries(),
            self.cache_key,
        );

        self.responding = true;
        let mut failures = 0;
        let step = loop {
            let model = Arc::clone(&self.model);
            // The call's code, as it arrives, runs as it arrives.
            let (code_tx, mut code_rx) = mpsc::unbounded_channel();
            let mut streaming = None;
            let result = {
                let mut forward = move |piece: Stream<'_>| {
                    let _ = code_tx.send(match piece {
                        Stream::Call { id } => (Some(id.clone()), String::new()),
                        Stream::Code(code) => (None, code.to_owned()),
                    });
                };
                let step = model.step(&request, &mut forward);
                tokio::pin!(step);
                // Keep recording what arrives while the model writes.
                loop {
                    tokio::select! {
                        _ = self.stop.changed() => return Ok(()),
                        _ = self.cancel.changed() => {
                            self.interrupt(streaming)?;
                            return Ok(());
                        },
                        result = &mut step => break result,
                        Some(piece) = code_rx.recv() => self.stream(&mut streaming, piece),
                        Some(inbound) = self.inbox.recv() => self.receive(inbound)?,
                        Some(outbound) = self.outbox.recv() => self.outbound(outbound)?,
                    }
                }
            };
            while let Ok(piece) = code_rx.try_recv() {
                self.stream(&mut streaming, piece);
            }
            match result {
                Ok(step) => break Ok((step, streaming)),
                Err(error) => {
                    self.append(Entry::Notice {
                        at: UnixMs::now(),
                        notice: Notice::Error(format!("{error:#}")),
                    })?;
                    // Code that already ran cannot be taken back: it stands
                    // as the step, and the model hears it was cut off.
                    if let (Some(cell), Some(streaming)) = (&self.cell, streaming)
                        && let Some(ran) = cell.interrupt()
                    {
                        break Err(Call {
                            id: streaming.id,
                            code: streaming.code[..ran.min(streaming.code.len())].to_owned(),
                        });
                    }
                    failures += 1;
                    if failures >= MAX_FAILURES {
                        return Err(error);
                    }
                    tokio::select! {
                        _ = self.stop.changed() => return Ok(()),
                        _ = tokio::time::sleep(Duration::from_secs(2u64.pow(failures))) => {},
                    }
                }
            }
        };
        self.responding = false;
        let at = UnixMs::now();
        self.last_step = Some(at);
        let (step, streaming) = match step {
            Ok(step) => step,
            Err(call) => {
                self.interrupted = true;
                self.prose = 0;
                let _ = self.trace.send(Trace::Step {
                    code: Some(call.code.clone()),
                    prose: String::new(),
                });
                self.append(Entry::Step {
                    at,
                    call: Some(call.clone()),
                    prose: String::new(),
                    carry: Carry::bare(call),
                    usage: Usage::default(),
                })?;
                if self.archived && self.unread.iter().any(|(_, from, _)| *from == Party::Human) {
                    self.fresh_notebook(at)?;
                }
                return Ok(());
            }
        };
        let _ = self.trace.send(Trace::Step {
            code: step.call.as_ref().map(|call| call.code.clone()),
            prose: step.prose.clone(),
        });
        self.append(Entry::Step {
            at,
            call: step.call.clone(),
            prose: step.prose,
            carry: step.carry,
            usage: step.usage,
        })?;
        match (step.call, streaming) {
            (Some(call), Some(streaming)) => {
                self.prose = 0;
                let cell = self.cell.as_ref().expect("a streamed call has a cell");
                // Whatever the stream missed, then the end.
                let rest = call.code.strip_prefix(&streaming.code).unwrap_or_default();
                let _ = cell.feed(rest.to_owned(), true);
            }
            (Some(call), None) => {
                self.prose = 0;
                self.cell = Some(self.notebook.run(call.code));
                self.told_returned = false;
            }
            (None, streaming) => {
                if streaming.is_some()
                    && let Some(cell) = &self.cell
                {
                    cell.stop();
                }
                self.prose += 1;
                if self.prose >= MAX_PROSE {
                    self.prose = 0;
                    self.stopped = true;
                }
            }
        }
        if self.archived && self.unread.iter().any(|(_, from, _)| *from == Party::Human) {
            self.fresh_notebook(at)?;
        }
        Ok(())
    }

    fn interrupt(&mut self, streaming: Option<Streaming>) -> anyhow::Result<()> {
        self.notebook.cancel();
        self.cell = None;
        self.responding = false;
        self.stopped = true;
        let _ = self.trace.send(Trace::Settled);
        if let Some(streaming) = streaming {
            self.interrupted = true;
            let call = Call {
                id: streaming.id,
                code: streaming.code,
            };
            self.append(Entry::Step {
                at: UnixMs::now(),
                call: Some(call.clone()),
                prose: String::new(),
                carry: Carry::bare(call),
                usage: Usage::default(),
            })?;
        }
        Ok(())
    }

    /// A piece of the call being written: its start opens a cell, its code
    /// feeds it.
    fn stream(&mut self, streaming: &mut Option<Streaming>, (id, code): (Option<CallId>, String)) {
        if let Some(id) = id {
            self.cell = Some(self.notebook.stream());
            self.told_returned = false;
            *streaming = Some(Streaming {
                id,
                code: String::new(),
            });
        }
        if let (Some(streaming), Some(cell)) = (streaming.as_mut(), &self.cell)
            && !code.is_empty()
        {
            streaming.code.push_str(&code);
            let _ = cell.feed(code, false);
        }
    }
}

fn until(at: UnixMs) -> Duration {
    Duration::from_millis(at.0.saturating_sub(UnixMs::now().0))
}

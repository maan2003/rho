//! One agent: its log, its notebook, and the loop that wakes the model.
//!
//! The loop does three things, over and over: record what arrives (messages
//! from outside, and what the notebook sends out), ask [`wake::decide`]
//! whether the model should look, and if so wake it with a report and run
//! the cell it answers with.

use std::sync::Arc;
use std::time::Duration;

use rho_agent::python::{PythonExec, PythonNotebook};
use rho_agent_types::UnixMs;
use rho_inference::types::{ExecCall, ExecId};
use rho_inference2::{Image, Model};
use tokio::sync::{Notify, broadcast, mpsc};

use crate::chat::{self, ChatEvent};
use crate::human::{Mailroom, Outbound};
use crate::log::{Block, Entry, Log, MessageId, Notice, Party, Wake};
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
    Woken { why: Wake, report: String },
    Step { code: Option<String>, prose: String },
}

/// Talks to a running agent. Dropping every handle stops it.
#[derive(Clone)]
pub struct AgentHandle {
    inbox: mpsc::UnboundedSender<Inbound>,
    chat: broadcast::Sender<ChatEvent>,
    trace: broadcast::Sender<Trace>,
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

    pub fn trace(&self) -> broadcast::Receiver<Trace> {
        self.trace.subscribe()
    }
}

pub struct Agent {
    id: String,
    log: Log,
    instructions: Arc<str>,
    model: Arc<dyn Model>,
    notebook: PythonNotebook,
    mailroom: Arc<Mailroom>,
    outbox: mpsc::UnboundedReceiver<Outbound>,
    inbox: mpsc::UnboundedReceiver<Inbound>,
    wake: Arc<Notify>,
    chat: broadcast::Sender<ChatEvent>,
    trace: broadcast::Sender<Trace>,
    /// Live cells, oldest first; the last is the latest.
    cells: Vec<Arc<PythonExec>>,
    /// The model has been told the latest cell returned.
    told_returned: bool,
    /// Messages the model has not seen, oldest first.
    unread: Vec<(MessageId, Party, UnixMs)>,
    last_step: Option<UnixMs>,
    awaiting: bool,
    prose: u32,
    restarted: bool,
    stopped: bool,
    cache_key: u128,
    next_cell: u64,
}

pub struct Config {
    pub id: String,
    pub log: Log,
    pub model: Arc<dyn Model>,
    pub shell: rho_tool_shell::ShellTools,
    pub instructions: Arc<str>,
}

impl Agent {
    /// An agent over `config.log`, carrying on from whatever it holds. Must
    /// be called inside a Tokio runtime.
    pub fn new(config: Config) -> anyhow::Result<(Self, AgentHandle)> {
        let (mailroom, outbox) = Mailroom::new();
        let notebook = PythonNotebook::new(config.shell, mailroom.exports())
            .map_err(|error| anyhow::anyhow!("the notebook failed to start: {error}"))?;
        let (inbox_tx, inbox) = mpsc::unbounded_channel();
        let (chat, _) = broadcast::channel(1024);
        let (trace, _) = broadcast::channel(1024);
        let mut agent = Self {
            id: config.id,
            log: config.log,
            instructions: config.instructions,
            model: config.model,
            notebook,
            mailroom,
            outbox,
            inbox,
            wake: Arc::new(Notify::new()),
            chat: chat.clone(),
            trace: trace.clone(),
            cells: Vec::new(),
            told_returned: false,
            unread: Vec::new(),
            last_step: None,
            awaiting: false,
            prose: 0,
            restarted: false,
            stopped: false,
            cache_key: 0,
            next_cell: 0,
        };
        agent.resume()?;
        Ok((
            agent,
            AgentHandle {
                inbox: inbox_tx,
                chat,
                trace,
            },
        ))
    }

    /// Pick up from the log: what the model has not seen, and whether there
    /// was a notebook that is now gone.
    fn resume(&mut self) -> anyhow::Result<()> {
        let mut delivered = std::collections::HashSet::new();
        let mut had_steps = false;
        let mut awaiting = false;
        for entry in self.log.entries() {
            match entry {
                Entry::Created { cache_key, .. } => self.cache_key = *cache_key,
                Entry::Step { .. } => had_steps = true,
                Entry::Woken { messages, .. } => delivered.extend(messages.iter().copied()),
                Entry::Awaiting { since, .. } => awaiting = since.is_some(),
                _ => {}
            }
        }
        for entry in self.log.entries() {
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
            self.cache_key = uuid::Uuid::new_v4().as_u128();
            self.append(Entry::Created {
                at: UnixMs::now(),
                cache_key: self.cache_key,
            })?;
        } else if had_steps {
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

    pub fn log(&self) -> &Log {
        &self.log
    }

    /// Run until every handle is dropped.
    pub async fn run(mut self) -> anyhow::Result<()> {
        loop {
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
        for cell in &self.cells {
            cell.cancel();
        }
        self.notebook.shutdown().await.map_err(anyhow::Error::msg)
    }

    fn append(&mut self, entry: Entry) -> anyhow::Result<()> {
        let seq = self.log.entries().len() as u64;
        let event = chat::project(&self.id, seq, &entry);
        self.log.append(entry)?;
        if let Some(event) = event {
            let _ = self.chat.send(event);
        }
        Ok(())
    }

    fn receive(&mut self, inbound: Inbound) -> anyhow::Result<()> {
        let at = UnixMs::now();
        let id = MessageId::new();
        if inbound.from == Party::Human {
            self.mailroom.received();
            self.stopped = false;
        }
        self.unread.push((id, inbound.from.clone(), at));
        self.append(Entry::Received {
            at,
            id,
            from: inbound.from,
            body: inbound.body,
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
        let latest = self.cells.last().map(|cell| cell.facts());
        let checkin = latest
            .and_then(|facts| facts.checkin)
            .map(|checkin| (checkin.after, checkin.wake_on_tools));
        let after = match checkin {
            Some((after, _)) => Some(after),
            // Nobody needs looking in on while they only wait for the human.
            None if self.awaiting => None,
            None => Some(wake::DEFAULT_CHECKIN),
        };
        let jobs = self.cells.iter().flat_map(|cell| cell.jobs());
        Facts {
            message: self.unread.first().map(|(_, _, at)| *at),
            returned: latest
                .and_then(|facts| facts.returned)
                .filter(|_| !self.told_returned),
            notified: self
                .cells
                .iter()
                .filter_map(|cell| cell.facts().notified_at)
                .min(),
            ended: jobs.filter_map(|job| job.finished).map(|end| end.at).min(),
            checkin: self.last_step.zip(after).map(|(at, after)| at + after),
            wake_on_tools: checkin.is_none_or(|(_, wake)| wake),
            prose: self.prose > 0,
            restarted: self.restarted,
            stopped: self.stopped,
        }
    }

    /// Everything the notebook has to say: the latest cell's output first,
    /// then each older cell's, named by its first line. Cells with nothing
    /// left to say are forgotten.
    fn report(&mut self) -> (Vec<String>, Vec<Image>) {
        let mut parts = Vec::new();
        let mut images = Vec::new();
        let latest = self.cells.len().saturating_sub(1);
        for (index, cell) in self.cells.iter().enumerate().rev() {
            let Some(output) = cell.more_output() else {
                continue;
            };
            cell.acknowledge_output();
            if index == latest {
                parts.insert(0, output.output.to_string());
            } else {
                parts.push(format!("From an earlier cell:\n{}", output.output));
            }
            images.extend(output.images.iter().map(|image| Image {
                media_type: image.media_type.clone(),
                data: image.data.clone(),
            }));
        }
        self.cells.retain(|cell| {
            let done = cell.done();
            if done {
                cell.release();
            }
            !done
        });
        (parts, images)
    }

    async fn wake_model(&mut self, why: Wake) -> anyhow::Result<()> {
        self.drain()?;
        let mut lines = Vec::new();
        match why {
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
        let (parts, images) = self.report();
        lines.extend(parts);
        self.wake_with(why, lines, images).await
    }

    async fn wake_with(
        &mut self,
        why: Wake,
        mut lines: Vec<String>,
        images: Vec<Image>,
    ) -> anyhow::Result<()> {
        if let Some(latest) = self.cells.last() {
            self.told_returned = latest.facts().returned.is_some();
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
        let request = crate::context::request(
            Arc::clone(&self.instructions),
            self.log.entries(),
            self.cache_key,
        );

        let mut failures = 0;
        let step = loop {
            let model = Arc::clone(&self.model);
            let result = {
                let step = model.step(&request);
                tokio::pin!(step);
                // Keep recording what arrives while the model thinks.
                loop {
                    tokio::select! {
                        result = &mut step => break result,
                        Some(inbound) = self.inbox.recv() => self.receive(inbound)?,
                        Some(outbound) = self.outbox.recv() => self.outbound(outbound)?,
                    }
                }
            };
            match result {
                Ok(step) => break step,
                Err(error) => {
                    failures += 1;
                    self.append(Entry::Notice {
                        at: UnixMs::now(),
                        notice: Notice::Error(format!("{error:#}")),
                    })?;
                    if failures >= MAX_FAILURES {
                        return self.stop("the model kept failing");
                    }
                    tokio::time::sleep(Duration::from_secs(2u64.pow(failures))).await;
                }
            }
        };
        let _ = self.trace.send(Trace::Step {
            code: step.call.as_ref().map(|call| call.code.clone()),
            prose: step.prose.clone(),
        });
        let at = UnixMs::now();
        self.last_step = Some(at);
        self.append(Entry::Step {
            at,
            call: step.call.clone(),
            prose: step.prose,
            carry: step.carry,
            usage: step.usage,
        })?;
        match step.call {
            Some(call) => {
                self.prose = 0;
                self.run_cell(call.id, call.code);
            }
            None => {
                self.prose += 1;
                if self.prose >= MAX_PROSE {
                    self.prose = 0;
                    return self.stop("the model answered in prose instead of code");
                }
            }
        }
        Ok(())
    }

    fn run_cell(&mut self, id: String, code: String) {
        self.next_cell += 1;
        let id = ExecId::try_from(id.as_str())
            .or_else(|_| ExecId::try_from(format!("cell_{}", self.next_cell).as_str()))
            .expect("a generated id is valid");
        let cell = self
            .notebook
            .exec(ExecCall { id, source: code }, Arc::clone(&self.wake));
        self.cells.push(cell);
        self.told_returned = false;
    }

    fn stop(&mut self, why: &str) -> anyhow::Result<()> {
        self.stopped = true;
        self.append(Entry::Notice {
            at: UnixMs::now(),
            notice: Notice::Stopped(why.to_owned()),
        })
    }
}

fn until(at: UnixMs) -> Duration {
    Duration::from_millis(at.0.saturating_sub(UnixMs::now().0))
}

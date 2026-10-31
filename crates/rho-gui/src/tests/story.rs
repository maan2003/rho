//! What a host says about agents, in the words the tests use: heads for the
//! directory, stories for the mail, and the frames each stands for.

use std::cell::{Cell, RefCell};

use rho_agent_hosts::connection::ConnEvent;
use rho_agent_types::{
    Address, AgentId, ContentPart, Mail, MailId, Props, SendKind, UnixMs, WorksetId, WorksetKind,
    props,
};
use rho_agents_client::protocol::rho2::{self, AgentEntry, ServerFrame, SpecView, WorksetEntry};
use rho_agents_client::stream::AgentFrame;
use rho_rpc::protocol::{Answer, Open, read_frame, write_frame};
use senax_encoder::{Packer, Unpacker};

pub type UiRuntimeKind = rho_agents_client::protocol::transcript::RuntimeKind;

/// What the directory says about one agent, and the status it last mailed
/// the user. Tests build these; the helpers below turn each into frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiAgentHead {
    pub agent_id: AgentId,
    pub runtime_kind: UiRuntimeKind,
    /// The workset it is in; `None` is a workset of its own.
    pub workset: Option<WorksetId>,
    pub kind: WorksetKind,
    /// The policy's preset: `advisor` labels it `adv-`.
    pub preset: Option<String>,
    pub user_facing: bool,
    pub parent: Option<AgentId>,
    pub name: Option<String>,
    pub title: Option<String>,
    /// The directory its label shows.
    pub cwd: Option<String>,
    /// What the agent last told the user it is doing, as a status mail.
    pub status: Option<String>,
    pub created_at: UnixMs,
}

impl UiAgentHead {
    pub fn workset(&self) -> WorksetId {
        self.workset.unwrap_or_else(|| own_workset(self.agent_id))
    }

    fn entry(&self) -> AgentEntry {
        let mut said = Props::new();
        let mut set = |key: &str, value: Option<String>| {
            if let Some(value) = value {
                said.insert(key.to_owned(), value);
            }
        };
        set(props::NAME, self.name.clone());
        set(props::TITLE, self.title.clone());
        set(props::CWD, self.cwd.clone());
        set(props::PARENT, self.parent.map(|parent| parent.encoded()));
        set(
            props::USER_FACING,
            self.user_facing.then(|| "true".to_owned()),
        );
        set("preset", self.preset.clone());
        AgentEntry {
            agent: self.agent_id,
            workset: self.workset(),
            spec: SpecView {
                runtime: self.runtime_kind,
                model: "test-model".to_owned(),
                effort: "medium".to_owned(),
                props: said,
            },
            created_at: self.created_at,
        }
    }
}

/// The workset an agent has to itself: one per agent, so deleting it takes
/// that agent alone.
pub fn own_workset(agent_id: AgentId) -> WorksetId {
    let encoded = agent_id.encoded();
    let mut bytes = [0u8; 6];
    for (at, byte) in encoded.bytes().enumerate() {
        bytes[at % 6] = bytes[at % 6].wrapping_mul(31).wrapping_add(byte);
    }
    WorksetId(bytes)
}

/// The story events tests tell: mail between the user and an agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiStoryEvent {
    UserMessage {
        text: String,
        at: UnixMs,
    },
    /// The agent sent the user something of this kind.
    Sent {
        text: String,
        kind: SendKind,
        at: UnixMs,
    },
}

thread_local! {
    /// The newest mail handed out, so every mail a test tells lands past
    /// the last: the model skips mail behind its cursor.
    static MAIL_HEAD: Cell<u64> = const { Cell::new(0) };
    /// The model this test drives. One per test thread, so a test's own
    /// fold and cursor are its own.
    static MODEL: RefCell<(rho_agents_client::model::Model, std::collections::HashSet<rho_agents_client::HostId>)> =
        RefCell::new((rho_agents_client::model::Model::new(), std::collections::HashSet::new()));
}

/// Forgets every mail id handed out, for a test starting fresh.
pub fn reset() {
    MAIL_HEAD.with(|head| head.set(0));
}

fn mail(from: Address, to: Address, kind: Option<SendKind>, text: String, at: UnixMs) -> Mail {
    let id = MAIL_HEAD.with(|head| {
        head.set(head.get() + 1);
        MailId(head.get())
    });
    Mail {
        id,
        at,
        from,
        to,
        thread: rho_agent_types::ThreadId(id),
        kind,
        content: vec![ContentPart::Text { text }],
    }
}

fn status_mail(head: &UiAgentHead) -> Option<Mail> {
    head.status.as_ref().map(|status| {
        mail(
            Address::Agent(head.agent_id),
            Address::User,
            Some(SendKind::Status),
            status.clone(),
            head.created_at,
        )
    })
}

/// A head told again, or told late: its directory entry and its status.
pub fn head_frames(head: UiAgentHead) -> Frame {
    let mut frames = vec![AgentFrame::Server(ServerFrame::Agent(head.entry())).into()];
    if let Some(status) = status_mail(&head) {
        frames.push(mails(vec![status]).into());
    }
    Frame::Many(frames)
}

/// `Ready`, then an agents stream opening on the directory these heads
/// stand for and the mail that says their status. A head for an agent
/// this client already holds says what it says now.
pub fn ready_with(heads: Vec<UiAgentHead>, agent_counter: u64) -> Frame {
    let statuses = heads.iter().filter_map(status_mail).collect::<Vec<_>>();
    let mut worksets = Vec::<WorksetEntry>::new();
    for head in &heads {
        if !worksets.iter().any(|entry| entry.workset == head.workset()) {
            worksets.push(WorksetEntry {
                workset: head.workset(),
                kind: head.kind.clone(),
                created_at: head.created_at,
            });
        }
    }
    let agents = heads.iter().map(UiAgentHead::entry).collect();
    // The head is the newest mail handed out: a client that has heard
    // every mail so far is caught up.
    let mail_head = MAIL_HEAD.with(|head| MailId(head.get()));
    let mut frames = vec![
        ConnEvent::Ready.into(),
        AgentFrame::Server(ServerFrame::Hello {
            machine_seed: 0,
            mail_head,
            agent_counter,
        })
        .into(),
        AgentFrame::Server(ServerFrame::Auth {
            auth: rho_agents_client::protocol::AuthState {
                namespaces: Vec::new(),
                disabled_namespaces: Vec::new(),
                active_namespace: None,
            },
        })
        .into(),
        AgentFrame::Server(ServerFrame::Directory { worksets, agents }).into(),
    ];
    if !statuses.is_empty() {
        frames.push(mails(statuses).into());
    }
    Frame::Many(frames)
}

/// The calls a host in this process has been asked so far
/// ([`crate::workspace::Workspace::host_in_process_for_test`]), each with
/// the stream to answer it on.
pub fn calls(
    streams: &mut tokio::sync::mpsc::UnboundedReceiver<rho_rpc::Stream>,
) -> Vec<(rho2::Request, rho_rpc::Stream)> {
    let mut calls = Vec::new();
    while let Ok(mut stream) = streams.try_recv() {
        let open = futures::executor::block_on(read_frame::<_, Open>(&mut stream))
            .expect("a stream opens by saying what it is for");
        if let Ok(rho2::Open::Request(request)) = open.unpack() {
            calls.push((request, stream));
        }
    }
    calls
}

/// Answers a call as the agent host would.
pub fn answer<T: Packer + Unpacker>(stream: &mut rho_rpc::Stream, answer: Answer<T>) {
    futures::executor::block_on(write_frame(stream, &answer)).expect("answer the call");
}

/// The user's mail to an agent, past the last mail told.
pub fn from_user(agent_id: AgentId, text: &str, at: UnixMs) -> Mail {
    mail(
        Address::User,
        Address::Agent(agent_id),
        None,
        text.to_owned(),
        at,
    )
}

/// Mail in the host's log, as the mail follow hands it.
pub fn mails(mails: Vec<Mail>) -> AgentFrame {
    AgentFrame::Server(ServerFrame::Mail { mails })
}

/// A run of one agent's story: mail between it and the user, each past
/// the last mail told.
pub fn story(agent_id: AgentId, events: Vec<UiStoryEvent>) -> AgentFrame {
    let agent = Address::Agent(agent_id);
    mails(
        events
            .into_iter()
            .map(|event| match event {
                UiStoryEvent::UserMessage { text, at } => {
                    mail(Address::User, agent, None, text, at)
                }
                UiStoryEvent::Sent { text, kind, at } => {
                    mail(agent, Address::User, Some(kind), text, at)
                }
            })
            .collect(),
    )
}

/// Deleting an agent is deleting the workset it is in.
pub fn deleted(workset: WorksetId) -> AgentFrame {
    AgentFrame::Server(ServerFrame::WorksetDeleted { workset })
}

/// Rows of a followed workset, as the host pushes them.
pub fn rows(workset: WorksetId, rows: Vec<(rho_agent_types::Wpos, rho2::LogEvent)>) -> AgentFrame {
    AgentFrame::Server(ServerFrame::Rows { workset, rows })
}

/// What a host says, on any of its streams.
pub enum Frame {
    Control(ConnEvent),
    Agents(AgentFrame),
    Many(Vec<Frame>),
}

impl From<ConnEvent> for Frame {
    fn from(event: ConnEvent) -> Self {
        Self::Control(event)
    }
}

impl From<AgentFrame> for Frame {
    fn from(frame: AgentFrame) -> Self {
        Self::Agents(frame)
    }
}

/// One frame into the workspace: a control event straight in,
/// an agents-stream frame through the same `ingest` the model thread runs,
/// called inline so a test stays in one thread and can assert in the frame
/// it fed.
pub fn feed(
    workspace: &mut crate::workspace::Workspace,
    host: rho_agents_client::HostId,
    frame: impl Into<Frame>,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<crate::workspace::Workspace>,
) {
    match frame.into() {
        Frame::Control(event) => workspace.handle_event(host, event, window, cx),
        Frame::Agents(frame) => {
            let followed = workspace.followed();
            let events = MODEL.with(|model| {
                let (model, attached) = &mut *model.borrow_mut();
                if attached.insert(host) {
                    model.attach(host, format!("host-{}", attached.len()));
                }
                model.command(rho_agents_client::model::ModelCommand::Follow(followed));
                model.ingest(host, frame)
            });
            workspace.handle_model_events(events, window, cx);
        }
        Frame::Many(frames) => {
            for frame in frames {
                feed(workspace, host, frame, window, cx);
            }
        }
    }
}

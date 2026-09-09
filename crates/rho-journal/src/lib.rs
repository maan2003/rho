//! Client-only action journal.
//!
//! Call [`record`] with an [`Event`] at the interaction site. Each call is
//! timestamped immediately and queued to a dedicated writer thread, which
//! commits one event per transaction under the GUI state directory.
//! The journal is deliberately generous and inert: it is for offline replay
//! and measurement, and is never uploaded or used to adapt GUI behavior.
//! The native GUI is already single-instance because it exclusively owns the
//! browser profile; the standalone dump command is correspondingly offline and
//! must be run after the GUI exits.

use std::path::Path;
use std::sync::{OnceLock, mpsc};

use redb::{TableDefinition, TableHandle as _};
use rho_db::{RhoDb, SenAs, SenValue};
use serde::{Deserialize, Serialize};

/// The name this table was written under, from before `Entry` moved out
/// of `rho-gui` and into this crate. redb records the Rust path of a value
/// type and refuses a database whose table says another one, so moving the
/// type took every existing journal with it: the GUI panicked on
/// `TableTypeMismatch` at startup, before the window, on any state
/// directory that had ever recorded an action. What is on disk is
/// unchanged; only the path in the type's name moved.
///
/// Found on a rig cloned from the user's own snapshot, which is what a rig
/// on real state is for: the crate's own tests build their journals fresh
/// and every one of them passed.
#[derive(Debug)]
struct EntryName;

impl rho_db::RecordedTypeName for EntryName {
    const NAME: &'static str = "rho-db::Sen<rho_gui::journal::Entry>";
}

const EVENTS: TableDefinition<u64, SenAs<Entry, EntryName>> =
    TableDefinition::new("gui_action_journal_v3");
/// The same table read as bytes. A row an older build wrote can name a
/// variant this one no longer has (the discard verdict became mute), and
/// the history of everything else is worth more than that one row, so the
/// dump decodes leniently and skips what it cannot read.
const STORED_EVENTS: TableDefinition<u64, StoredEntry> =
    TableDefinition::new("gui_action_journal_v3");

#[derive(Debug)]
struct StoredEntry;

impl redb::Value for StoredEntry {
    type SelfType<'a> = &'a [u8];
    type AsBytes<'a> = &'a [u8];

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> &'a [u8]
    where
        Self: 'a,
    {
        data
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a &'b [u8]) -> &'a [u8]
    where
        Self: 'b,
    {
        value
    }

    /// redb records the name a table was created with, so this has to
    /// answer to the name [`EVENTS`] answers to.
    fn type_name() -> redb::TypeName {
        <SenAs<Entry, EntryName> as redb::Value>::type_name()
    }
}

#[derive(
    Clone, Debug, Serialize, Deserialize, PartialEq, senax_encoder::Encode, senax_encoder::Decode,
)]
pub struct Entry {
    /// RFC 3339 UTC wall-clock time captured at the interaction site.
    pub timestamp: String,
    pub event: Event,
}

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    senax_encoder::Encode,
    senax_encoder::Decode,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeIdentity {
    Note {
        uuid: [u8; 16],
    },
    Label {
        uuid: [u8; 16],
    },
    Agent {
        agent: String,
    },
    Host {
        seed: u64,
    },
    Page {
        uuid: [u8; 16],
    },
    Slack {
        workspace: String,
        channel: String,
        thread: Option<String>,
    },
    PullRequest {
        repo: String,
        number: u64,
    },
    File {
        host: u64,
        path: String,
    },
}

impl From<rho_desk::cells::Id> for NodeIdentity {
    fn from(id: rho_desk::cells::Id) -> Self {
        use rho_desk::cells::Id;
        match id {
            Id::Note(uuid) => Self::Note { uuid: uuid.0 },
            Id::Label(uuid) => Self::Label { uuid: uuid.0 },
            Id::Agent(agent) => Self::Agent {
                agent: agent.encoded(),
            },
            Id::Host(seed) => Self::Host { seed },
            Id::Page(page) => Self::Page { uuid: page.0 },
            Id::Slack(unit) => Self::Slack {
                workspace: unit.workspace,
                channel: unit.channel,
                thread: unit.thread,
            },
            Id::PullRequest { repo, number } => Self::PullRequest { repo, number },
            Id::File { host, path } => Self::File {
                host,
                path: path.to_string(),
            },
        }
    }
}

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    senax_encoder::Encode,
    senax_encoder::Decode,
)]
pub struct AgentIdentity(pub String);

impl From<rho_ui_proto::AgentId> for AgentIdentity {
    fn from(id: rho_ui_proto::AgentId) -> Self {
        Self(id.encoded())
    }
}

impl From<&rho_ui_proto::AgentId> for AgentIdentity {
    fn from(id: &rho_ui_proto::AgentId) -> Self {
        Self(id.encoded())
    }
}
impl From<&str> for AgentIdentity {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

#[derive(
    Clone, Debug, Serialize, Deserialize, PartialEq, senax_encoder::Encode, senax_encoder::Decode,
)]
pub struct DealerCardIdentity {
    pub host: u32,
    pub node_id: NodeIdentity,
}

#[derive(
    Clone, Debug, Serialize, Deserialize, PartialEq, senax_encoder::Encode, senax_encoder::Decode,
)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DealerCardKind {
    Note,
    Agent,
    Thread,
}

#[derive(
    Clone, Debug, Serialize, Deserialize, PartialEq, senax_encoder::Encode, senax_encoder::Decode,
)]
#[serde(rename_all = "snake_case")]
pub enum DealerVerdict {
    Skip,
    Done,
    Mute,
    Defer,
    Open,
    File,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    senax_encoder::Encode,
    senax_encoder::Decode,
)]
#[serde(rename_all = "snake_case")]
pub enum PhoneFlickDirection {
    Up,
    Down,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    senax_encoder::Encode,
    senax_encoder::Decode,
)]
#[serde(rename_all = "snake_case")]
pub enum PhoneVerdict {
    Done,
    Mute,
    Defer,
    Todo,
    File,
    Reply,
}

#[derive(
    Clone, Debug, Serialize, Deserialize, PartialEq, senax_encoder::Encode, senax_encoder::Decode,
)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SurfaceIdentity {
    Draft,
    Home,
    Messages,
    DeskNode {
        host: u32,
        node_id: NodeIdentity,
    },
    Transcript {
        agent_id: AgentIdentity,
    },
    File {
        agent_id: AgentIdentity,
        path: String,
    },
    Shell {
        agent_id: AgentIdentity,
    },
    Diff {
        agent_id: AgentIdentity,
    },
    Terminal {
        agent_id: AgentIdentity,
        terminal_id: u64,
    },
    Browser {
        page_id: String,
    },
    /// Retired: Zulip is gone from rho and nothing writes these. They
    /// stay because a journal already on disk decodes by variant order,
    /// and taking one out of the middle would misread every file that has
    /// a later variant in it.
    ZulipInbox,
    ZulipNarrow {
        label: String,
    },
    SlackList,
    SlackConversation {
        thread: SlackThread,
    },
    Image {
        title: String,
    },
    Dashboard,
    Usage,
    /// The places one Slack search found. The query is the identity: two
    /// searches are the same surface drawn twice, and the journal should say
    /// which one the reader was reading.
    SlackSearch {
        query: String,
    },
}

/// Who ignored the thread: this rho, or Slack telling rho that another
/// client did.
#[derive(
    Clone,
    Copy,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    senax_encoder::Encode,
    senax_encoder::Decode,
)]
#[serde(rename_all = "snake_case")]
pub enum IgnoredBy {
    Rho,
    Slack,
}

/// A Slack thread as the journal names it: the workspace and conversation a
/// person would recognise, plus the thread's own key so two threads in one
/// conversation stay distinct.
#[derive(
    Clone, Debug, Serialize, Deserialize, PartialEq, senax_encoder::Encode, senax_encoder::Decode,
)]
pub struct SlackThread {
    pub workspace: String,
    pub conversation: String,
    pub thread: String,
}

#[derive(
    Clone, Debug, Serialize, Deserialize, PartialEq, senax_encoder::Encode, senax_encoder::Decode,
)]
#[serde(rename_all = "snake_case")]
pub enum CreateMethod {
    /// The `new` transient: the user asked for it and chose the area.
    New,
    /// A browser tab that was born rather than opened from a link.
    TabBirth,
}

#[derive(
    Clone, Debug, Serialize, Deserialize, PartialEq, senax_encoder::Encode, senax_encoder::Decode,
)]
#[serde(rename_all = "snake_case")]
pub enum CreatedKind {
    Note,
    Page,
    Agent,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    senax_encoder::Encode,
    senax_encoder::Decode,
)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceShowMethod {
    Overview,
    Open,
    Command,
    Mru,
    Deal,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    senax_encoder::Encode,
    senax_encoder::Decode,
)]
#[serde(rename_all = "snake_case")]
pub enum HistoryDirection {
    Back,
    Forward,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    senax_encoder::Encode,
    senax_encoder::Decode,
)]
#[serde(rename_all = "snake_case")]
pub enum HistoryAppendMethod {
    Deal,
    Overview,
    Command,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    senax_encoder::Encode,
    senax_encoder::Decode,
)]
#[serde(rename_all = "snake_case")]
pub enum HistoryRemoveMethod {
    Close,
    Dedupe,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    senax_encoder::Encode,
    senax_encoder::Decode,
)]
#[serde(rename_all = "snake_case")]
pub enum DealModeAction {
    Enter,
    Interacted,
    Exit,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    senax_encoder::Encode,
    senax_encoder::Decode,
)]
#[serde(rename_all = "snake_case")]
pub enum SignalState {
    On,
    Off,
}

#[derive(
    Clone, Debug, Serialize, Deserialize, PartialEq, senax_encoder::Encode, senax_encoder::Decode,
)]
pub struct BuildIdentity {
    pub version: String,
    pub git_commit: Option<String>,
}

#[derive(
    Clone, Debug, Serialize, Deserialize, PartialEq, senax_encoder::Encode, senax_encoder::Decode,
)]
pub struct DealerPolicySnapshot {
    pub queue_floor: f64,
    pub blocked_reply_head_start: f64,
    pub blocked_reply_slope_per_day: f64,
    pub fyi_reply_pace_days: f64,
    pub thread_reply_head_start: f64,
    /// Where a channel's own unread traffic starts, and what being
    /// answered by somebody else takes off it. Added after the first
    /// journals were written, so a session recorded before the channel
    /// curve existed decodes with both at zero, which is what it had.
    #[senax(default)]
    #[serde(default)]
    pub channel_traffic_head_start: f64,
    #[senax(default)]
    #[serde(default)]
    pub channel_answered_drop: f64,
    pub skip_cooldown_minutes: i64,
    pub lamp_threshold: f64,
    pub chime_threshold: f64,
    pub agent_recency_bonus: f64,
    pub agent_recency_window_ms: i64,
}

#[derive(
    Clone, Debug, Serialize, Deserialize, PartialEq, senax_encoder::Encode, senax_encoder::Decode,
)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    SessionStarted {
        build: BuildIdentity,
        dealer_policy: DealerPolicySnapshot,
    },
    WindowFocusChanged {
        focused: bool,
    },
    UserIdle {
        timeout_s: u64,
    },
    UserResumed,
    LampTransition {
        state: SignalState,
        top_priority: Option<f64>,
        card: Option<DealerCardIdentity>,
    },
    ChimeRing {
        top_priority: f64,
        card: DealerCardIdentity,
    },
    SurfaceShown {
        surface: SurfaceIdentity,
        method: SurfaceShowMethod,
    },
    HistoryStepped {
        direction: HistoryDirection,
        position: usize,
        len: usize,
    },
    HistoryAppended {
        identity: SurfaceIdentity,
        method: HistoryAppendMethod,
    },
    HistoryRemoved {
        identity: SurfaceIdentity,
        method: HistoryRemoveMethod,
    },
    DealMode {
        action: DealModeAction,
        card: Option<DealerCardIdentity>,
    },
    SurfaceClosed {
        surface: SurfaceIdentity,
        dealt_untouched: bool,
    },
    OverviewOpened,
    AgentOpened {
        agent_id: AgentIdentity,
    },
    AgentSelected {
        agent_id: Option<String>,
    },
    /// The Slack session came up, went away, gave a thread a node, or
    /// carried the user's own reply. The thread is named, not numbered:
    /// the record has to be readable a month later.
    SlackConnected {
        workspace: String,
    },
    SlackDisconnected {
        workspace: String,
        reason: String,
    },
    /// Retired: nothing writes this. A thread that starts to matter needs
    /// no node -- a Slack unit is a virtual node and rho writes nothing on
    /// attention -- so the event it recorded no longer happens. The
    /// variant stays because a journal already on disk decodes by variant
    /// order, and taking one out of the middle would misread every file
    /// that has one.
    SlackThreadBound {
        thread: SlackThread,
        node_id: NodeIdentity,
    },
    SlackReplied {
        thread: SlackThread,
    },
    /// The thread stopped being the user's: `x` here, which tells Slack, or
    /// an unfollow in another client, which Slack tells rho.
    SlackThreadIgnored {
        thread: SlackThread,
        by: IgnoredBy,
    },
    /// The old backlog marked read in one go: the cutoff the user gave and
    /// how much it touched. The verdicts it wrote are undone as a batch,
    /// which is what `SlackMarkReadBeforeUndone` records; the marking
    /// itself is Slack's state and is not reversed.
    SlackMarkedReadBefore {
        cutoff: String,
        conversations: usize,
        threads: usize,
    },
    SlackMarkReadBeforeUndone {
        cards: usize,
    },
    /// The reader rewrote something they had already sent: `e` (or `up` on
    /// an empty composer) and then `enter`, which is `chat.update` on
    /// Slack's side.
    SlackMessageEdited {
        conversation: String,
        ts: String,
    },
    /// A picture sent with a message: `files.getUploadURLExternal` and
    /// `files.completeUploadExternal` on Slack's side.
    SlackFileSent {
        conversation: String,
        bytes: u64,
    },
    MinibufferOpened {
        prompt: String,
    },
    MinibufferSubmitted {
        prompt: String,
        input: String,
    },
    MinibufferCancelled {
        prompt: String,
        input: String,
    },
    DeskRawModeToggled {
        enabled: bool,
    },
    /// A manual Desk heading lookup is a miss signal for future dealing.
    Find {
        query: String,
        target: String,
        found: bool,
    },
    /// What one card was told to do. There is no session behind it: the
    /// verdict is about the card the reader had in front of them, so what
    /// else was ranked at the time is not the seam's to claim.
    Dealer {
        card: DealerCardIdentity,
        kind: DealerCardKind,
        verdict: DealerVerdict,
        skip_until: Option<String>,
        occurred_at: String,
    },
    /// A local verdict reversal. External effects initiated by the original
    /// verdict (for example, marking a Slack thread read) are not reversed.
    VerdictUndone {
        card: DealerCardIdentity,
        verdict: DealerVerdict,
    },
    /// A new node the user asked for, and where it was filed.
    Created {
        node_id: NodeIdentity,
        kind: CreatedKind,
        method: CreateMethod,
        at_root: bool,
    },
    /// One-shot on the first run of the build that deleted the inbox: the
    /// capture items the user had written became notes at the root.
    CaptureCarryover {
        notes: u32,
        unreadable: u32,
    },
    /// One event per scroll burst. The position is a coarse vertical row or
    /// line offset; surfaces without a readable viewport report zero.
    Scroll {
        surface: SurfaceIdentity,
        rough_position: i64,
    },
    PhoneFlick {
        direction: PhoneFlickDirection,
        moved_card: bool,
    },
    PhoneVerdict {
        verdict: PhoneVerdict,
    },
}

impl Event {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SessionStarted { .. } => "session_started",
            Self::WindowFocusChanged { .. } => "window_focus_changed",
            Self::UserIdle { .. } => "user_idle",
            Self::UserResumed => "user_resumed",
            Self::LampTransition { .. } => "lamp_transition",
            Self::ChimeRing { .. } => "chime_ring",
            Self::SurfaceShown { .. } => "surface_shown",
            Self::HistoryStepped { .. } => "history_stepped",
            Self::HistoryAppended { .. } => "history_appended",
            Self::HistoryRemoved { .. } => "history_removed",
            Self::DealMode { .. } => "deal_mode",
            Self::SurfaceClosed { .. } => "surface_closed",
            Self::OverviewOpened => "overview_opened",
            Self::AgentOpened { .. } => "agent_opened",
            Self::AgentSelected { .. } => "agent_selected",
            Self::SlackConnected { .. } => "slack_connected",
            Self::SlackDisconnected { .. } => "slack_disconnected",
            Self::SlackThreadBound { .. } => "slack_thread_bound",
            Self::SlackReplied { .. } => "slack_replied",
            Self::SlackThreadIgnored { .. } => "slack_thread_ignored",
            Self::SlackMarkedReadBefore { .. } => "slack_marked_read_before",
            Self::SlackMarkReadBeforeUndone { .. } => "slack_mark_read_before_undone",
            Self::SlackMessageEdited { .. } => "slack_message_edited",
            Self::SlackFileSent { .. } => "slack_file_sent",
            Self::MinibufferOpened { .. } => "minibuffer_opened",
            Self::MinibufferSubmitted { .. } => "minibuffer_submitted",
            Self::MinibufferCancelled { .. } => "minibuffer_cancelled",
            Self::DeskRawModeToggled { .. } => "desk_raw_mode_toggled",
            Self::Created { .. } => "created",
            Self::CaptureCarryover { .. } => "capture_carryover",
            Self::Scroll { .. } => "scroll",
            Self::Find { .. } => "find",
            Self::Dealer { .. } => "dealer",
            Self::VerdictUndone { .. } => "verdict_undone",
            Self::PhoneFlick { .. } => "phone_flick",
            Self::PhoneVerdict { .. } => "phone_verdict",
        }
    }
}

enum Message {
    Entry(Entry),
    Flush(mpsc::SyncSender<()>),
}

pub struct Journal {
    db: RhoDb,
    sender: mpsc::Sender<Message>,
}

impl Journal {
    /// Opens the client's database at `state_dir` and takes the journal's
    /// tables in it. For tests and tools; the session's own database is
    /// opened once by the model thread and handed to [`Journal::open_on`].
    pub fn open(state_dir: &Path) -> std::io::Result<Self> {
        Self::open_on(rho_db::client::open(state_dir)?)
    }

    /// The journal's tables in a database somebody else opened. The file
    /// holds every other kind of client state too, under its own names;
    /// this touches the journal's and nothing else.
    pub fn open_on(db: RhoDb) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread().build()?;
        runtime.block_on(async {
            let mut write = db.write().await;
            write.delete_table("gui_action_journal_v1");
            write.open_table(EVENTS);
            write.commit();
        });
        let sequence = next_sequence(&db);
        let (sender, receiver) = mpsc::channel();
        let writer_db = db.clone();
        std::thread::Builder::new()
            .name("rho-action-journal".into())
            .spawn(move || writer(writer_db, sequence, receiver))?;
        Ok(Self { db, sender })
    }

    pub fn record(&self, event: Event) {
        let entry = Entry {
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            event,
        };
        if self.sender.send(Message::Entry(entry)).is_err() {
            tracing::error!("action journal writer stopped");
        }
    }

    pub fn dump(&self, kind: Option<&str>, output: impl std::io::Write) -> anyhow::Result<()> {
        dump_db(&self.db, kind, output)
    }

    fn flush(&self) -> std::io::Result<()> {
        let (send, receive) = mpsc::sync_channel(0);
        self.sender.send(Message::Flush(send)).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "action journal writer stopped",
            )
        })?;
        receive.recv().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "action journal writer stopped",
            )
        })
    }
}

fn next_sequence(db: &RhoDb) -> u64 {
    db.read()
        .open_table(EVENTS)
        .iter()
        .next_back()
        .map_or(0, |(key, _)| {
            key.value()
                .checked_add(1)
                .expect("action journal sequence overflow")
        })
}

fn writer(db: RhoDb, mut sequence: u64, receiver: mpsc::Receiver<Message>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("build action journal runtime");

    for message in receiver {
        match message {
            Message::Entry(entry) => {
                runtime.block_on(async {
                    let mut write = db.write().await;
                    write
                        .open_table(EVENTS)
                        .insert(&sequence, SenValue::borrowed(&entry));
                    write.commit();
                });
                sequence = sequence
                    .checked_add(1)
                    .expect("action journal sequence overflow");
            }
            Message::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

static GLOBAL: OnceLock<Journal> = OnceLock::new();

/// Takes the journal's tables in the client's database, which `main` has
/// asked to be handed when the model thread opens it.
pub fn init(db: RhoDb, dealer_policy: DealerPolicySnapshot) -> std::io::Result<()> {
    let journal = Journal::open_on(db)?;
    GLOBAL.set(journal).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "action journal is already initialized",
        )
    })?;
    record(Event::SessionStarted {
        build: BuildIdentity {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            git_commit: option_env!("RHO_BUILD_GIT_COMMIT").map(str::to_owned),
        },
        dealer_policy,
    });
    Ok(())
}

/// Waits until all previously enqueued events have committed. Normal GUI
/// shutdown calls this; interaction sites should only use [`record`].
pub fn flush() {
    if let Some(journal) = GLOBAL.get()
        && let Err(error) = journal.flush()
    {
        tracing::error!(%error, "failed to flush action journal");
    }
}

/// Records an event if the journal has been initialized.
pub fn record(event: Event) {
    if let Some(journal) = GLOBAL.get() {
        journal.record(event);
    }
}

pub fn dump(
    state_dir: &Path,
    kind: Option<&str>,
    output: impl std::io::Write,
) -> anyhow::Result<()> {
    if !rho_db::client::path(state_dir).exists() {
        return Ok(());
    }
    // Exclusively: the journal is tables in the client's one database, and
    // a GUI holding it is a GUI whose rows are still being written.
    let db = rho_db::client::open(state_dir)?;
    dump_db(&db, kind, output)
}

#[derive(Serialize)]
struct DumpEntry<'a> {
    sequence: u64,
    timestamp: &'a str,
    event: &'a Event,
}

fn dump_db(db: &RhoDb, kind: Option<&str>, mut output: impl std::io::Write) -> anyhow::Result<()> {
    let read = db.read();
    if !read.has_table(EVENTS.name()) {
        return Ok(());
    }
    let mut skipped = 0u64;
    for (sequence, value) in read.open_table(STORED_EVENTS).iter() {
        let sequence = sequence.value();
        let mut bytes = value.value();
        let Ok(entry) = <Entry as senax_encoder::Decoder>::decode(&mut bytes) else {
            skipped += 1;
            continue;
        };
        if kind.is_none_or(|kind| entry.event.kind() == kind) {
            serde_json::to_writer(
                &mut output,
                &DumpEntry {
                    sequence,
                    timestamp: &entry.timestamp,
                    event: &entry.event,
                },
            )?;
            writeln!(output)?;
        }
    }
    if skipped > 0 {
        // On stderr, so the dump's own lines stay machine-readable.
        eprintln!("skipped {skipped} journal rows this build cannot read");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A journal written before `Entry` moved into this crate still
    /// opens.
    ///
    /// The crate's other tests build their journals fresh, so they write
    /// the name this build uses and read it back: every one of them passed
    /// while the GUI panicked at startup on the user's own state. This one
    /// writes the old name first, which is the only way to fail the way a
    /// reader fails.
    #[test]
    fn a_journal_written_before_the_entry_moved_crates_still_opens() {
        /// What `std::any::type_name` gave while `Entry` was
        /// `rho_gui::journal::Entry`. Spelled out rather than referenced,
        /// so that renaming the pin in the code does not rename the thing
        /// this test is checking against.
        #[derive(Debug)]
        struct AsWrittenInRhoGui;

        impl rho_db::RecordedTypeName for AsWrittenInRhoGui {
            const NAME: &'static str = "rho-db::Sen<rho_gui::journal::Entry>";
        }

        const OLD: TableDefinition<u64, SenAs<Entry, AsWrittenInRhoGui>> =
            TableDefinition::new("gui_action_journal_v3");

        let dir = tempfile::tempdir().unwrap();
        let db = RhoDb::open(rho_db::client::path(dir.path()));
        futures::executor::block_on(async {
            let mut write = db.write().await;
            let entry = Entry {
                timestamp: "2026-09-06T19:20:32Z".to_owned(),
                event: Event::UserResumed,
            };
            write.open_table(OLD).insert(&1, SenValue::borrowed(&entry));
            write.commit();
        });
        drop(db);

        // The panic this reproduces was inside `open`, before anything a
        // caller could catch, so the assertion is that it returns at all.
        let journal = Journal::open(dir.path()).expect("open a journal an older build wrote");
        journal.record(Event::UserResumed);
        journal.flush().unwrap();
        let mut output = Vec::new();
        journal.dump(None, &mut output).unwrap();
        let dumped = String::from_utf8(output).unwrap();
        assert_eq!(
            dumped.lines().count(),
            2,
            "the row the older build wrote is still there and the new one \
             went in beside it: {dumped}"
        );
    }

    /// A row from a build whose vocabulary has moved on, the discard
    /// verdict being the first, is one row lost and not a lost journal.
    #[test]
    fn a_row_this_build_cannot_read_is_skipped_and_the_rest_still_dumps() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path()).unwrap();
        journal.record(Event::UserResumed);
        journal.flush().unwrap();
        futures::executor::block_on(async {
            let mut write = journal.db.write().await;
            write
                .open_table(STORED_EVENTS)
                // Past the writer's own sequence, so recording the next
                // event cannot quietly overwrite it.
                .insert(&999, &b"not an entry this build knows".as_slice());
            write.commit();
        });
        journal.record(Event::DeskRawModeToggled { enabled: true });
        journal.flush().unwrap();

        let mut output = Vec::new();
        journal.dump(None, &mut output).unwrap();
        let events = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["event"]["type"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(events, vec!["user_resumed", "desk_raw_mode_toggled"]);
    }

    #[test]
    fn persists_events_and_dumps_filtered_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path()).unwrap();
        journal.record(Event::AgentOpened {
            agent_id: "agent-a".into(),
        });
        journal.record(Event::DeskRawModeToggled { enabled: true });
        journal.record(Event::UserIdle { timeout_s: 60 });
        journal.record(Event::UserResumed);
        journal.flush().unwrap();

        let mut output = Vec::new();
        journal.dump(None, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        let entries = output
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0]["sequence"], 0);
        assert_eq!(entries[0]["event"]["type"], "agent_opened");
        assert_eq!(entries[1]["sequence"], 1);
        assert_eq!(entries[1]["event"]["type"], "desk_raw_mode_toggled");
        assert_eq!(entries[2]["event"]["type"], "user_idle");
        assert_eq!(entries[2]["event"]["timeout_s"], 60);
        assert_eq!(entries[3]["event"]["type"], "user_resumed");

        let mut filtered = Vec::new();
        journal.dump(Some("agent_opened"), &mut filtered).unwrap();
        assert_eq!(String::from_utf8(filtered).unwrap().lines().count(), 1);
    }

    #[test]
    fn standalone_dump_reports_an_active_gui_journal() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path()).unwrap();
        journal.record(Event::AgentOpened {
            agent_id: "agent-a".into(),
        });
        journal.flush().unwrap();

        let error = dump(dir.path(), None, Vec::new()).unwrap_err();
        assert!(error.to_string().contains("exit the GUI"));
    }

    /// A journal written before the channel curve existed still decodes.
    /// The fields are new, so the run that wrote those bytes had no
    /// channel curve, and zero is exactly what it had.
    #[test]
    fn a_policy_written_before_the_channel_curve_still_decodes() {
        use bytes::BytesMut;
        use senax_encoder::{Decoder as _, Encoder as _};

        /// The struct as it stood before the two channel fields, so this
        /// is the shape of the bytes already on disk and not a mock of it.
        #[derive(senax_encoder::Encode)]
        struct WithoutTheChannelCurve {
            queue_floor: f64,
            blocked_reply_head_start: f64,
            blocked_reply_slope_per_day: f64,
            fyi_reply_pace_days: f64,
            thread_reply_head_start: f64,
            skip_cooldown_minutes: i64,
            lamp_threshold: f64,
            chime_threshold: f64,
            agent_recency_bonus: f64,
            agent_recency_window_ms: i64,
        }

        let mut buffer = BytesMut::new();
        WithoutTheChannelCurve {
            queue_floor: -1.0,
            blocked_reply_head_start: 1.0,
            blocked_reply_slope_per_day: 12.0,
            fyi_reply_pace_days: 3.0,
            thread_reply_head_start: 1.1,
            skip_cooldown_minutes: 15,
            lamp_threshold: 2.0,
            chime_threshold: 3.0,
            agent_recency_bonus: 4.0,
            agent_recency_window_ms: 3_600_000,
        }
        .encode(&mut buffer)
        .unwrap();

        let mut bytes = buffer.freeze();
        let policy = DealerPolicySnapshot::decode(&mut bytes).unwrap();
        assert_eq!(policy.thread_reply_head_start, 1.1);
        assert_eq!(policy.channel_traffic_head_start, 0.0);
        assert_eq!(policy.channel_answered_drop, 0.0);
    }

    #[test]
    fn session_started_round_trips_build_and_policy_context() {
        let event = Event::SessionStarted {
            build: BuildIdentity {
                version: "1.2.3".into(),
                git_commit: Some("deadbeef".into()),
            },
            dealer_policy: DealerPolicySnapshot {
                queue_floor: 1.0,
                blocked_reply_head_start: 2.0,
                blocked_reply_slope_per_day: 3.0,
                fyi_reply_pace_days: 4.0,
                thread_reply_head_start: 5.0,
                channel_traffic_head_start: 11.0,
                channel_answered_drop: 12.0,
                skip_cooldown_minutes: 6,
                lamp_threshold: 7.0,
                chime_threshold: 8.0,
                agent_recency_bonus: 9.0,
                agent_recency_window_ms: 10,
            },
        };
        let encoded = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&encoded).unwrap(), event);
    }

    #[test]
    fn signal_events_round_trip_the_top_card() {
        let card = DealerCardIdentity {
            host: 1,
            node_id: NodeIdentity::Note { uuid: [3; 16] },
        };
        for event in [
            Event::LampTransition {
                state: SignalState::On,
                top_priority: Some(1.5),
                card: Some(card.clone()),
            },
            Event::ChimeRing {
                top_priority: 1.5,
                card,
            },
        ] {
            let encoded = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&encoded).unwrap(), event);
        }
    }

    #[test]
    fn user_idle_events_round_trip() {
        for event in [Event::UserIdle { timeout_s: 60 }, Event::UserResumed] {
            let encoded = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&encoded).unwrap(), event);
        }
    }

    #[test]
    fn surface_close_records_whether_a_deal_was_untouched() {
        let event = Event::SurfaceClosed {
            surface: SurfaceIdentity::Transcript {
                agent_id: AgentIdentity("agent-a".into()),
            },
            dealt_untouched: true,
        };
        let encoded = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&encoded).unwrap(), event);
    }

    #[test]
    fn history_events_round_trip_direction_position_and_methods() {
        let surface = SurfaceIdentity::Transcript {
            agent_id: AgentIdentity("agent-a".into()),
        };
        for event in [
            Event::HistoryStepped {
                direction: HistoryDirection::Back,
                position: 3,
                len: 7,
            },
            Event::HistoryAppended {
                identity: surface.clone(),
                method: HistoryAppendMethod::Deal,
            },
            Event::HistoryRemoved {
                identity: surface,
                method: HistoryRemoveMethod::Dedupe,
            },
        ] {
            let encoded = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&encoded).unwrap(), event);
        }
    }

    #[test]
    fn dealer_event_round_trips() {
        let event = Event::Dealer {
            card: DealerCardIdentity {
                host: 1,
                node_id: NodeIdentity::Note { uuid: [7; 16] },
            },
            kind: DealerCardKind::Thread,
            verdict: DealerVerdict::Defer,
            skip_until: None,
            occurred_at: "2026-09-01T20:00:00+00:00".into(),
        };
        let encoded = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&encoded).unwrap(), event);
    }

    #[test]
    fn phone_events_round_trip_gesture_outcomes() {
        for event in [
            Event::PhoneFlick {
                direction: PhoneFlickDirection::Up,
                moved_card: true,
            },
            Event::PhoneFlick {
                direction: PhoneFlickDirection::Down,
                moved_card: false,
            },
            Event::PhoneVerdict {
                verdict: PhoneVerdict::Todo,
            },
        ] {
            let encoded = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&encoded).unwrap(), event);
        }
    }
}

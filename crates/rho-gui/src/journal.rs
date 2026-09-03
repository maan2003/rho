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

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, mpsc};

use redb::{TableDefinition, TableHandle as _};
use rho_db::{RhoDb, Sen, SenValue};
use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = "action-journal.redb";
const LOCK_FILE_NAME: &str = "action-journal.lock";

const EVENTS: TableDefinition<u64, Sen<Entry>> = TableDefinition::new("gui_action_journal_v3");

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
pub struct NodeIdentity {
    pub replica_id: u16,
    pub counter: u64,
}

impl From<rho_desk::NodeId> for NodeIdentity {
    fn from(id: rho_desk::NodeId) -> Self {
        Self {
            replica_id: id.replica_id,
            counter: id.counter,
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
    Dismiss,
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
    Dismiss,
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
    /// A Slack thread started to matter, so it has a node in the tree.
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
    /// Complete event emitted by the dealer seam. `considered_not_dealt`
    /// is intentionally retained for counterfactual replay.
    Dealer {
        card: DealerCardIdentity,
        kind: DealerCardKind,
        verdict: DealerVerdict,
        skip_until: Option<String>,
        occurred_at: String,
        time_to_verdict_ms: u64,
        considered_not_dealt: Vec<DealerCardIdentity>,
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
    _lock: File,
    sender: mpsc::Sender<Message>,
    path: PathBuf,
}

impl Journal {
    pub fn open(state_dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(state_dir)?;
        let lock = acquire_lock(state_dir)?;
        let path = state_dir.join(FILE_NAME);
        let db = RhoDb::open(&path);
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
        Ok(Self {
            db,
            _lock: lock,
            sender,
            path,
        })
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

    pub fn path(&self) -> &Path {
        &self.path
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

fn acquire_lock(state_dir: &Path) -> std::io::Result<File> {
    let path = state_dir.join(LOCK_FILE_NAME);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        return Err(std::io::Error::new(
            error.kind(),
            "action journal is in use; exit the GUI before opening or dumping it",
        ));
    }
    Ok(file)
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

pub fn init(state_dir: &Path) -> std::io::Result<()> {
    let journal = Journal::open(state_dir)?;
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
        dealer_policy: crate::dashboard::dealer_policy_snapshot(),
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
    let path = state_dir.join(FILE_NAME);
    if !path.exists() {
        return Ok(());
    }
    let _lock = acquire_lock(state_dir)?;
    let db = RhoDb::open(path);
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
    for (sequence, value) in read.open_table(EVENTS).iter() {
        let sequence = sequence.value();
        let entry = value.value().into_owned();
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

        assert!(journal.path().exists());
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

    #[test]
    fn session_started_round_trips_build_and_policy_context() {
        let event = Event::SessionStarted {
            build: BuildIdentity {
                version: "1.2.3".into(),
                git_commit: Some("deadbeef".into()),
            },
            dealer_policy: crate::dashboard::dealer_policy_snapshot(),
        };
        let encoded = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&encoded).unwrap(), event);
    }

    #[test]
    fn signal_events_round_trip_the_top_card() {
        let card = DealerCardIdentity {
            host: 1,
            node_id: NodeIdentity {
                replica_id: 2,
                counter: 3,
            },
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
    fn dealer_event_round_trips_considered_cards() {
        let event = Event::Dealer {
            card: DealerCardIdentity {
                host: 1,
                node_id: NodeIdentity {
                    replica_id: 2,
                    counter: 7,
                },
            },
            kind: DealerCardKind::Thread,
            verdict: DealerVerdict::Defer,
            skip_until: None,
            occurred_at: "2026-09-01T20:00:00+00:00".into(),
            time_to_verdict_ms: 4200,
            considered_not_dealt: vec![DealerCardIdentity {
                host: 1,
                node_id: NodeIdentity {
                    replica_id: 2,
                    counter: 9,
                },
            }],
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

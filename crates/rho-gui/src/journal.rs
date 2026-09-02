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

#[cfg(feature = "native")]
use std::fs::{File, OpenOptions};
#[cfg(feature = "native")]
use std::os::fd::AsRawFd as _;
#[cfg(feature = "native")]
use std::path::{Path, PathBuf};
#[cfg(feature = "native")]
use std::sync::{OnceLock, mpsc};

#[cfg(feature = "native")]
use redb::{TableDefinition, TableHandle as _};
#[cfg(feature = "native")]
use rho_db::{RhoDb, Sen, SenValue};
use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = "action-journal.redb";
const LOCK_FILE_NAME: &str = "action-journal.lock";

#[cfg(feature = "native")]
const EVENTS: TableDefinition<u64, Sen<Entry>> = TableDefinition::new("gui_action_journal_v1");

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    feature = "native",
    derive(senax_encoder::Encode, senax_encoder::Decode)
)]
pub struct Entry {
    /// RFC 3339 UTC wall-clock time captured at the interaction site.
    pub timestamp: String,
    pub event: Event,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    feature = "native",
    derive(senax_encoder::Encode, senax_encoder::Decode)
)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DealerCardIdentity {
    Desk { host: String, heading_offset: usize },
    Agent { agent_id: String },
    Inbox { id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    feature = "native",
    derive(senax_encoder::Encode, senax_encoder::Decode)
)]
#[serde(rename_all = "snake_case")]
pub enum DealerInboxKind {
    Ping,
    Obligation,
    Capture,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    feature = "native",
    derive(senax_encoder::Encode, senax_encoder::Decode)
)]
#[serde(tag = "type", content = "inbox_kind", rename_all = "snake_case")]
pub enum DealerCardKind {
    Desk,
    Agent,
    Inbox(DealerInboxKind),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    feature = "native",
    derive(senax_encoder::Encode, senax_encoder::Decode)
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    feature = "native",
    derive(senax_encoder::Encode, senax_encoder::Decode)
)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SurfaceIdentity {
    Draft,
    DeskHeading { host: String, heading_offset: usize },
    Inbox { id: String },
    Transcript { agent_id: String },
    File { agent_id: String, path: String },
    Shell { agent_id: String },
    Diff { agent_id: String },
    Terminal { agent_id: String, terminal_id: u64 },
    Browser { page_id: String },
    ZulipInbox,
    ZulipNarrow { label: String },
    Dashboard,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    feature = "native",
    derive(senax_encoder::Encode, senax_encoder::Decode)
)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMethod {
    Keyboard,
    TabBirth,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    feature = "native",
    derive(senax_encoder::Encode, senax_encoder::Decode)
)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboxVerdict {
    Discard,
    File { heading: String },
    Defer { until_ms: i64 },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(
    feature = "native",
    derive(senax_encoder::Encode, senax_encoder::Decode)
)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceShowMethod {
    Overview,
    Open,
    Mru,
    Deal,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(
    feature = "native",
    derive(senax_encoder::Encode, senax_encoder::Decode)
)]
#[serde(rename_all = "snake_case")]
pub enum HistoryDirection {
    Back,
    Forward,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(
    feature = "native",
    derive(senax_encoder::Encode, senax_encoder::Decode)
)]
#[serde(rename_all = "snake_case")]
pub enum HistoryAppendMethod {
    Deal,
    Overview,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(
    feature = "native",
    derive(senax_encoder::Encode, senax_encoder::Decode)
)]
#[serde(rename_all = "snake_case")]
pub enum HistoryRemoveMethod {
    Close,
    Dedupe,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(
    feature = "native",
    derive(senax_encoder::Encode, senax_encoder::Decode)
)]
#[serde(rename_all = "snake_case")]
pub enum DealModeAction {
    Enter,
    Interacted,
    Exit,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(
    feature = "native",
    derive(senax_encoder::Encode, senax_encoder::Decode)
)]
#[serde(rename_all = "snake_case")]
pub enum SignalState {
    On,
    Off,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    feature = "native",
    derive(senax_encoder::Encode, senax_encoder::Decode)
)]
pub struct BuildIdentity {
    pub version: String,
    pub git_commit: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    feature = "native",
    derive(senax_encoder::Encode, senax_encoder::Decode)
)]
pub struct DealerPolicySnapshot {
    pub queue_floor: f64,
    pub blocked_reply_head_start: f64,
    pub blocked_reply_slope_per_day: f64,
    pub fyi_reply_pace_days: f64,
    pub inbox_obligation_pace_days: u32,
    pub inbox_capture_pace_days: u32,
    pub skip_cooldown_minutes: i64,
    pub lamp_threshold: f64,
    pub chime_threshold: f64,
    pub agent_recency_bonus: f64,
    pub agent_recency_window_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    feature = "native",
    derive(senax_encoder::Encode, senax_encoder::Decode)
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
    DeskHeadingDeferred {
        heading: String,
        until: String,
        card: DealerCardIdentity,
    },
    OverviewOpened,
    AgentOpened {
        agent_id: String,
    },
    AgentSelected {
        agent_id: Option<String>,
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
    Capture {
        inbox_id: String,
        method: CaptureMethod,
    },
    InboxVerdict {
        inbox_id: String,
        verdict: InboxVerdict,
    },
    /// One event per scroll burst. The position is a coarse vertical row or
    /// line offset; surfaces without a readable viewport report zero.
    Scroll {
        surface: SurfaceIdentity,
        rough_position: i64,
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
            Self::DeskHeadingDeferred { .. } => "desk_heading_deferred",
            Self::OverviewOpened => "overview_opened",
            Self::AgentOpened { .. } => "agent_opened",
            Self::AgentSelected { .. } => "agent_selected",
            Self::MinibufferOpened { .. } => "minibuffer_opened",
            Self::MinibufferSubmitted { .. } => "minibuffer_submitted",
            Self::MinibufferCancelled { .. } => "minibuffer_cancelled",
            Self::DeskRawModeToggled { .. } => "desk_raw_mode_toggled",
            Self::Capture { .. } => "capture",
            Self::InboxVerdict { .. } => "inbox_verdict",
            Self::Scroll { .. } => "scroll",
            Self::Find { .. } => "find",
            Self::Dealer { .. } => "dealer",
        }
    }
}

#[cfg(feature = "native")]
enum Message {
    Entry(Entry),
    Flush(mpsc::SyncSender<()>),
}

#[cfg(feature = "native")]
pub struct Journal {
    db: RhoDb,
    _lock: File,
    sender: mpsc::Sender<Message>,
    path: PathBuf,
}

#[cfg(feature = "native")]
impl Journal {
    pub fn open(state_dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(state_dir)?;
        let lock = acquire_lock(state_dir)?;
        let path = state_dir.join(FILE_NAME);
        let db = RhoDb::open(&path);
        let runtime = tokio::runtime::Builder::new_current_thread().build()?;
        runtime.block_on(async {
            let mut write = db.write().await;
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

#[cfg(feature = "native")]
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

#[cfg(feature = "native")]
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

#[cfg(feature = "native")]
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

#[cfg(feature = "native")]
static GLOBAL: OnceLock<Journal> = OnceLock::new();

#[cfg(feature = "native")]
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
#[cfg(feature = "native")]
pub fn flush() {
    if let Some(journal) = GLOBAL.get()
        && let Err(error) = journal.flush()
    {
        tracing::error!(%error, "failed to flush action journal");
    }
}

/// Records an event if the native journal has been initialized. Browser GUI
/// builds intentionally keep the same one-line API but do not persist locally.
pub fn record(event: Event) {
    #[cfg(feature = "native")]
    if let Some(journal) = GLOBAL.get() {
        journal.record(event);
    }
    #[cfg(not(feature = "native"))]
    let _ = event;
}

#[cfg(feature = "native")]
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

#[cfg(feature = "native")]
#[derive(Serialize)]
struct DumpEntry<'a> {
    sequence: u64,
    timestamp: &'a str,
    event: &'a Event,
}

#[cfg(feature = "native")]
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

#[cfg(all(test, feature = "native"))]
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
        let card = DealerCardIdentity::Agent {
            agent_id: "agent-a".into(),
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
                agent_id: "eng-a".into(),
            },
            dealt_untouched: true,
        };
        let encoded = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&encoded).unwrap(), event);
    }

    #[test]
    fn history_events_round_trip_direction_position_and_methods() {
        let surface = SurfaceIdentity::Transcript {
            agent_id: "eng-a".into(),
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
            card: DealerCardIdentity::Inbox {
                id: "capture-1".into(),
            },
            kind: DealerCardKind::Inbox(DealerInboxKind::Capture),
            verdict: DealerVerdict::Defer,
            skip_until: None,
            occurred_at: "2026-09-01T20:00:00+00:00".into(),
            time_to_verdict_ms: 4200,
            considered_not_dealt: vec![
                DealerCardIdentity::Agent {
                    agent_id: "agent-a".into(),
                },
                DealerCardIdentity::Desk {
                    host: "local".into(),
                    heading_offset: 12,
                },
            ],
        };
        let encoded = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&encoded).unwrap(), event);
    }
}

//! Slack as the frame shows it.
//!
//! [`SlackSystem`] is the one entity behind every Slack surface: the
//! registered workspaces and the session to the first of them. It plugs
//! into the frame under the `slack` kind
//! ([SPEC-slack-addresses](../../specs/SPEC-slack-addresses.md)): a
//! workspace's conversations, a conversation or a thread, and the
//! listings, each opening as a [`SlackSurface`]. What of Slack wants the
//! user is [`crate::cards`]'s to say; Home's rows, the channels and
//! `find`'s are made here. The token and the session stay in this
//! process: no agent host ever sees Slack.

use std::rc::Rc;

use anyhow::Context as _;
use gpui::{App, AppContext as _, Context, Entity, Subscription, Window};
use jiff::Timestamp;
use rho_dealer::facts::Seen;
use rho_dealer::rank::Trace;
use rho_dealer::{Address, Addressed as _, Claim, Marks};
use rho_frame::attention::{Verdict, Write};
use rho_frame::{
    AnySurface, Candidate, ChannelRow, Found, Frame, FrameLink, HomeRow, HomeSection, Point,
    Reversal, Row, Rows, Subject, System,
};
use rho_window::style::StyleClass;

use super::surface::{SlackSurface, surface_at};
use super::{Hooks, SearchKind};
use crate::address::{Listing, SlackAddress, unit_of};
use crate::cards;
use crate::config::{CredentialStore, Credentials, Paths, WorkspaceName};
use crate::health::Signal;
use crate::model::{self, Change, Model, Unit};
use crate::session::{HandledBefore, Session, SessionEvent, Source};
use crate::types::{ChannelId, Conversation, ThreadKey, Ts};

gpui::actions!(
    rho_slack,
    [
        /// Opens what the cursor is on: a conversation, a thread, a hit, a
        /// file, a link, an app's button.
        OpenRow,
        /// Puts the keyboard in the composer, in insert.
        Compose,
        /// Sends what is in the composer, or posts the rewrite.
        Send,
        /// Rewrites the message under the cursor.
        EditMessage,
        /// Rewrites the last message of the user's own.
        EditLast,
        /// Leaves a rewrite, giving the composer back what it held.
        CancelEdit,
        /// What to react to the message under the cursor with.
        React,
        /// Marks the message under the cursor and what follows it unread.
        MarkUnread,
        /// Saves the message under the cursor for later, or unsaves it.
        SaveForLater,
        /// The next conversation with something unread.
        NextUnread,
        /// Narrows the conversations by name.
        Narrow,
        /// Asks Slack for messages.
        FindMessage,
        /// Asks Slack for files.
        FindFile,
        /// Jumps to a conversation by name.
        Switch,
        /// A direct message to people.
        NewMessage,
        /// The search page before this one.
        PreviousPage,
        /// The search page after this one.
        NextPage,
    ]
);

/// The age `mark read before` offers when the user says nothing: a week of
/// backlog is the one the question is usually about.
const DEFAULT_MARK_CUTOFF: &str = "7d";

/// How many matching names a prompt offers while the reader types.
const MATCHES_OFFERED: usize = 64;

pub struct SlackSystem {
    paths: Paths,
    session: Option<Entity<Session>>,
    pub(super) frame: FrameLink,
    _session: Vec<Subscription>,
}

impl SlackSystem {
    /// The system over the client database `db`: Slack's files live beside
    /// it. Starts the session to the first registered workspace, when there
    /// is one.
    pub fn new(db: rho_db::RhoDb, frame: FrameLink, cx: &mut Context<Self>) -> Self {
        let state_dir = db
            .path()
            .parent()
            .expect("the client database is in a directory")
            .to_owned();
        let mut paths = Paths::under(&state_dir);
        // An isolated run (QA, a second profile) points this elsewhere so
        // it cannot touch the real workspaces.
        if let Some(path) = std::env::var_os("RHO_SLACK_CREDENTIALS").filter(|it| !it.is_empty()) {
            paths.credentials = path.into();
        }
        bind_keys(cx);
        let mut system = Self {
            paths,
            session: None,
            frame,
            _session: Vec::new(),
        };
        system.start(cx);
        system
    }

    /// A system over a session built elsewhere: a test's, over the fake.
    pub fn with_session(
        session: Entity<Session>,
        paths: Paths,
        frame: FrameLink,
        cx: &mut Context<Self>,
    ) -> Self {
        bind_keys(cx);
        let mut system = Self {
            paths,
            session: None,
            frame,
            _session: Vec::new(),
        };
        system.adopt(session, cx);
        system
    }

    pub fn session(&self) -> Option<&Entity<Session>> {
        self.session.as_ref()
    }

    fn credentials(&self) -> anyhow::Result<CredentialStore> {
        CredentialStore::open(&self.paths.credentials)
    }

    fn workspaces(&self) -> Vec<WorkspaceName> {
        self.credentials()
            .map(|store| store.workspaces().collect())
            .unwrap_or_default()
    }

    /// The session to the first registered workspace, unless one runs. One
    /// session for the whole client: its mirror has one opener.
    fn start(&mut self, cx: &mut Context<Self>) {
        if self.session.is_some() {
            return;
        }
        let Some(credentials) = self
            .credentials()
            .ok()
            .and_then(|store| store.all().into_iter().next())
        else {
            return;
        };
        let paths = self.paths.clone();
        let session = cx.new(|cx| Session::new(credentials, paths, cx));
        self.adopt(session, cx);
    }

    fn adopt(&mut self, session: Entity<Session>, cx: &mut Context<Self>) {
        self._session = vec![
            cx.subscribe(&session, |this, session, event, cx| {
                this.session_event(session, event, cx)
            }),
            // Whatever moved in Slack may move a claim, a Home row or a
            // channel: the frame asks again once per effect cycle.
            cx.observe(&session, |this, _, cx| {
                this.frame.changed(cx);
                cx.notify();
            }),
        ];
        self.session = Some(session);
        self.frame.changed(cx);
        cx.notify();
    }

    /// The session, if `workspace` is the one it is to.
    fn session_for(&self, workspace: &WorkspaceName, cx: &App) -> Option<&Entity<Session>> {
        self.session
            .as_ref()
            .filter(|session| session.read(cx).model().workspace() == workspace)
    }

    /// The unit an address names, with its session.
    fn unit_of(&self, address: &Address, cx: &App) -> Option<(&Entity<Session>, Unit)> {
        match SlackAddress::from_address(address)? {
            SlackAddress::Unit(workspace, unit) => Some((self.session_for(&workspace, cx)?, unit)),
            _ => None,
        }
    }

    /// The editor chrome and the Markdown pipeline the surfaces borrow, so
    /// Slack reads like every other buffer in the frame.
    pub(super) fn hooks() -> Hooks {
        Hooks {
            configure_editor: |editor, window, cx| {
                rho_window::editor_config::configure(editor, window, cx);
                editor.set_show_compact_gutter(false, cx);
                editor.set_mouse_click_selection_enabled(true, cx);
            },
            configure_markdown: rho_window::markdown::configure_buffer,
            gutter_colour: rho_window::style::user_prompt_gutter_color,
            prompt_style: |cx| StyleClass::UserMessage.resolve(cx),
        }
    }

    fn session_event(
        &mut self,
        session: Entity<Session>,
        event: &SessionEvent,
        cx: &mut Context<Self>,
    ) {
        let frame = self.frame.clone();
        let workspace = session.read(cx).model().workspace().clone();
        match event {
            SessionEvent::OpenConversation(channel) => {
                let address = SlackAddress::Unit(workspace, Unit::conversation(channel)).address();
                open_then(&frame, address, cx, |surface, window, cx| {
                    surface.compose(window, cx)
                });
            }
            SessionEvent::Directory(channels) => {
                let channels = Rc::new(channels.clone());
                frame.with(cx, move |frame, window, cx| {
                    prompt_directory(session, channels, frame, window, cx)
                });
            }
            SessionEvent::Connected => {
                rho_journal::record(rho_journal::Event::SlackConnected {
                    workspace: workspace.0,
                });
            }
            SessionEvent::Disconnected(reason) => {
                rho_journal::record(rho_journal::Event::SlackDisconnected {
                    workspace: workspace.0,
                    reason: reason.clone(),
                });
            }
            SessionEvent::Changed(changes) => {
                for change in changes {
                    rho_journal::record(rho_journal::Event::SlackChanged {
                        change: format!("{change:?}"),
                    });
                    // Slack said a thread is not the user's any more: its
                    // card closes on its own, since the follow list is what
                    // the claims read. Only the record of who did it is
                    // left.
                    if let Change::Muted(Unit {
                        channel,
                        thread: Some(root),
                    }) = change
                    {
                        let model = session.read(cx).model();
                        rho_journal::record(rho_journal::Event::SlackThreadIgnored {
                            thread: journal_thread(model, &model.key(channel, root)),
                            by: rho_journal::IgnoredBy::Slack,
                        });
                    }
                }
            }
            SessionEvent::Replied(key) => {
                rho_journal::record(rho_journal::Event::SlackReplied {
                    thread: journal_thread(session.read(cx).model(), key),
                });
            }
            SessionEvent::Notice(text) => frame.echo(text.clone(), StyleClass::StatusError, cx),
            SessionEvent::Health(Signal::Degraded(reason)) => {
                frame.echo(reason.clone(), StyleClass::StatusError, cx)
            }
            SessionEvent::Health(Signal::Recovered) => {
                frame.echo("slack: caught up", StyleClass::SystemInfo, cx)
            }
            // A search's answer is drawn by the surface waiting for it.
            SessionEvent::Found(_) => {}
            SessionEvent::Dialog { .. }
            | SessionEvent::View { .. }
            | SessionEvent::ViewSubmitted { .. }
            | SessionEvent::ExternalOptions { .. } => frame.echo(
                "slack: app dialogs are not supported here yet; use Slack for this one",
                StyleClass::SystemInfo,
                cx,
            ),
        }
        self.frame.changed(cx);
    }

    /// The source a unit opens as: a followed thread has its own, and a
    /// conversation opens as the room it is.
    fn source(model: &Model, unit: &Unit) -> Source {
        match &unit.thread {
            Some(root) => Source::Thread(model.key(&unit.channel, root)),
            None => Source::Conversation(unit.channel.clone()),
        }
    }

    /// The rows of the `slack` menu every screen has. Built while the
    /// system may be leased, so it reads nothing of it.
    pub(super) fn menu_rows(
        this: &Entity<Self>,
        session: Option<Entity<Session>>,
        cx: &App,
    ) -> Vec<Row> {
        let register = {
            let this = this.clone();
            Row::run("r", "register workspace…", move |frame, window, cx| {
                prompt_register(this.clone(), frame, window, cx)
            })
        };
        let Some(session) = session else {
            return vec![register];
        };
        let workspace = session.read(cx).model().workspace().clone();
        let open = |key: &str, description: &str, address: SlackAddress| {
            let address = address.address();
            Row::run(key, description, move |frame, window, cx| {
                frame.open(address.clone(), window, cx)
            })
        };
        let with =
            |key: &str,
             description: &str,
             run: fn(Entity<Session>, &mut Frame, &mut Window, &mut Context<Frame>)| {
                let session = session.clone();
                Row::run(key, description, move |frame, window, cx| {
                    run(session.clone(), frame, window, cx)
                })
            };
        vec![
            open(
                "o",
                "conversations",
                SlackAddress::Conversations(workspace.clone()),
            ),
            with("j", "jump…", prompt_switch),
            with("n", "new message…", prompt_people),
            with("c", "browse channels…", |session, frame, _, cx| {
                frame.echo(
                    "slack: loading the channel directory…",
                    StyleClass::SystemInfo,
                    cx,
                );
                session.update(cx, |session, cx| session.browse_channels(cx));
            }),
            with("s", "search messages…", |session, frame, window, cx| {
                prompt_find(session, None, SearchKind::Messages, frame, window, cx)
            }),
            with("f", "search files…", |session, frame, window, cx| {
                prompt_find(session, None, SearchKind::Files, frame, window, cx)
            }),
            open(
                "v",
                "activity",
                SlackAddress::Listing(workspace.clone(), Listing::Activity),
            ),
            open(
                "l",
                "saved (local)",
                SlackAddress::Listing(workspace, Listing::Saved),
            ),
            with("d", "drafts…", prompt_drafts),
            with("m", "mark read before…", prompt_mark_read_before),
            register,
        ]
    }
}

/// Opens `address`, then does `then` to its surface.
pub(super) fn open_then(
    frame: &FrameLink,
    address: Address,
    cx: &mut App,
    then: impl FnOnce(&mut SlackSurface, &mut Window, &mut Context<SlackSurface>) + 'static,
) {
    frame.with(cx, move |frame, window, cx| {
        frame.open(address.clone(), window, cx);
        if frame.current_address() != &address {
            return;
        }
        if let Some(surface) = surface_at(frame, &address) {
            surface.update(cx, |surface, cx| then(surface, window, cx));
        }
    });
}

impl System for SlackSystem {
    fn kinds(&self) -> &'static [&'static str] {
        &[SlackAddress::KIND]
    }

    fn open(
        &mut self,
        address: &Address,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnySurface> {
        let place = SlackAddress::from_address(address)?;
        let session = self.session_for(place.workspace(), cx)?.clone();
        let system = cx.entity();
        let frame = self.frame.clone();
        let surface = match place {
            SlackAddress::Conversations(_) => {
                cx.new(|cx| SlackSurface::list(system, frame, session, window, cx))
            }
            SlackAddress::Unit(_, unit) => {
                let model = session.read(cx).model();
                let source = Self::source(model, &unit);
                // Opened while it asks for the reader, it lands on the
                // oldest message from someone else they have not dealt
                // with: three mentions are one card, and this is the first.
                let land = model.attention(&unit).and_then(|_| {
                    let cursor = model.handled_through(&unit).cloned();
                    session
                        .read(cx)
                        .oldest_from_other_after(&unit, cursor.as_ref())
                });
                cx.new(|cx| {
                    SlackSurface::conversation(system, frame, session, source, land, window, cx)
                })
            }
            SlackAddress::Listing(_, listing) => {
                cx.new(|cx| SlackSurface::listing(system, frame, session, listing, window, cx))
            }
        };
        Some(AnySurface::new(surface))
    }

    fn rows(&mut self, point: &Point, _window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        // A Slack surface offers the menu itself, with its own rows in it.
        if point.surface.kind() == SlackAddress::KIND {
            return Vec::new();
        }
        let rows = Self::menu_rows(&cx.entity(), self.session.clone(), cx);
        let mut menu = Rows::new("slack");
        menu.extend(rows);
        vec![Row::menu("s", "slack", menu)]
    }

    fn claims(
        &self,
        marks: &Marks,
        now: Timestamp,
        trace: Option<&mut Trace>,
        cx: &App,
    ) -> Vec<Claim> {
        let Some(session) = &self.session else {
            return Vec::new();
        };
        let session = session.read(cx);
        cards::claims(session.model(), session.mirror(), marks, now, trace)
    }

    fn name(&self, address: &Address, cx: &App) -> Option<(String, String)> {
        let (session, unit) = self.unit_of(address, cx)?;
        let session = session.read(cx);
        Some(cards::name(session.model(), session.mirror(), &unit))
    }

    fn seen(&self, address: &Address, cx: &App) -> Seen {
        match self.unit_of(address, cx) {
            Some((session, unit)) => cards::seen(session.read(cx).model(), &unit),
            None => Seen::Whole,
        }
    }

    /// What Slack is asking the reader about, newest first, short of what
    /// they said they are done with: the unread threads, conversations
    /// and mentions at a glance.
    fn home(&self, marks: &Marks, cx: &App) -> Vec<HomeSection> {
        /// Home is a glance; the rest is in activity.
        const CAP: usize = 12;
        let Some(session) = &self.session else {
            return Vec::new();
        };
        let session = session.read(cx);
        let model = session.model();
        let now = Timestamp::now();
        let mut rows: Vec<(f64, HomeRow)> = model
            .asking()
            .filter_map(|(unit, _)| {
                let facts = model.unit(unit)?;
                let address = cards::address(model, unit);
                let done = marks
                    .get(&address)
                    .facts()
                    .seen_slack()
                    .is_some_and(|seen| {
                        rho_dealer::facts::slack_ts_order(&facts.newest.0, seen).is_le()
                    });
                if done {
                    return None;
                }
                let (title, context) = cards::name(model, session.mirror(), unit);
                let title = match context.is_empty() {
                    true => title,
                    false => format!("{context} › {title}"),
                };
                let at = facts.newest.epoch_seconds();
                Some((
                    at,
                    HomeRow {
                        address,
                        title,
                        label: ago(facts.newest.millis(), now),
                        unread: true,
                    },
                ))
            })
            .collect();
        rows.sort_by(|a, b| b.0.total_cmp(&a.0));
        let rows: Vec<HomeRow> = rows.into_iter().take(CAP).map(|(_, row)| row).collect();
        if rows.is_empty() {
            return Vec::new();
        }
        vec![HomeSection {
            title: "slack".to_owned(),
            rows,
        }]
    }

    /// The conversations the user starred: the Slack channels they follow
    /// from Home, tagged with the workspace.
    fn channels(&self, _marks: &Marks, cx: &App) -> Vec<ChannelRow> {
        let Some(session) = &self.session else {
            return Vec::new();
        };
        let session = session.read(cx);
        let model = session.model();
        let Some(mirror) = session.mirror() else {
            return Vec::new();
        };
        let workspace = model.workspace();
        let favorites = mirror.favorites(&workspace.0);
        let mut rows: Vec<ChannelRow> = session
            .rows()
            .into_iter()
            .filter(|row| favorites.contains(&row.id))
            .map(|row| ChannelRow {
                address: SlackAddress::Unit(workspace.clone(), Unit::conversation(&row.id))
                    .address(),
                name: row.label,
                unread: row.unread && !row.muted,
                tag: workspace.0.clone(),
            })
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    /// Every conversation the user is in, and every thread rho tracks.
    fn find(&self, _marks: &Marks, cx: &App) -> Vec<Found> {
        let Some(session) = &self.session else {
            return Vec::new();
        };
        let session = session.read(cx);
        let model = session.model();
        let workspace = model.workspace();
        let mut found: Vec<Found> = session
            .rows()
            .into_iter()
            .map(|row| Found {
                address: SlackAddress::Unit(workspace.clone(), Unit::conversation(&row.id))
                    .address(),
                title: row.label,
                description: format!("slack · {workspace}"),
                recency: row.latest.map_or(0, |ts| ts.millis().max(0) as u64),
            })
            .collect();
        for unit in model.tracked() {
            if unit.thread.is_none() {
                continue;
            }
            let Some(facts) = model.unit(&unit) else {
                continue;
            };
            let (title, context) = cards::name(model, session.mirror(), &unit);
            found.push(Found {
                address: cards::address(model, &unit),
                title,
                description: format!("slack › {context} · thread"),
                recency: facts.newest.millis().max(0) as u64,
            });
        }
        found
    }

    /// A Slack mute is Slack's: a conversation muted or a thread
    /// unfollowed there, which the user can lift from any client.
    fn mutes_itself(&self, address: &Address) -> bool {
        matches!(
            SlackAddress::from_address(address),
            Some(SlackAddress::Unit(..))
        )
    }

    fn said(
        &mut self,
        address: &Address,
        verdict: &Verdict,
        cx: &mut Context<Self>,
    ) -> Option<Reversal> {
        let (session, unit) = self.unit_of(address, cx)?;
        let session = session.clone();
        let said = session.update(cx, |session, cx| say(session, &unit, verdict, cx))?;
        let session = session.downgrade();
        Some(Box::new(move |cx| {
            let _ = session.update(cx, |session, cx| unsay(session, said, cx));
        }))
    }
}

/// What a verdict did in Slack, for the undo to take back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Said {
    unit: Unit,
    /// Where the unit's cursor stood, when the verdict moved it.
    before: Option<HandledBefore>,
    /// Whether the verdict muted the unit in Slack.
    muted: bool,
}

/// What a verdict on `unit` does in Slack. Done and mute mean the user is
/// through with what is there: rho's cursor moves to its newest message,
/// and the outbox tells Slack, so every other client reads it as read. A
/// mute is Slack's own too, a conversation muted or a thread unfollowed
/// there, so it holds wherever they read Slack (S6). A snooze is only
/// rho's.
pub fn say(
    session: &mut Session,
    unit: &Unit,
    verdict: &Verdict,
    cx: &mut Context<Session>,
) -> Option<Said> {
    if matches!(verdict, Verdict::Snooze(_)) {
        return None;
    }
    let newest = session.model().unit(unit).map(|facts| facts.newest.clone());
    let before = newest.and_then(|newest| {
        let before = session.handled_before(unit);
        session.mark_handled(unit, &newest, cx).then_some(before)
    });
    let muted = *verdict == Verdict::Mute;
    if muted {
        set_muted(session, unit, true, cx);
    }
    Some(Said {
        unit: unit.clone(),
        before,
        muted,
    })
}

/// Takes back what [`say`] did in Slack.
pub fn unsay(session: &mut Session, said: Said, cx: &mut Context<Session>) {
    if said.muted {
        set_muted(session, &said.unit, false, cx);
    }
    if let Some(before) = &said.before {
        session.undo_handled(&said.unit, before, cx);
    }
}

/// Slack's own mute: a conversation muted, or a thread unfollowed.
fn set_muted(session: &mut Session, unit: &Unit, muted: bool, cx: &mut Context<Session>) {
    match &unit.thread {
        Some(root) => {
            let key = session.model().key(&unit.channel, root);
            if muted {
                rho_journal::record(rho_journal::Event::SlackThreadIgnored {
                    thread: journal_thread(session.model(), &key),
                    by: rho_journal::IgnoredBy::Rho,
                });
                session.ignore_thread(&key, cx);
            } else {
                session.follow_thread(&key, cx);
            }
        }
        None => session.set_unit_muted(unit, muted, cx),
    }
}

fn journal_thread(model: &Model, key: &ThreadKey) -> rho_journal::SlackThread {
    rho_journal::SlackThread {
        workspace: key.workspace.0.clone(),
        conversation: model.label(&key.channel),
        thread: key.thread_ts.0.clone(),
    }
}

/// How long ago, in the deal bar's units.
fn ago(at_ms: i64, now: Timestamp) -> String {
    let minutes = (now.as_millisecond() - at_ms).max(0) / 60_000;
    match minutes {
        0 => "now".to_owned(),
        1..60 => format!("{minutes}m"),
        60..1440 => format!("{}h", minutes / 60),
        _ => format!("{}d", minutes / 1440),
    }
}

// ---- prompts ----

fn whole_input(frame: &mut Frame) {
    if let Some(minibuffer) = frame.minibuffer_mut() {
        minibuffer.set_complete_whole_input();
    }
}

fn secret(frame: &mut Frame) {
    if let Some(minibuffer) = frame.minibuffer_mut() {
        minibuffer.set_secret();
    }
}

/// Registers a workspace by hand: name, then token, then cookie. Three
/// prompts rather than one line because the token and cookie are long
/// pastes, and a mistyped line would have to be pasted again whole.
fn prompt_register(
    this: Entity<SlackSystem>,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    let names = this.read(cx).workspaces();
    frame.open_prompt(
        "slack workspace:",
        // Completing over the registered names is what makes this prompt
        // double as "replace a rotated token".
        Rc::new(move |_, needle, _| {
            let needle = needle.trim().to_lowercase();
            names
                .iter()
                .filter(|name| name.0.to_lowercase().contains(&needle))
                .map(|name| Candidate {
                    value: name.0.clone(),
                    description: "registered".to_owned(),
                })
                .collect()
        }),
        Rc::new(move |frame, input, window, cx| {
            let name = input.trim().to_owned();
            if name.is_empty() {
                return;
            }
            let this = this.clone();
            frame.open_prompt(
                format!("{name} xoxc token:"),
                Rc::new(|_, _, _| Vec::new()),
                Rc::new(move |frame, input, window, cx| {
                    let token = input.trim().to_owned();
                    if token.is_empty() {
                        return;
                    }
                    let (this, name) = (this.clone(), name.clone());
                    frame.open_prompt(
                        format!("{name} d cookie:"),
                        Rc::new(|_, _, _| Vec::new()),
                        Rc::new(move |frame, cookie, _, cx| {
                            let registered = this.update(cx, |system, cx| {
                                let registered = system.register(&name, &token, &cookie);
                                system.start(cx);
                                registered
                            });
                            match registered {
                                Ok(workspace) => frame.echo(
                                    &format!("slack: {workspace} registered"),
                                    StyleClass::SystemInfo,
                                    cx,
                                ),
                                // What was wrong with the input, never the
                                // input: a token on the echo line is a
                                // token on screen.
                                Err(error) => frame.echo(
                                    &format!("slack: {error:#}"),
                                    StyleClass::StatusError,
                                    cx,
                                ),
                            }
                        }),
                        window,
                        cx,
                    );
                    secret(frame);
                }),
                window,
                cx,
            );
            secret(frame);
        }),
        window,
        cx,
    );
}

impl SlackSystem {
    fn register(&mut self, name: &str, token: &str, cookie: &str) -> anyhow::Result<WorkspaceName> {
        let credentials = Credentials::parse(name, token, cookie)?;
        let workspace = credentials.workspace.clone();
        self.credentials()
            .context("the credential store")?
            .register(credentials)?;
        Ok(workspace)
    }
}

/// `ctrl-p`: a conversation, by name.
pub(super) fn prompt_switch(
    session: Entity<Session>,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    let reading = session.clone();
    frame.open_prompt(
        "jump to conversation:",
        Rc::new(move |_, query, cx| {
            let session = reading.read(cx);
            session
                .reached_by(query, MATCHES_OFFERED)
                .into_iter()
                .map(|row| Candidate {
                    description: [
                        session.favorite(&row.id).then_some("starred"),
                        row.unread.then_some("unread"),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · "),
                    value: row.label,
                })
                .collect()
        }),
        Rc::new(move |frame, input, window, cx| {
            let session = session.read(cx);
            let model = session.model();
            let rows = session.rows();
            let input = input.trim();
            let row = rows
                .iter()
                .find(|row| row.label.eq_ignore_ascii_case(input))
                .or_else(|| {
                    rows.iter()
                        .find(|row| row.label.to_lowercase().contains(&input.to_lowercase()))
                });
            match row {
                Some(row) => frame.open(
                    SlackAddress::Unit(model.workspace().clone(), Unit::conversation(&row.id))
                        .address(),
                    window,
                    cx,
                ),
                None => frame.echo(
                    &format!("slack: no conversation called {input}"),
                    StyleClass::SystemInfo,
                    cx,
                ),
            }
        }),
        window,
        cx,
    );
    whole_input(frame);
}

/// `ctrl-n`: a direct message to one to eight people, opened by Slack and
/// then answered with [`SessionEvent::OpenConversation`].
pub(super) fn prompt_people(
    session: Entity<Session>,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    let reading = session.clone();
    frame.open_prompt(
        "new message — people (comma separated):",
        Rc::new(move |_, query, cx| {
            let (prefix, needle) = query
                .rsplit_once(',')
                .map_or(("", query), |(prefix, needle)| (prefix, needle));
            let needle = needle.trim().trim_start_matches('@').to_lowercase();
            reading
                .read(cx)
                .model()
                .people()
                .into_iter()
                .filter(|user| {
                    user.name.to_lowercase().contains(&needle)
                        || user.handle.to_lowercase().contains(&needle)
                })
                .map(|user| Candidate {
                    value: match prefix.is_empty() {
                        true => format!("@{}", user.handle),
                        false => format!("{prefix}, @{}", user.handle),
                    },
                    description: user.name,
                })
                .collect()
        }),
        Rc::new(move |frame, input, _, cx| {
            let people = session.read(cx).model().people();
            let mut recipients = Vec::new();
            for name in input
                .split(',')
                .map(|name| name.trim().trim_start_matches('@'))
            {
                let matches: Vec<_> = people
                    .iter()
                    .filter(|user| {
                        user.handle.eq_ignore_ascii_case(name)
                            || user.name.eq_ignore_ascii_case(name)
                    })
                    .collect();
                if matches.len() != 1 {
                    frame.echo(
                        "slack: choose each person's exact @handle; separate people with commas",
                        StyleClass::SystemInfo,
                        cx,
                    );
                    return;
                }
                if !recipients.contains(&matches[0].id) {
                    recipients.push(matches[0].id.clone());
                }
            }
            if recipients.is_empty() || recipients.len() > 8 {
                frame.echo(
                    "slack: choose one to eight people",
                    StyleClass::SystemInfo,
                    cx,
                );
                return;
            }
            session.update(cx, |session, cx| session.open_direct(recipients, cx));
        }),
        window,
        cx,
    );
    whole_input(frame);
}

/// The channel directory Slack sent: one to join and open.
fn prompt_directory(
    session: Entity<Session>,
    channels: Rc<Vec<Conversation>>,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    let choices = channels.clone();
    frame.open_prompt(
        "browse channels — choose to join and open:",
        Rc::new(move |_, query, _| {
            let query = query.trim_start_matches('#').to_lowercase();
            choices
                .iter()
                .filter(|channel| channel.name.to_lowercase().contains(&query))
                .map(|channel| Candidate {
                    value: format!("#{}", channel.name),
                    description: "join and open".into(),
                })
                .collect()
        }),
        Rc::new(move |_, input, _, cx| {
            let wanted = input.trim().trim_start_matches('#');
            let Some(channel) = channels
                .iter()
                .find(|channel| channel.name.eq_ignore_ascii_case(wanted))
            else {
                return;
            };
            session.update(cx, |session, cx| {
                session.join_channel(channel.id.clone(), cx)
            });
        }),
        window,
        cx,
    );
    whole_input(frame);
}

/// What Slack's search understands beyond words, offered as completions.
pub(super) fn filter_candidates(typed: &str) -> Vec<Candidate> {
    let token = typed.split_whitespace().last().unwrap_or_default();
    [
        ("from:", "messages or files from a person"),
        ("in:", "search one channel or direct message"),
        ("before:", "before a date, for example before:2025-01-31"),
        ("after:", "after a date, for example after:2025-01-01"),
        ("has:", "with a file, link, reaction, pin, or star"),
        ("is:", "saved items or thread replies"),
    ]
    .into_iter()
    .filter(|(operator, _)| token.is_empty() || operator.starts_with(token))
    .map(|(value, description)| Candidate {
        value: value.to_owned(),
        description: description.to_owned(),
    })
    .collect()
}

/// Asks Slack for messages or files, everywhere or in one conversation
/// (`scope` is Slack's `in:` word for it and the label the prompt says).
/// The listing opens at once, saying it is asking.
pub(super) fn prompt_find(
    session: Entity<Session>,
    scope: Option<(String, String)>,
    kind: SearchKind,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    let noun = match kind {
        SearchKind::Messages => "messages",
        SearchKind::Files => "files",
    };
    let prompt = match &scope {
        Some((_, label)) => format!("slack {noun} in {label} (from: before: after: has: is:):"),
        None => format!("slack {noun} (from: in: before: after: has: is:):"),
    };
    frame.open_prompt(
        prompt,
        Rc::new(|_, typed, _| filter_candidates(typed)),
        Rc::new(move |frame, input, window, cx| {
            let query = match (input.trim(), &scope) {
                ("", _) => return,
                (input, Some((scope, _))) => format!("{input} in:{scope}"),
                (input, None) => input.to_owned(),
            };
            let workspace = session.read(cx).model().workspace().clone();
            let listing = match kind {
                SearchKind::Messages => Listing::Messages,
                SearchKind::Files => Listing::Files,
            };
            let address = SlackAddress::Listing(workspace, listing).address();
            frame.open(address.clone(), window, cx);
            if let Some(surface) = surface_at(frame, &address) {
                surface.update(cx, |surface, cx| surface.ask(&query, kind, window, cx));
            }
        }),
        window,
        cx,
    );
}

/// The drafts kept for every conversation: one opens at its composer.
fn prompt_drafts(
    session: Entity<Session>,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    let workspace = session.read(cx).model().workspace().clone();
    let drafts: Vec<(String, String, Address)> = session
        .read(cx)
        .drafts()
        .into_iter()
        .map(|(source, draft)| {
            let label = session.read(cx).label(&source);
            let snippet: String = draft
                .text
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(80)
                .collect();
            let description = match (snippet.is_empty(), draft.files.len()) {
                (false, 0) => snippet,
                (false, count) => format!("{snippet} · {count} file(s)"),
                (true, count) => format!("{count} file(s)"),
            };
            let address = SlackAddress::of_source(&workspace, &source).address();
            (label, description, address)
        })
        .collect();
    if drafts.is_empty() {
        frame.echo("slack: no drafts", StyleClass::SystemInfo, cx);
        return;
    }
    let drafts = Rc::new(drafts);
    let complete = drafts.clone();
    frame.open_prompt(
        "slack drafts:",
        Rc::new(move |_, needle, _| {
            let needle = needle.to_lowercase();
            complete
                .iter()
                .filter(|(label, _, _)| label.to_lowercase().contains(&needle))
                .map(|(label, description, _)| Candidate {
                    value: label.clone(),
                    description: description.clone(),
                })
                .collect()
        }),
        Rc::new(move |frame, input, window, cx| {
            let chosen = match input.trim() {
                "" => drafts.first(),
                input => drafts.iter().find(|(label, _, _)| label == input),
            };
            let Some((_, _, address)) = chosen else {
                return;
            };
            frame.open(address.clone(), window, cx);
            if let Some(surface) = surface_at(frame, address) {
                surface.update(cx, |surface, cx| surface.compose(window, cx));
            }
        }),
        window,
        cx,
    );
    whole_input(frame);
}

fn plural(count: usize, noun: &str) -> String {
    match count {
        1 => format!("1 {noun}"),
        _ => format!("{count} {noun}s"),
    }
}

fn mark_cutoff_text(input: &str) -> String {
    match input.trim() {
        "" => DEFAULT_MARK_CUTOFF.to_owned(),
        text => text.to_owned(),
    }
}

/// A cutoff the user typed: an age back from `now` (`45m`, `6h`, `7d`) or
/// a local date.
fn parse_mark_cutoff(
    input: &str,
    now: chrono::DateTime<chrono::Local>,
) -> Option<chrono::DateTime<chrono::Local>> {
    use chrono::TimeZone as _;

    let text = input.trim();
    if let Ok(date) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return chrono::Local
            .from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
            .earliest();
    }
    let (digits, unit) = match text.find(|c: char| !c.is_ascii_digit()) {
        Some(at) => text.split_at(at),
        None => (text, "m"),
    };
    let count: i64 = digits.parse().ok()?;
    let minutes = match unit {
        "m" | "min" => count,
        "h" | "hr" => count.checked_mul(60)?,
        "d" => count.checked_mul(60 * 24)?,
        _ => return None,
    };
    now.checked_sub_signed(chrono::TimeDelta::try_minutes(minutes)?)
}

/// `mark read before`: the backlog older than a cutoff, marked read in
/// Slack and done here, as one thing `shift-u` takes back. The prompt
/// counts what it would touch before anything happens, because Slack's
/// half is not reversible there.
fn prompt_mark_read_before(
    session: Entity<Session>,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    let counting = session.clone();
    frame.open_prompt(
        format!("mark read before ({DEFAULT_MARK_CUTOFF}):"),
        Rc::new(move |_, needle, cx| {
            let cutoff = mark_cutoff_text(needle);
            let description = match parse_mark_cutoff(&cutoff, chrono::Local::now()) {
                Some(before) => {
                    let plan = counting
                        .read(cx)
                        .model()
                        .mark_plan(before.timestamp() as f64);
                    format!(
                        "{} · {} · enter",
                        plural(plan.conversations.len(), "conversation"),
                        plural(plan.threads.len(), "thread")
                    )
                }
                None => "an age like 7d, or a date like 2026-08-15".to_owned(),
            };
            vec![Candidate {
                value: cutoff,
                description,
            }]
        }),
        Rc::new(move |frame, input, _, cx| mark_read_before(&session, &input, frame, cx)),
        window,
        cx,
    );
}

fn mark_read_before(
    session: &Entity<Session>,
    input: &str,
    frame: &mut Frame,
    cx: &mut Context<Frame>,
) {
    let text = mark_cutoff_text(input);
    let Some(cutoff) = parse_mark_cutoff(&text, chrono::Local::now()) else {
        frame.echo(
            &format!("mark read before: {text} is not an age or a date"),
            StyleClass::SystemInfo,
            cx,
        );
        return;
    };
    let before = cutoff.timestamp() as f64;
    let model = session.read(cx).model();
    let plan = model.mark_plan(before);
    let (conversations, threads) = (plan.conversations.len(), plan.threads.len());
    // Every unit the plan covers is done at the newest message at or
    // before the cutoff, so anything that arrived since is still theirs;
    // a card older than the cutoff that Slack has nothing unread for is
    // backlog just the same, closed at its own newest.
    let mut units: Vec<(Unit, Ts)> = plan
        .conversations
        .iter()
        .map(|(channel, ts)| (Unit::conversation(channel), ts.clone()))
        .chain(
            plan.threads
                .iter()
                .map(|(key, ts)| (Unit::thread(&key.channel, &key.thread_ts), ts.clone())),
        )
        .collect();
    for card in frame.hand().cards.clone() {
        let Some(SlackAddress::Unit(workspace, unit)) = SlackAddress::from_address(&card.address)
        else {
            continue;
        };
        if &workspace != model.workspace() || units.iter().any(|(known, _)| *known == unit) {
            continue;
        }
        if let Some(closed) = model.closed_by(&unit, before) {
            units.push((unit, closed));
        }
    }
    if units.is_empty() {
        frame.echo(
            &format!("mark read before {text}: nothing that old"),
            StyleClass::SystemInfo,
            cx,
        );
        return;
    }
    // A unit is done at the cutoff and no further: anything newer is still
    // the reader's. One whose cursor was past it already is left alone.
    let moved: Vec<(Unit, Ts, HandledBefore)> = session.update(cx, |session, cx| {
        session.mark_read_before(plan, cx);
        units
            .into_iter()
            .filter_map(|(unit, ts)| {
                let before = session.handled_before(&unit);
                session
                    .mark_handled(&unit, &ts, cx)
                    .then_some((unit, ts, before))
            })
            .collect()
    });
    let model = session.read(cx).model();
    let writes: Vec<Write> = moved
        .iter()
        .map(|(unit, ts, _)| {
            Write::Fact(rho_dealer::facts::Fact::Settled {
                node: cards::address(model, unit),
                seen: Seen::Slack(ts.0.clone()),
            })
        })
        .collect();
    let closed = moved.len();
    let undo = session.downgrade();
    frame.say(
        "mark read before".to_owned(),
        writes,
        vec![Box::new(move |cx| {
            let _ = undo.update(cx, |session, cx| {
                for (unit, _, before) in &moved {
                    session.undo_handled(unit, before, cx);
                }
            });
            rho_journal::record(rho_journal::Event::SlackMarkReadBeforeUndone { cards: closed });
        })],
        cx,
    );
    rho_journal::record(rho_journal::Event::SlackMarkedReadBefore {
        cutoff: text.clone(),
        conversations,
        threads,
    });
    frame.echo(
        &format!(
            "marked read before {text}: {} · {} · {closed} closed",
            plural(conversations, "conversation"),
            plural(threads, "thread")
        ),
        StyleClass::SystemInfo,
        cx,
    );
}

/// `shift-n`: the next conversation with something unread, or the list
/// when there is none.
pub(super) fn next_unread(
    session: &Entity<Session>,
    here: Option<&ChannelId>,
    frame: &FrameLink,
    cx: &mut App,
) {
    let model = session.read(cx).model();
    let workspace = model.workspace().clone();
    match model.next_unread(here) {
        model::NextUnread::Go(channel) => frame.open(
            SlackAddress::Unit(workspace, Unit::conversation(&channel)).address(),
            cx,
        ),
        // The reader has read everything the narrowing reaches. The key
        // stops here and says what it is not showing them.
        model::NextUnread::Outside(waiting) => {
            frame.echo(outside_the_narrowing(waiting), StyleClass::SystemInfo, cx)
        }
        model::NextUnread::Nothing => {
            frame.open(SlackAddress::Conversations(workspace).address(), cx)
        }
    }
}

fn outside_the_narrowing(waiting: usize) -> String {
    let plural = match waiting {
        1 => "conversation",
        _ => "conversations",
    };
    format!(
        "slack: {waiting} unread {plural} outside the narrowing; \
         s with an empty query shows every conversation"
    )
}

/// The address of what a source shows.
pub(super) fn address_of(model: &Model, source: &Source) -> Address {
    SlackAddress::Unit(model.workspace().clone(), unit_of(source)).address()
}

/// What a surface over the conversations has the point on.
pub(super) fn subject_of(model: &Model, source: Option<Source>) -> Subject {
    match source {
        Some(source) => Subject::Address(address_of(model, &source)),
        None => Subject::None,
    }
}

fn bind_keys(cx: &mut App) {
    use gpui::KeyBinding;
    let mut bindings = Vec::new();
    // Deal mode is not here, so these need no `!VimDeal`: a key means one
    // thing per context.
    for context in rho_frame::normal_contexts("RhoSlackList", "") {
        bindings.extend([
            KeyBinding::new("enter", OpenRow, Some(&context)),
            // `s` narrows the list by name; `shift-s` asks Slack what people
            // said, which is a request over a network and not an index.
            KeyBinding::new("s", Narrow, Some(&context)),
        ]);
    }
    for context in rho_frame::normal_contexts("RhoSlackConversation", "") {
        bindings.extend([
            KeyBinding::new("enter", OpenRow, Some(&context)),
            KeyBinding::new("i", Compose, Some(&context)),
            // On a line that is not one of the user's messages these give the
            // key back to vim.
            KeyBinding::new("e", EditMessage, Some(&context)),
            KeyBinding::new("r", React, Some(&context)),
            KeyBinding::new("m", MarkUnread, Some(&context)),
            KeyBinding::new("b", SaveForLater, Some(&context)),
        ]);
    }
    for surface in ["RhoSlackList", "RhoSlackConversation"] {
        for context in rho_frame::normal_contexts(surface, "") {
            bindings.extend([
                KeyBinding::new("shift-s", FindMessage, Some(&context)),
                KeyBinding::new("ctrl-shift-s", FindFile, Some(&context)),
                KeyBinding::new("ctrl-p", Switch, Some(&context)),
                KeyBinding::new("ctrl-n", NewMessage, Some(&context)),
                // `shift-n` and not `n`: `n` is the search the reader just ran.
                KeyBinding::new("shift-n", NextUnread, Some(&context)),
            ]);
        }
    }
    // The composer is the end of the conversation's own buffer, so it has
    // vim's modes: insert is where these mean what they say. With the
    // completion menu open the keys are the menu's.
    let insert = "RhoSlackConversation > Editor && vim_mode == insert && !showing_completions";
    bindings.extend([
        KeyBinding::new("enter", Send, Some(insert)),
        KeyBinding::new("shift-enter", editor::actions::Newline, Some(insert)),
        // `up` on an empty composer is Slack's own habit; `escape` with a
        // rewrite open gives back what the composer held. Both fall through
        // to the editor's answer otherwise.
        KeyBinding::new("up", EditLast, Some(insert)),
        KeyBinding::new("escape", CancelEdit, Some(insert)),
    ]);
    let results = "RhoSlackResults > Editor";
    bindings.extend([
        KeyBinding::new("enter", OpenRow, Some(results)),
        KeyBinding::new("[", PreviousPage, Some(results)),
        KeyBinding::new("]", NextPage, Some(results)),
        KeyBinding::new("shift-s", FindMessage, Some(results)),
        KeyBinding::new("ctrl-shift-s", FindFile, Some(results)),
    ]);
    cx.bind_keys(bindings);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cutoff_is_an_age_or_a_date() {
        let now = chrono::Local::now();
        assert_eq!(
            mark_cutoff_text("  "),
            "7d",
            "nothing typed means the default"
        );
        assert_eq!((now - parse_mark_cutoff("7d", now).unwrap()).num_days(), 7);
        assert_eq!((now - parse_mark_cutoff("6h", now).unwrap()).num_hours(), 6);
        assert_eq!(
            (now - parse_mark_cutoff("45", now).unwrap()).num_minutes(),
            45
        );
        let date = parse_mark_cutoff("2026-08-15", now).unwrap();
        assert_eq!(
            date.format("%Y-%m-%d %H:%M").to_string(),
            "2026-08-15 00:00"
        );
        assert!(parse_mark_cutoff("last tuesday", now).is_none());
    }

    #[test]
    fn the_edge_of_a_narrowing_reads_like_english() {
        assert!(outside_the_narrowing(1).starts_with("slack: 1 unread conversation outside"));
        assert!(outside_the_narrowing(3).starts_with("slack: 3 unread conversations outside"));
    }

    #[test]
    fn ago_reads_in_the_bars_units() {
        let now = Timestamp::from_millisecond(10_000_000_000).unwrap();
        let ms = |mins: i64| now.as_millisecond() - mins * 60_000;
        assert_eq!(ago(ms(0), now), "now");
        assert_eq!(ago(ms(5), now), "5m");
        assert_eq!(ago(ms(130), now), "2h");
        assert_eq!(ago(ms(3000), now), "2d");
    }
}

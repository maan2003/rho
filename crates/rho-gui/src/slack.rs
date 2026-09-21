//! Slack in the GUI: registering workspaces, and (from here on) the live
//! session, the items it raises, and the thread surface.
//!
//! The Slack client itself lives in `rho-slack`; this module is only the
//! seam where it meets the workspace, the inbox, and the journal.

use anyhow::Context as _;
use gpui::AppContext as _;
use rho_desk::cells::SlackUnit;
use rho_slack::config::{CredentialStore, Credentials, WorkspaceName};
use rho_slack::health::Signal;
use rho_slack::model::{Change, Model, NextUnread, Unit};
use rho_slack::session::{HandledBefore, Session, SessionEvent, Source};
use rho_slack::types::{ChannelId, ThreadKey, Ts, human_size};
use rho_slack::ui::conversation::{Attaching, EditStart};
use rho_window::style::StyleClass;

/// Slack has no way to put a file on a message that already exists, so an
/// attachment offered mid-rewrite is refused rather than sent as a second
/// message. Said the same way whichever of the three ways in was used.
const NOT_WHILE_EDITING: &str =
    "slack: a rewrite cannot carry a picture; finish or leave the edit first";

/// The message a rewrite was open on has been deleted from somewhere else.
/// Slack will not update a message that is not there, so the rewrite is
/// closed rather than left to be refused on every enter, and the words are
/// in the composer to send as a new message if the reader still wants them.
const REWRITE_LOST: &str = "slack: that message was deleted; your rewrite is in the composer";

use crate::dashboard::SlackFacts;
use crate::minibuffer::Candidate;
use crate::pane::{SlackInventoryKind, SurfaceKey};
use crate::workspace::{ContextId, SurfaceView, Workspace};

pub(crate) fn slack_filter_candidates(typed: &str) -> Vec<crate::minibuffer::Candidate> {
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
    .map(|(value, description)| crate::minibuffer::Candidate {
        value: value.to_owned(),
        description: description.to_owned(),
    })
    .collect()
}

impl Workspace {
    /// Registers a workspace by hand: name, then token, then cookie. Three
    /// prompts rather than one line because the token and cookie are long
    /// pastes, and a mistyped line would have to be re-pasted whole.
    pub(crate) fn prompt_slack_register(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.open_prompt(
            "slack workspace:",
            std::rc::Rc::new(|workspace: &Workspace, needle: &str, _: &gpui::App| {
                // Completing over the registered names is what makes this
                // prompt double as "replace a rotated token".
                let needle = needle.trim().to_lowercase();
                workspace
                    .slack_workspaces()
                    .into_iter()
                    .filter(|name| name.0.to_lowercase().contains(&needle))
                    .map(|name| Candidate {
                        value: name.0,
                        description: "registered".to_owned(),
                    })
                    .collect()
            }),
            std::rc::Rc::new(|workspace: &mut Workspace, input, window, cx| {
                let name = input.trim().to_owned();
                if name.is_empty() {
                    return;
                }
                workspace.prompt_slack_token(name, window, cx);
            }),
            window,
            cx,
        );
    }

    fn prompt_slack_token(
        &mut self,
        name: String,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.open_prompt(
            format!("{name} xoxc token:"),
            std::rc::Rc::new(|_, _, _| Vec::new()),
            std::rc::Rc::new(move |workspace: &mut Workspace, input, window, cx| {
                let token = input.trim().to_owned();
                if token.is_empty() {
                    return;
                }
                workspace.prompt_slack_cookie(name.clone(), token, window, cx);
            }),
            window,
            cx,
        );
    }

    fn prompt_slack_cookie(
        &mut self,
        name: String,
        token: String,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.open_prompt(
            format!("{name} d cookie:"),
            std::rc::Rc::new(|_, _, _| Vec::new()),
            std::rc::Rc::new(move |workspace: &mut Workspace, input, _window, cx| {
                workspace.register_slack_workspace(&name, &token, &input, cx);
            }),
            window,
            cx,
        );
    }

    pub(crate) fn register_slack_workspace(
        &mut self,
        name: &str,
        token: &str,
        cookie: &str,
        cx: &mut gpui::Context<Self>,
    ) {
        let registered = Credentials::parse(name, token, cookie).and_then(|credentials| {
            let mut store = self.slack_credentials()?;
            let workspace = credentials.workspace.clone();
            store.register(credentials)?;
            Ok(workspace)
        });
        match registered {
            Ok(workspace) => {
                self.echo(
                    &format!("slack: {workspace} registered"),
                    StyleClass::SystemInfo,
                    cx,
                );
            }
            Err(error) => {
                // The message names what was wrong with the input, never the
                // input: a token in the message strip is a token on screen.
                self.notice_on(
                    None,
                    &format!("slack: {error}"),
                    StyleClass::StatusError,
                    cx,
                );
            }
        }
    }

    /// Where this rho keeps Slack's files, under the client state
    /// directory `main` resolved. rho-slack has no default of its own, so
    /// a rho that has not said where its state lives — a test — has no
    /// Slack session rather than the user's.
    pub(crate) fn slack_paths(&self) -> anyhow::Result<rho_slack::config::Paths> {
        let state_dir =
            rho_mirror::mirror::state_dir().context("the client state directory is not set")?;
        let mut paths = rho_slack::config::Paths::under(state_dir);
        // The override exists so an isolated run (QA, a second profile)
        // cannot touch the real workspaces.
        if let Some(path) = std::env::var_os("RHO_SLACK_CREDENTIALS").filter(|it| !it.is_empty()) {
            paths.credentials = path.into();
        }
        Ok(paths)
    }

    pub(crate) fn slack_credentials(&self) -> anyhow::Result<CredentialStore> {
        CredentialStore::open(self.slack_paths()?.credentials)
    }

    pub(crate) fn slack_workspaces(&self) -> Vec<WorkspaceName> {
        self.slack_credentials()
            .map(|store| store.workspaces().collect())
            .unwrap_or_default()
    }
}

/// The Slack client the desk holds, and whether it can be trusted.
///
/// One session for the whole client, started from the first registered
/// workspace and not before: a reader with no Slack never opens one. The
/// freshness flag sits beside it because it is a fact about that session
/// and nothing else — while it is set the mirror may be behind, and it
/// lights the lamp on its own, since nothing else in the queue knows.
#[derive(Default)]
pub(crate) struct Slack {
    session: Option<gpui::Entity<Session>>,
    pub(crate) list: Option<gpui::Entity<rho_slack::ui::ListView>>,
    degraded: Option<String>,
    /// Newest inbound message considered for a desktop notification per
    /// Slack unit. Focused messages are recorded too, so leaving the
    /// conversation cannot make the same arrival alert later.
    notified: std::collections::BTreeMap<Unit, Ts>,
}

impl Slack {
    /// The session if one has been started.
    pub(crate) fn session(&self) -> Option<gpui::Entity<Session>> {
        self.session.clone()
    }

    /// Whether a session has been started at all.
    pub(crate) fn started(&self) -> bool {
        self.session.is_some()
    }

    /// Keeps a session that has just been started.
    pub(crate) fn start(&mut self, session: gpui::Entity<Session>) {
        self.list = None;
        self.session = Some(session);
    }

    /// Why the session cannot be trusted to be current, while it cannot.
    pub(crate) fn degraded(&self) -> Option<&String> {
        self.degraded.as_ref()
    }

    /// The session has fallen behind, for this reason.
    pub(crate) fn fell_behind(&mut self, reason: String) {
        self.degraded = Some(reason);
    }

    /// The session has caught up.
    pub(crate) fn caught_up(&mut self) {
        self.degraded = None;
    }
}

fn source_is_unit(source: &Source, unit: &Unit) -> bool {
    source.channel() == &unit.channel
        && match (source, &unit.thread) {
            (Source::Conversation(_), None) => true,
            (Source::Thread(key), Some(root)) => key.thread_ts == *root,
            _ => false,
        }
}

/// New timestamps alert only away from the conversation. Equality and older
/// timestamps are reconnect/replay evidence and never alert again.
fn should_notify_slack(
    previous: Option<&Ts>,
    focused: Option<&Source>,
    unit: &Unit,
    newest: &Ts,
) -> bool {
    let new = previous.is_none_or(|previous| newest.is_newer_than(previous));
    new && focused.is_none_or(|source| !source_is_unit(source, unit))
}

impl Workspace {
    /// Opens the durable Slack draft inventory. Selecting one opens its
    /// source at the existing composer and enters insert mode.
    pub(crate) fn open_slack_drafts(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(session) = self.slack_session(window, cx) else {
            return;
        };
        let drafts = session
            .read(cx)
            .drafts()
            .into_iter()
            .map(|(source, draft)| {
                let label = session.read(cx).label(&source);
                let target = match &source {
                    Source::Conversation(_) => label,
                    Source::Thread(key) => format!("{label} / thread {}", key.thread_ts.0),
                };
                let snippet = draft
                    .text
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .take(80)
                    .collect::<String>();
                let description = match (snippet.is_empty(), draft.files.len()) {
                    (false, 0) => snippet,
                    (false, count) => format!("{snippet} · {count} file(s)"),
                    (true, count) => format!("{count} file(s)"),
                };
                (target, description, source)
            })
            .collect::<Vec<_>>();
        if drafts.is_empty() {
            self.echo("slack: no drafts", StyleClass::SystemInfo, cx);
            return;
        }
        let complete = drafts.clone();
        let select = drafts;
        self.open_prompt(
            "slack drafts:",
            std::rc::Rc::new(move |_, needle, _| {
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
            std::rc::Rc::new(move |workspace: &mut Workspace, input, window, cx| {
                let chosen = match input.trim() {
                    "" => select.first(),
                    input => select.iter().find(|(label, _, _)| label == input),
                };
                let Some((_, _, source)) = chosen else {
                    return;
                };
                workspace.open_slack_source(source.clone(), window, cx);
                if let SurfaceView::SlackConversation(view) = &workspace.active_surface().view {
                    view.clone()
                        .update(cx, |view, cx| view.select_compose(window, cx));
                }
                workspace.enter_insert_when_shown(window, cx);
            }),
            window,
            cx,
        );
        self.set_prompt_complete_whole_input();
    }

    pub(crate) fn slack_list_view(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> Option<gpui::Entity<rho_slack::ui::ListView>> {
        if let Some(list) = &self.slack.list {
            return Some(list.clone());
        }
        let session = self.slack_session(window, cx)?;
        let hooks = Self::slack_hooks();
        let list = cx.new(|cx| rho_slack::ui::ListView::new(session, hooks, window, cx));
        self.slack.list = Some(list.clone());
        Some(list)
    }

    /// Opens the conversation list, starting the session on first entry.
    /// This is the way in: everything else is reached from a row.
    pub(crate) fn open_slack(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        if self.slack_session(window, cx).is_none() {
            self.notice_on(
                None,
                "slack: no workspace registered",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        self.active_context = ContextId::Slack;
        let surface = self.make_surface(SurfaceKey::SlackList, window, cx);
        self.show_slack_surface(surface, cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    /// Opens every mention, DM, and followed thread known to the mirror,
    /// including read items; this is navigation, not the dealer queue.
    pub(crate) fn open_slack_activity(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(session) = self.slack_session(window, cx) else {
            return;
        };
        let rows = session.read(cx).activity();
        self.open_slack_inventory(SlackInventoryKind::Activity, rows, session, window, cx);
    }

    /// Opens messages explicitly saved in rho's local Later inventory.
    pub(crate) fn open_slack_saved(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(session) = self.slack_session(window, cx) else {
            return;
        };
        let rows = session.read(cx).saved();
        self.open_slack_inventory(SlackInventoryKind::Saved, rows, session, window, cx);
    }

    /// Saves the message under the cursor for the local Later inventory.
    pub(crate) fn slack_save_for_later(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return;
        };
        let view = view.clone();
        let source = view.read(cx).source().clone();
        let Some(message) = view.update(cx, |view, cx| view.cursor_message(cx)) else {
            return;
        };
        if let Some(session) = self.slack.session() {
            let saved = session.read(cx).is_saved_for_later(&source, &message.ts);
            if saved {
                session.read(cx).remove_from_later(&source, &message.ts);
                self.echo("slack: removed from saved", StyleClass::SystemInfo, cx);
            } else {
                session.read(cx).save_for_later(&source, &message);
                self.echo("slack: saved for later", StyleClass::SystemInfo, cx);
            }
            self.refresh_slack_inventories(window, cx);
        }
    }

    /// Marks the message under the cursor and everything after it unread in
    /// Slack, while making the same cursor move in rho immediately.
    pub(crate) fn slack_mark_unread(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return;
        };
        let view = view.clone();
        let source = view.read(cx).source().clone();
        let Some(message) = view.update(cx, |view, cx| view.cursor_message(cx)) else {
            return;
        };
        if let Some(session) = self.slack.session() {
            session.update(cx, |session, cx| {
                session.mark_unread_from(&source, &message.ts, cx)
            });
            self.echo("slack: marked unread", StyleClass::SystemInfo, cx);
            self.refresh_slack_inventories(window, cx);
        }
    }

    fn open_slack_inventory(
        &mut self,
        kind: SlackInventoryKind,
        rows: Vec<rho_slack::session::ActivityEntry>,
        session: gpui::Entity<Session>,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.active_context = ContextId::Slack;
        let title = kind.title();
        let key = SurfaceKey::SlackInventory(kind);
        let surface = match self.find_surface(|surface| surface.key == key).cloned() {
            Some(surface) => surface,
            None => {
                let hooks = Self::slack_hooks();
                let view = cx.new(|cx| rho_slack::ui::ResultsView::new(session, hooks, window, cx));
                self._slack_view_subscriptions.push(cx.subscribe_in(
                    &view,
                    window,
                    |workspace, _, event: &rho_slack::ui::results::Event, window, cx| {
                        if let rho_slack::ui::results::Event::Open(place) = event {
                            workspace.open_slack_search_target(
                                rho_slack::ui::Target::Message(place.clone()),
                                window,
                                cx,
                            );
                        }
                    },
                ));
                Self::wrap_surface(key, SurfaceView::SlackResults(view))
            }
        };
        let SurfaceView::SlackResults(view) = &surface.view else {
            return;
        };
        view.clone()
            .update(cx, |view, cx| view.inventory(title, &rows, window, cx));
        self.show_slack_surface(surface, cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    fn refresh_slack_inventories(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(session) = self.slack.session() else {
            return;
        };
        for kind in [SlackInventoryKind::Activity, SlackInventoryKind::Saved] {
            let view = self
                .find_surface(|surface| surface.key == SurfaceKey::SlackInventory(kind))
                .and_then(|surface| match &surface.view {
                    SurfaceView::SlackResults(view) => Some(view.clone()),
                    _ => None,
                });
            let Some(view) = view else { continue };
            let rows = match kind {
                SlackInventoryKind::Activity => session.read(cx).activity(),
                SlackInventoryKind::Saved => session.read(cx).saved(),
            };
            view.update(cx, |view, cx| {
                view.inventory(kind.title(), &rows, window, cx)
            });
        }
    }

    /// The live session, started from the first registered workspace. A
    /// session is per workspace; the prompt registers several, and the first
    /// is the one rho lives in.
    pub(crate) fn slack_session(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> Option<gpui::Entity<Session>> {
        if let Some(session) = self.slack.session() {
            return Some(session);
        }
        let store = self.slack_credentials().ok()?;
        let name = store.workspaces().next()?;
        let credentials = store.get(&name)?.clone();
        let paths = self.slack_paths().ok()?;
        let session = cx.new(|cx| Session::new(credentials, paths, cx));
        // Window-scoped: a thread ignored in another client closes its card
        // here, and closing a card writes to the tree.
        self._slack_subscription =
            Some(
                cx.subscribe_in(&session, window, |workspace, session, event, window, cx| {
                    workspace.on_slack_event(session.clone(), event, window, cx);
                }),
            );
        self._slack_observer = Some(cx.observe_in(&session, window, |workspace, _, window, cx| {
            workspace.refresh_slack_inventories(window, cx)
        }));
        self.slack.start(session.clone());
        Some(session)
    }

    /// The chrome above the Slack listing, for a test that asserts what the
    /// reader is told about the state the list is in.
    #[cfg(test)]
    pub(crate) fn slack_banner_for_test(&self, cx: &gpui::App) -> Vec<String> {
        let SurfaceView::SlackList(view) = &self.active_surface().view else {
            return Vec::new();
        };
        view.read(cx).drawn_banner_for_test()
    }

    /// Puts the point on a row of the Slack list, for a test that then
    /// moves the rows under it.
    #[cfg(test)]
    pub(crate) fn slack_place_cursor_for_test(
        &mut self,
        row: usize,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let SurfaceView::SlackList(view) = &self.active_surface().view else {
            return;
        };
        view.clone()
            .update(cx, |view, cx| view.place_cursor_for_test(row, window, cx));
    }

    /// The conversation the point is on in the Slack list, named the way
    /// the rows are, for a test that asserts what `enter` would open.
    #[cfg(test)]
    pub(crate) fn slack_cursor_conversation_for_test(
        &mut self,
        cx: &mut gpui::Context<Self>,
    ) -> Option<String> {
        let SurfaceView::SlackList(view) = &self.active_surface().view else {
            return None;
        };
        let source = view.clone().update(cx, |view, cx| view.cursor_source(cx))?;
        let session = self.slack.session()?;
        Some(session.read(cx).model().label(source.channel()))
    }

    /// The conversation names the Slack list is drawing, for a test that
    /// asserts what the reader is looking at rather than what the model
    /// holds.
    #[cfg(test)]
    pub(crate) fn slack_rows_for_test(&self, cx: &gpui::App) -> Vec<String> {
        let SurfaceView::SlackList(view) = &self.active_surface().view else {
            return Vec::new();
        };
        view.read(cx).drawn_conversations_for_test(cx)
    }

    /// The conversation on screen, named the way the list names it, for a
    /// test that asserts where a key took the reader.
    #[cfg(test)]
    pub(crate) fn slack_open_label_for_test(&self, cx: &gpui::App) -> Option<String> {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return None;
        };
        let source = view.read(cx).source().clone();
        Some(self.slack.session()?.read(cx).label(&source))
    }

    /// The lines of the search results as the reader reads them, for a
    /// test that asserts what is on screen rather than what came back.
    #[cfg(test)]
    pub(crate) fn slack_results_for_test(&self, cx: &gpui::App) -> Vec<String> {
        let SurfaceView::SlackResults(view) = &self.active_surface().view else {
            return Vec::new();
        };
        view.read(cx).drawn_lines_for_test()
    }

    /// The transcript on screen, for a test that asserts where a hit landed
    /// the reader.
    #[cfg(test)]
    pub(crate) fn slack_transcript_for_test(&self, cx: &gpui::App) -> Vec<String> {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return Vec::new();
        };
        view.read(cx).drawn_lines_for_test(cx)
    }

    #[cfg(test)]
    pub(crate) fn slack_compose_text_for_test(&self, cx: &gpui::App) -> Option<String> {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return None;
        };
        Some(view.read(cx).compose_text_for_test(cx))
    }

    /// What the list's last redraw cost, and how many lines it drew.
    ///
    /// Found across the open surfaces rather than on the active one: the
    /// cost that matters is what a message costs the list while the reader
    /// is in a conversation, which is when both surfaces redraw.
    #[cfg(test)]
    pub(crate) fn slack_list_cost_for_test(
        &self,
        cx: &gpui::App,
    ) -> Option<(std::time::Duration, usize)> {
        self.open_surfaces_for_test().find_map(|surface| {
            let SurfaceView::SlackList(view) = &surface.view else {
                return None;
            };
            let view = view.read(cx);
            Some((
                view.last_refresh_for_test(),
                view.drawn_line_count_for_test(),
            ))
        })
    }

    /// The same for the open conversation.
    #[cfg(test)]
    pub(crate) fn slack_conversation_cost_for_test(
        &self,
        cx: &gpui::App,
    ) -> Option<(std::time::Duration, usize)> {
        self.open_surfaces_for_test().find_map(|surface| {
            let SurfaceView::SlackConversation(view) = &surface.view else {
                return None;
            };
            let view = view.read(cx);
            Some((
                view.last_refresh_for_test(),
                view.drawn_row_count_for_test(),
            ))
        })
    }

    /// A session built elsewhere, for a test that wants Slack surfaces over
    /// a fake server rather than over the user's workspace. The one seam:
    /// everything after it — opening the list, narrowing it, escaping —
    /// runs the code the reader runs.
    #[cfg(test)]
    pub(crate) fn install_slack_session_for_test(
        &mut self,
        session: gpui::Entity<Session>,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self._slack_subscription =
            Some(
                cx.subscribe_in(&session, window, |workspace, session, event, window, cx| {
                    workspace.on_slack_event(session.clone(), event, window, cx);
                }),
            );
        self._slack_observer = Some(cx.observe_in(&session, window, |workspace, _, window, cx| {
            workspace.refresh_slack_inventories(window, cx)
        }));
        self.slack.start(session);
        // The session is the other half of the seed. Whichever half arrives
        // second runs it; the marker in the mirror is what makes it once.
        self.seed_slack_cursors(cx);
    }

    /// The host services the Slack surfaces borrow: editor chrome and the
    /// transcript's Markdown pipeline, so chat reads like every other
    /// buffer in the frame.
    pub(crate) fn slack_hooks() -> rho_slack::ui::Hooks {
        rho_slack::ui::Hooks {
            configure_editor: |editor, window, cx| {
                rho_window::editor_config::configure(editor, window, cx);
                editor.set_mouse_click_selection_enabled(true, cx);
            },
            configure_markdown: rho_window::markdown::configure_buffer,
            gutter_colour: rho_window::style::user_prompt_gutter_color,
            prompt_style: |cx| StyleClass::UserMessage.resolve(cx),
        }
    }

    /// Moves rho's own half of a unit's cursor and says where it stood, so
    /// an undo can put it back. `at` is where to put it, defaulting to the
    /// unit's newest message: `mark read before` names a cutoff, and every
    /// other verdict means all of it.
    ///
    /// The move is local and immediate. Telling Slack is the session's
    /// outbox, which is why nothing here waits for anything.
    pub(crate) fn advance_slack_cursor(
        &mut self,
        unit: &SlackUnit,
        at: Option<Ts>,
        cx: &mut gpui::Context<Self>,
    ) -> Option<(Unit, HandledBefore)> {
        let session = self.slack.session()?;
        let unit = model_unit(unit);
        session.update(cx, |session, cx| {
            let at = at.or_else(|| {
                session
                    .model()
                    .unit(&unit)
                    .map(|facts| facts.newest.clone())
            })?;
            let before = session.handled_before(&unit);
            session.mark_handled(&unit, &at, cx);
            Some((unit.clone(), before))
        })
    }

    /// `shift-u`: the cursors a verdict moved, back where it found them.
    pub(crate) fn restore_slack_cursors(
        &mut self,
        cursors: &[(Unit, HandledBefore)],
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(session) = self.slack.session() else {
            return;
        };
        session.update(cx, |session, cx| {
            for (unit, before) in cursors {
                session.undo_handled(unit, before, cx);
            }
        });
    }

    /// The one-time seed: what the store's `handled_through` cells said
    /// becomes rho's local cursor, once, at the first start that has both
    /// the session and the cells. The cells are not read again and nothing
    /// deletes them -- they are the record of verdicts made before the
    /// cursor lived in the mirror.
    ///
    /// One pass over the store's facts, once per workspace ever, so the
    /// per-event cost rule is untouched.
    pub(crate) fn seed_slack_cursors(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(session) = self.slack.session() else {
            return;
        };
        if session.read(cx).handled_seeded() {
            return;
        }
        let cells = self.desk_cells.slack_handled_cells();
        session.update(cx, |session, _| {
            for (unit, ts) in cells {
                session.seed_handled(&model_unit(&unit), &Ts(ts.0));
            }
            session.set_handled_seeded();
        });
    }

    /// Opens the conversation a Slack card is about and puts the reader on
    /// the oldest message from someone else they have not handled: three
    /// mentions in a channel are one card, and the reader starts at the
    /// first of them rather than the last.
    pub(crate) fn open_slack_deal(
        &mut self,
        unit: &SlackUnit,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let Some(session) = self.slack_session(window, cx) else {
            return false;
        };
        let unit_of = model_unit(unit);
        // rho's half of the cursor. Slack's mark is the other half and the
        // conversation view lands on it in its own right, so the oldest
        // message this reader has not dealt with is what is asked for here.
        let cursor = session.read(cx).model().handled_through(&unit_of).cloned();
        let land = session
            .read(cx)
            .oldest_from_other_after(&unit_of, cursor.as_ref());
        self.open_slack_source(unit_source(unit), window, cx);
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return false;
        };
        if let Some(land) = land {
            view.clone()
                .update(cx, |view, cx| view.reveal(land, window, cx));
        }
        true
    }

    /// Shows one conversation: a channel, a group, a DM, or a thread. A
    /// thread opened from a channel is a child surface, so `ctrl-k` returns
    /// to the channel it came from.
    pub(crate) fn open_slack_source(
        &mut self,
        source: Source,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(session) = self.slack_session(window, cx) else {
            return;
        };
        if let Some(list) = self.slack_list_view(window, cx) {
            list.update(cx, |list, cx| {
                list.select_channel(source.channel(), window, cx)
            });
        }
        self.active_context = ContextId::Slack;
        // Opening a muted unit no longer takes the mute back (8 Sep): the
        // mute is Slack's, and rho unmuting a conversation because the
        // reader looked at it would be rho changing something of theirs
        // that every other client reads. `shift-u` on the verdict is the
        // way back, and so is unmuting in Slack.
        let key = SurfaceKey::SlackConversation(source.clone());
        self.slack_labels
            .insert(source.clone(), session.read(cx).label(&source));
        let surface = match self.find_surface(|surface| surface.key == key).cloned() {
            Some(surface) => surface,
            None => {
                let hooks = Self::slack_hooks();
                let view = cx.new(|cx| {
                    rho_slack::ui::ConversationView::new(session, source, hooks, window, cx)
                });
                self._slack_view_subscriptions.push(cx.subscribe_in(
                    &view,
                    window,
                    |workspace, view, event, window, cx| match event {
                        rho_slack::ui::conversation::Event::OpenFile(file) => {
                            if file.is_image() {
                                workspace.open_slack_image(view.clone(), file.clone(), cx);
                            } else {
                                view.update(cx, |view, cx| view.open_file(file.clone(), cx));
                            }
                        }
                        rho_slack::ui::conversation::Event::AttachRefused => {
                            workspace.echo(NOT_WHILE_EDITING, StyleClass::SystemInfo, cx);
                        }
                        rho_slack::ui::conversation::Event::RewriteLost => {
                            workspace.echo(REWRITE_LOST, StyleClass::SystemInfo, cx);
                        }
                        rho_slack::ui::conversation::Event::AttachFailed(said) => {
                            workspace.echo(
                                &format!("slack: {said}"),
                                StyleClass::SystemImportant,
                                cx,
                            );
                        }
                        rho_slack::ui::conversation::Event::BroadcastWithFilesUnsupported => {
                            workspace.echo(
                                "slack: files cannot also be sent to the channel",
                                StyleClass::SystemImportant,
                                cx,
                            );
                        }
                        rho_slack::ui::conversation::Event::ActivateRequested => {
                            workspace.slack_open_row(window, cx);
                        }
                    },
                ));
                Self::wrap_surface(key, SurfaceView::SlackConversation(view))
            }
        };
        self.show_slack_surface(surface, cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    /// Slack surfaces enter the back history when they are opened, so that
    /// `ctrl-k` out of a thread lands on the channel it was opened from
    /// rather than on whatever was on screen before Slack.
    fn show_slack_surface(
        &mut self,
        surface: crate::workspace::Surface,
        cx: &mut gpui::Context<Self>,
    ) {
        self.display_surface_with_method(surface, rho_journal::SurfaceShowMethod::Command, cx);
    }

    /// `shift-n`: the next conversation with something unread, or the list
    /// when there is none. Reading through what came in is one key, and it
    /// ends somewhere that says so rather than on a conversation the
    /// reader has already read.
    pub(crate) fn slack_next_unread(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(session) = self.slack_session(window, cx) else {
            return;
        };
        let here = match &self.active_surface().view {
            SurfaceView::SlackConversation(view) => Some(view.read(cx).source().channel().clone()),
            _ => None,
        };
        match session.read(cx).model().next_unread(here.as_ref()) {
            NextUnread::Go(channel) => {
                self.open_slack_source(Source::Conversation(channel), window, cx)
            }
            // The reader has read everything the narrowing reaches. The key
            // stops here rather than jumping out of the list they are in,
            // and says what it is not showing them: nothing moves, and
            // nothing is quietly left out.
            NextUnread::Outside(waiting) => self.echo(
                &Self::outside_the_narrowing(waiting),
                StyleClass::SystemInfo,
                cx,
            ),
            NextUnread::Nothing => self.open_slack(window, cx),
        }
    }

    /// What the reader is told when the next-unread key reaches the edge of
    /// a narrowing with unread still outside it. Its own function so the
    /// singular reads like English and both cases can be asserted without a
    /// window.
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

    /// `enter` on a list row, or on a message: the row's conversation, or
    /// the thread the message is in.
    pub(crate) fn slack_open_row(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        // An app control is more specific than the thread around its message.
        if let SurfaceView::SlackConversation(view) = &self.active_surface().view {
            let view = view.clone();
            if let Some(interaction) = view.update(cx, |view, cx| view.cursor_interaction(cx)) {
                if let Some(confirmation) = interaction.action.confirmation.clone() {
                    self.prompt_slack_confirmation(view, interaction, confirmation, window, cx);
                    return;
                }
                let element_type = interaction.action.element_type.clone();
                match element_type.as_str() {
                    "button" => {
                        if let Some(url) = interaction.action.payload["url"].as_str() {
                            self.create_browser_page(url.to_owned(), None, window, cx);
                        } else {
                            view.update(cx, |view, cx| view.run_interaction(interaction, None, cx));
                        }
                    }
                    "static_select" | "overflow" => {
                        self.prompt_slack_select(view, interaction, window, cx);
                    }
                    kind @ ("users_select" | "channels_select" | "conversations_select") => {
                        let mut interaction = interaction;
                        interaction.action.options = view.read(cx).interaction_options(kind, cx);
                        self.prompt_slack_select(view, interaction, window, cx);
                    }
                    "datepicker" => self.prompt_slack_date(view, interaction, window, cx),
                    "external_select" => self.prompt_slack_external(view, interaction, window, cx),
                    kind => self.echo(
                        &format!("slack: {kind} app controls are not supported yet"),
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
                return;
            }
        }
        // A file line is a file: the reader who put the cursor there asked
        // for the attachment, not for the thread it hangs under.
        if let SurfaceView::SlackConversation(view) = &self.active_surface().view {
            let view = view.clone();
            let file = view.update(cx, |view, cx| view.cursor_file(cx));
            if let Some(file) = file {
                if file.is_image() {
                    self.open_slack_image(view, file, cx);
                } else {
                    view.update(cx, |view, cx| view.open_file(file, cx));
                }
                return;
            }
        }
        // A link's label shows no address, so the line carries the URL: the
        // reader on it asked for the page, not for the thread around it.
        if let SurfaceView::SlackConversation(view) = &self.active_surface().view {
            let view = view.clone();
            let link = view.update(cx, |view, cx| view.cursor_link(cx));
            if let Some(link) = link {
                self.create_browser_page(link, None, window, cx);
                return;
            }
        }
        let source = match &self.active_surface().view {
            SurfaceView::SlackList(view) => {
                view.clone().update(cx, |view, cx| view.cursor_source(cx))
            }
            SurfaceView::SlackConversation(view) => view
                .clone()
                .update(cx, |view, cx| view.cursor_thread(cx))
                .map(Source::Thread),
            _ => None,
        };
        if let Some(source) = source {
            self.open_slack_source(source, window, cx);
        }
    }

    fn prompt_slack_confirmation(
        &mut self,
        view: gpui::Entity<rho_slack::ui::ConversationView>,
        interaction: rho_slack::ui::conversation::MessageInteraction,
        confirmation: rho_slack::block::Confirmation,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let confirm = confirmation.confirm.clone();
        let deny = confirmation.deny.clone();
        let choices = vec![
            Candidate {
                value: confirm.clone(),
                description: "confirm".to_owned(),
            },
            Candidate {
                value: deny,
                description: "cancel".to_owned(),
            },
        ];
        self.open_prompt(
            format!("{} — {}:", confirmation.title, confirmation.text),
            std::rc::Rc::new(move |_, needle, _| {
                let needle = needle.to_lowercase();
                choices
                    .iter()
                    .filter(|choice| choice.value.to_lowercase().contains(&needle))
                    .cloned()
                    .collect()
            }),
            std::rc::Rc::new(move |_, input, _window, cx| {
                if input == confirm {
                    view.update(cx, |view, cx| {
                        view.run_interaction(interaction.clone(), None, cx)
                    });
                }
            }),
            window,
            cx,
        );
        if let Some(minibuffer) = &mut self.minibuffer {
            minibuffer.set_complete_whole_input();
        }
    }

    fn prompt_slack_external(
        &mut self,
        view: gpui::Entity<rho_slack::ui::ConversationView>,
        interaction: rho_slack::ui::conversation::MessageInteraction,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let minimum = interaction.action.payload["min_query_length"]
            .as_u64()
            .unwrap_or(0) as usize;
        let label = interaction.action.label.clone();
        self.open_prompt(
            format!("{label} query:"),
            std::rc::Rc::new(|_, _, _| Vec::new()),
            std::rc::Rc::new(move |workspace, input, _window, cx| {
                if input.chars().count() < minimum {
                    workspace.echo(
                        &format!("slack: enter at least {minimum} characters"),
                        StyleClass::SystemInfo,
                        cx,
                    );
                    return;
                }
                view.update(cx, |view, cx| {
                    view.fetch_external_options(interaction.clone(), input, cx)
                });
            }),
            window,
            cx,
        );
    }

    fn prompt_slack_date(
        &mut self,
        view: gpui::Entity<rho_slack::ui::ConversationView>,
        interaction: rho_slack::ui::conversation::MessageInteraction,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let label = interaction.action.label.clone();
        self.open_prompt(
            format!("{label} (YYYY-MM-DD):"),
            std::rc::Rc::new(|_, _, _| Vec::new()),
            std::rc::Rc::new(move |workspace, input, _window, cx| {
                if chrono::NaiveDate::parse_from_str(&input, "%Y-%m-%d").is_err() {
                    workspace.echo(
                        "slack: enter a date as YYYY-MM-DD",
                        StyleClass::SystemInfo,
                        cx,
                    );
                    return;
                }
                let option = rho_slack::block::InteractionOption {
                    label: input.clone(),
                    value: input,
                };
                view.update(cx, |view, cx| {
                    view.run_interaction(interaction.clone(), Some(option), cx)
                });
            }),
            window,
            cx,
        );
    }

    fn prompt_slack_select(
        &mut self,
        view: gpui::Entity<rho_slack::ui::ConversationView>,
        interaction: rho_slack::ui::conversation::MessageInteraction,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let options = interaction.action.options.clone();
        let choices = options.clone();
        self.open_prompt(
            format!("{}:", interaction.action.label),
            std::rc::Rc::new(move |_, needle, _| {
                let needle = needle.to_lowercase();
                choices
                    .iter()
                    .filter(|option| option.label.to_lowercase().contains(&needle))
                    .map(|option| Candidate {
                        value: option.label.clone(),
                        description: option.value.clone(),
                    })
                    .collect()
            }),
            std::rc::Rc::new(move |_, input, _window, cx| {
                let Some(option) = options.iter().find(|option| option.label == input).cloned()
                else {
                    return;
                };
                view.update(cx, |view, cx| {
                    view.run_interaction(interaction.clone(), Some(option), cx)
                });
            }),
            window,
            cx,
        );
        if let Some(minibuffer) = &mut self.minibuffer {
            minibuffer.set_complete_whole_input();
        }
    }

    /// A picture opens in rho, not in the desktop's viewer: the bytes are
    /// already cached for the thumbnail, so this is usually instant.
    fn open_slack_image(
        &mut self,
        view: gpui::Entity<rho_slack::ui::ConversationView>,
        file: rho_slack::types::FileSummary,
        cx: &mut gpui::Context<Self>,
    ) {
        let path = view.update(cx, |view, cx| view.file_path(&file, cx));
        let title = file.title.clone();
        cx.spawn(async move |this, cx| {
            let path = path.await;
            let _ = this.update_in(cx, |this, window, cx| match path {
                Ok(path) => match camino::Utf8PathBuf::from_path_buf(path) {
                    Ok(path) => this.open_image(path, title, window, cx),
                    Err(path) => {
                        tracing::warn!(path = %path.display(), "slack image path is not utf-8");
                    }
                },
                Err(error) => {
                    tracing::warn!(error = %error, "slack image fetch failed");
                    this.notice_on(
                        None,
                        &format!("slack: {error:#}"),
                        rho_window::style::StyleClass::SystemInfo,
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    fn open_slack_file_image(
        &mut self,
        file: rho_slack::types::FileSummary,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(session) = self.slack.session() else {
            return;
        };
        let path = session.update(cx, |session, cx| session.file_path(&file, cx));
        let title = file.title.clone();
        cx.spawn(async move |this, cx| {
            let path = path.await;
            let _ = this.update_in(cx, |this, window, cx| match path {
                Ok(path) => match camino::Utf8PathBuf::from_path_buf(path) {
                    Ok(path) => this.open_image(path, title, window, cx),
                    Err(path) => {
                        tracing::warn!(path = %path.display(), "slack image path is not utf-8");
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, "slack file search image fetch failed");
                    this.notice_on(
                        None,
                        &format!("slack: {error:#}"),
                        rho_window::style::StyleClass::SystemInfo,
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    /// Shows a cached picture full-window. Opened from a conversation, so
    /// `ctrl-k` walks back to it and `q` closes.
    pub(crate) fn open_image(
        &mut self,
        path: camino::Utf8PathBuf,
        title: String,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let key = SurfaceKey::Image {
            path: path.clone(),
            title,
        };
        let surface = match self.find_surface(|surface| surface.key == key).cloned() {
            Some(surface) => surface,
            None => {
                let view = cx.new(|cx| rho_window::image_view::ImageView::new(path, cx));
                crate::workspace::Workspace::wrap_surface(key, SurfaceView::Image(view))
            }
        };
        self.show_slack_surface(surface, cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    /// `i`: into the composer, which is where a reply is written.
    pub(crate) fn slack_compose(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if let SurfaceView::SlackConversation(view) = &self.active_surface().view {
            view.clone()
                .update(cx, |view, cx| view.select_compose(window, cx));
            // The editor is shown on the next frame. Enter insert there so
            // a mouse click followed by typing cannot lose its first keys.
            self.enter_insert_when_shown(window, cx);
        }
    }

    /// Messages that landed at the end of the conversation on screen while
    /// the reader was further up. `None` when there is nothing to say,
    /// which is every surface that is not a Slack conversation.
    pub(crate) fn slack_unseen(&self, cx: &gpui::App) -> Option<usize> {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return None;
        };
        Some(view.read(cx).unseen()).filter(|unseen| *unseen > 0)
    }

    /// `enter` in the composer: send, or post the rewrite if an edit is
    /// open.
    ///
    /// What goes in the journal is what Slack accepted, never what was
    /// pressed. A rewrite the server refuses puts the reader's words back
    /// and says so, and the record has to agree with the screen: before
    /// this it was written the moment enter was pressed, so a refused
    /// rewrite and an upload that failed both left a record of something
    /// that did not happen.
    pub(crate) fn slack_submit(&mut self, cx: &mut gpui::Context<Self>) {
        let broadcast = match &self.active_surface().view {
            SurfaceView::SlackConversation(view) => view.read(cx).also_send_to_channel(),
            _ => false,
        };
        self.slack_submit_with_options(broadcast, cx);
    }

    pub(crate) fn slack_submit_with_options(
        &mut self,
        also_send_to_channel: bool,
        cx: &mut gpui::Context<Self>,
    ) {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return;
        };
        let view = view.clone();
        let channel = view.read(cx).source().channel().clone();
        let submitting = view.update(cx, |view, cx| {
            view.submit_with_options(also_send_to_channel, cx)
        });
        cx.spawn(async move |this, cx| {
            let submitted = submitting.await;
            let _ = this.update(cx, |this, cx| {
                this.record_submitted(&channel, submitted, cx)
            });
        })
        .detach();
    }

    /// Writes down what a press of enter turned out to be. Nothing is
    /// recorded for a send, which the session already journals from its own
    /// confirmed path, or for a refusal, which is not something the reader
    /// did.
    fn record_submitted(
        &mut self,
        channel: &ChannelId,
        submitted: rho_slack::ui::conversation::Submitted,
        cx: &gpui::Context<Self>,
    ) {
        use rho_slack::ui::conversation::Submitted;

        let Some(session) = self.slack.session() else {
            return;
        };
        let conversation = session.read(cx).model().label(channel);
        match submitted {
            Submitted::Edited(ts) => rho_journal::record(rho_journal::Event::SlackMessageEdited {
                conversation,
                ts: ts.0,
            }),
            Submitted::FileSent(bytes) => rho_journal::record(rho_journal::Event::SlackFileSent {
                conversation,
                bytes,
            }),
            Submitted::Sent | Submitted::Refused | Submitted::Nothing | Submitted::Sending => {}
        }
    }

    /// Attaches a picture to the conversation's next message: clipboard
    /// bytes (`ctrl-v`) or a path (a drop, or the attach prompt). Returns
    /// whether the surface took it, so the paste path can fall back to
    /// pasting text where there is no conversation.
    pub(crate) fn slack_attach_bytes(
        &mut self,
        name: String,
        bytes: Vec<u8>,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return false;
        };
        let size = bytes.len() as u64;
        let outcome = view
            .clone()
            .update(cx, |view, cx| view.attach(name.clone(), bytes, cx));
        let said = match outcome {
            Attaching::Replaced => format!("slack: {name} attached, replacing the last one"),
            Attaching::Attached => format!("slack: {name} attached · {}", human_size(size)),
            Attaching::NotWhileEditing => NOT_WHILE_EDITING.to_owned(),
            Attaching::Sending => "slack: wait for the current send to finish".to_owned(),
            Attaching::TooMany => format!(
                "slack: at most {} files can be attached",
                rho_slack::ui::conversation::MAX_ATTACHMENTS
            ),
            Attaching::TooLarge => "slack: attachment limit exceeded".to_owned(),
        };
        self.echo(&said, StyleClass::SystemInfo, cx);
        // Taken either way: a refusal that fell back to pasting the picture's
        // bytes into the composer as text would be worse than the refusal.
        true
    }

    pub(crate) fn slack_attach_path(
        &mut self,
        path: &std::path::Path,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return false;
        };
        match view
            .clone()
            .update(cx, |view, cx| view.attach_path(path, cx))
        {
            Ok(outcome) => {
                let said = match outcome {
                    Attaching::Replaced => {
                        format!("slack: {} attached, replacing the last one", path.display())
                    }
                    Attaching::Attached => format!("slack: {} attached", path.display()),
                    Attaching::NotWhileEditing => NOT_WHILE_EDITING.to_owned(),
                    Attaching::Sending => "slack: wait for the current send to finish".to_owned(),
                    Attaching::TooMany => format!(
                        "slack: at most {} files can be attached",
                        rho_slack::ui::conversation::MAX_ATTACHMENTS
                    ),
                    Attaching::TooLarge => "slack: attachment limit exceeded".to_owned(),
                };
                self.echo(&said, StyleClass::SystemInfo, cx);
            }
            // A path that cannot be read is worth saying out loud now
            // rather than at send time.
            Err(error) => self.echo(
                &format!("slack: {error:#}"),
                StyleClass::SystemImportant,
                cx,
            ),
        }
        true
    }

    /// Drops the waiting picture without sending it.
    pub(crate) fn slack_clear_attachment(&mut self, cx: &mut gpui::Context<Self>) -> bool {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return false;
        };
        let cleared = view
            .clone()
            .update(cx, |view, cx| view.clear_attachment(cx));
        if cleared {
            self.echo("slack: attachment cleared", StyleClass::SystemInfo, cx);
        }
        cleared
    }

    /// The attach prompt: the keyboard's way to the same thing a drop does.
    /// `r` on a message: the reaction menu over it.
    ///
    /// Answers false when the point is not on a message, so the key goes
    /// back to the editor rather than being swallowed on a blank line.
    pub(crate) fn slack_open_react_menu(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return false;
        };
        let view = view.clone();
        let Some(choices) = view.update(cx, |view, cx| view.reaction_choices(cx)) else {
            return false;
        };
        self.slack_reacting = Some(choices.ts.clone());
        self.open_menu(crate::transient::slack_react_menu(&choices), window, cx);
        true
    }

    /// One key from that menu: the emoji goes on, or the reader's own comes
    /// off if it was already there.
    pub(crate) fn slack_react(
        &mut self,
        name: &str,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(ts) = self.slack_reacting.clone() else {
            return;
        };
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return;
        };
        view.clone()
            .update(cx, |view, cx| view.react(&ts, name, cx));
    }

    /// `/` from the reaction menu: any emoji, by name.
    ///
    /// The same table the composer completes `:` from, so what the reader
    /// can type in a message they can react with, and the name they see in
    /// the menu afterwards is the name they typed.
    pub(crate) fn prompt_slack_react(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.slack_reacting.is_none() {
            return;
        }
        self.open_prompt(
            "react:",
            std::rc::Rc::new(|workspace: &Workspace, typed: &str, cx: &gpui::App| {
                let SurfaceView::SlackConversation(view) = &workspace.active_surface().view else {
                    return Vec::new();
                };
                let view = view.read(cx);
                let channel = view.source().channel().clone();
                view.session()
                    .read(cx)
                    .model()
                    .suggestions(&channel, ':', typed.trim().trim_matches(':'))
                    .into_iter()
                    .map(|found| Candidate {
                        value: found.value.trim_matches(':').to_owned(),
                        description: found.detail,
                    })
                    .collect()
            }),
            std::rc::Rc::new(|workspace: &mut Workspace, input, window, cx| {
                let name = input.trim().trim_matches(':').to_owned();
                if name.is_empty() {
                    return;
                }
                workspace.slack_react(&name, window, cx);
            }),
            window,
            cx,
        );
    }

    pub(crate) fn prompt_slack_attach(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.open_prompt(
            "attach file:",
            std::rc::Rc::new(|_: &Workspace, needle: &str, _: &gpui::App| {
                let path = std::path::Path::new(needle.trim());
                let description = match path.is_file() {
                    true => "enter attaches it".to_owned(),
                    false => "a path to a file".to_owned(),
                };
                vec![Candidate {
                    value: needle.trim().to_owned(),
                    description,
                }]
            }),
            std::rc::Rc::new(|workspace: &mut Workspace, input, _window, cx| {
                let path = std::path::PathBuf::from(input.trim());
                if !workspace.slack_attach_path(&path, cx) {
                    workspace.echo(
                        "slack: open a conversation first",
                        StyleClass::SystemInfo,
                        cx,
                    );
                }
            }),
            window,
            cx,
        );
    }

    pub(crate) fn prompt_slack_message_actions(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return false;
        };
        let Some(actions) = view.clone().update(cx, |view, cx| view.message_actions(cx)) else {
            return false;
        };
        self.open_menu(crate::transient::slack_message_menu(&actions), window, cx);
        true
    }

    pub(crate) fn slack_edit_message_at(
        &mut self,
        ts: Ts,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return;
        };
        if matches!(
            view.clone()
                .update(cx, |view, cx| view.start_edit_at(&ts, window, cx)),
            EditStart::Started(_)
        ) {
            self.enter_composer(window, cx);
        }
    }

    pub(crate) fn slack_react_at(
        &mut self,
        ts: Ts,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return;
        };
        let Some(choices) = view
            .clone()
            .update(cx, |view, cx| view.reaction_choices_for(&ts, cx))
        else {
            return;
        };
        self.slack_reacting = Some(ts);
        self.open_menu(crate::transient::slack_react_menu(&choices), window, cx);
    }

    /// Opens an explicit confirmation prompt before deleting an own message.
    /// Header and context-menu mouse actions call this with their target.
    pub(crate) fn confirm_slack_delete_message(
        &mut self,
        ts: Ts,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.open_prompt(
            "delete message? type delete:",
            std::rc::Rc::new(|_, typed, _| {
                vec![Candidate {
                    value: "delete".to_owned(),
                    description: if typed.trim() == "delete" {
                        "Enter permanently deletes your message".to_owned()
                    } else {
                        "confirmation required".to_owned()
                    },
                }]
            }),
            std::rc::Rc::new(move |workspace: &mut Workspace, input, _window, cx| {
                if input.trim() != "delete" {
                    workspace.echo("slack: message not deleted", StyleClass::SystemInfo, cx);
                    return;
                }
                let SurfaceView::SlackConversation(view) = &workspace.active_surface().view else {
                    return;
                };
                let deleting = view
                    .clone()
                    .update(cx, |view, cx| view.delete_message(ts.clone(), cx));
                cx.spawn(async move |this, cx| {
                    let deleted = deleting.await;
                    let _ = this.update(cx, |this, cx| match deleted {
                        Ok(()) => this.echo("slack: message deleted", StyleClass::SystemInfo, cx),
                        Err(error) => this.echo(
                            &format!("slack: {error:#}"),
                            StyleClass::SystemImportant,
                            cx,
                        ),
                    });
                })
                .detach();
            }),
            window,
            cx,
        );
    }

    pub(crate) fn slack_copy_message_link(&mut self, ts: Ts, cx: &mut gpui::Context<Self>) {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return;
        };
        let linking = view
            .clone()
            .update(cx, |view, cx| view.message_link(ts, cx));
        cx.spawn(async move |this, cx| {
            let linked = linking.await;
            let _ = this.update(cx, |this, cx| match linked {
                Ok(link) => {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(link));
                    this.echo("slack: message link copied", StyleClass::SystemInfo, cx);
                }
                Err(error) => this.echo(
                    &format!("slack: {error:#}"),
                    StyleClass::SystemImportant,
                    cx,
                ),
            });
        })
        .detach();
    }

    pub(crate) fn prompt_slack_forward_message(
        &mut self,
        ts: Ts,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.open_prompt(
            "forward to:",
            std::rc::Rc::new(|workspace: &Workspace, needle: &str, cx: &gpui::App| {
                let Some(session) = workspace.slack.session() else {
                    return Vec::new();
                };
                let needle = needle.to_lowercase();
                session
                    .read(cx)
                    .model()
                    .conversation_rows()
                    .into_iter()
                    .filter(|row| row.label.to_lowercase().contains(&needle))
                    .map(|row| Candidate {
                        value: row.label,
                        description: row.id.0,
                    })
                    .collect()
            }),
            std::rc::Rc::new(move |workspace: &mut Workspace, input, _window, cx| {
                let Some(session) = workspace.slack.session() else {
                    return;
                };
                let destination = session
                    .read(cx)
                    .model()
                    .conversation_rows()
                    .into_iter()
                    .find(|row| row.label == input.trim() || row.id.0 == input.trim())
                    .map(|row| row.id);
                let Some(destination) = destination else {
                    workspace.echo("slack: choose a conversation", StyleClass::SystemInfo, cx);
                    return;
                };
                let SurfaceView::SlackConversation(view) = &workspace.active_surface().view else {
                    return;
                };
                let forwarding = view.clone().update(cx, |view, cx| {
                    view.forward_message(ts.clone(), destination, cx)
                });
                cx.spawn(async move |this, cx| {
                    let forwarded = forwarding.await;
                    let _ = this.update(cx, |this, cx| match forwarded {
                        Ok(()) => this.echo("slack: message forwarded", StyleClass::SystemInfo, cx),
                        Err(error) => this.echo(
                            &format!("slack: {error:#}"),
                            StyleClass::SystemImportant,
                            cx,
                        ),
                    });
                })
                .detach();
            }),
            window,
            cx,
        );
    }

    /// `e`: rewrite the message under the cursor. Someone else's message is
    /// not the reader's to change, and a key that does nothing quietly
    /// reads as broken, so the refusal is said out loud.
    pub(crate) fn slack_edit_message(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return false;
        };
        let view = view.clone();
        match view.update(cx, |view, cx| view.start_edit(window, cx)) {
            EditStart::Started(_) => {
                self.enter_composer(window, cx);
                true
            }
            EditStart::NotYours => {
                self.echo(
                    "slack: only your own messages can be edited",
                    StyleClass::SystemInfo,
                    cx,
                );
                true
            }
            EditStart::Nothing => false,
        }
    }

    /// `up` in an empty composer: rewrite the last thing the reader said.
    /// The composer is already in insert mode, so nothing switches here.
    pub(crate) fn slack_edit_last(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return false;
        };
        let view = view.clone();
        matches!(
            view.update(cx, |view, cx| view.edit_last_own(window, cx)),
            EditStart::Started(_)
        )
    }

    /// `escape` with an edit open: the message stands and the composer is
    /// given back what it held. With no edit open this is vim's escape.
    pub(crate) fn slack_cancel_edit(&mut self, cx: &mut gpui::Context<Self>) -> bool {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return false;
        };
        view.clone().update(cx, |view, cx| view.cancel_edit(cx))
    }

    /// Puts the cursor in the composer in insert mode: without the mode
    /// switch the first character typed is swallowed as a motion.
    fn enter_composer(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        if let Ok(action) = cx.build_action("vim::InsertBefore", None) {
            window.dispatch_action(action, cx);
        }
    }

    /// `s`: narrow the listing to what the user types. The prompt is the
    /// search, so there is nothing extra to dismiss afterwards.
    /// A mute, which is Slack's (8 Sep): a thread is unfollowed there and a
    /// channel or direct message is muted there. Rho writes no cell for it
    /// -- a private copy drifted the moment the user muted or unmuted
    /// anywhere else -- so this call is the whole verdict, and `shift-u`
    /// is the same call the other way.
    pub(crate) fn slack_set_unit_muted(
        &mut self,
        unit: &SlackUnit,
        muted: bool,
        cx: &mut gpui::Context<Self>,
    ) {
        match (unit.thread.is_some(), muted) {
            (true, true) => self.slack_ignore_thread(unit, cx),
            (true, false) => self.slack_follow_thread(unit, cx),
            (false, _) => {
                let Some(session) = self.slack.session() else {
                    return;
                };
                let unit = model_unit(unit);
                session.update(cx, |session, cx| session.set_unit_muted(&unit, muted, cx));
            }
        }
    }

    /// `x` on a thread card is Slack's ignore thread: rho's mute is the
    /// verdict on the node, and the same keystroke tells Slack, so no other
    /// client raises the thread either. Rho keeps no subscription state; if
    /// the call fails the mute still stands and the notice says the
    /// thread is still followed in Slack.
    pub(crate) fn slack_ignore_thread(&mut self, thread: &SlackUnit, cx: &mut gpui::Context<Self>) {
        let Some(session) = self.slack.session() else {
            return;
        };
        let Some(key) = thread_key(thread) else {
            return;
        };
        rho_journal::record(rho_journal::Event::SlackThreadIgnored {
            thread: journal_thread_labelled(session.read(cx).model(), &key),
            by: rho_journal::IgnoredBy::Rho,
        });
        session.update(cx, |session, cx| session.ignore_thread(&key, cx));
    }

    /// `shift-u` after `x`: the mute was an unfollow in Slack, so the
    /// undo is a follow there. Nothing else brings the card back, since the
    /// follow list is what says the thread is the user's.
    pub(crate) fn slack_follow_thread(&mut self, thread: &SlackUnit, cx: &mut gpui::Context<Self>) {
        let Some(session) = self.slack.session() else {
            return;
        };
        let Some(key) = thread_key(thread) else {
            return;
        };
        session.update(cx, |session, cx| session.follow_thread(&key, cx));
    }

    /// The other direction: Slack says the thread was unfollowed, here or
    /// anywhere else. The card closes on its own, because the follow list
    /// is what says the thread is the user's and the crate reads it; all
    /// that is left here is the record of who did it. An already closed
    /// card has nothing to record, which is what stops rho's own `x` from
    /// writing the line twice when the socket echoes it back.
    pub(crate) fn slack_thread_muted(&mut self, unit: &SlackUnit, cx: &mut gpui::Context<Self>) {
        let Some(key) = thread_key(unit) else {
            return;
        };
        let key = &key;
        let Some(card) = self.dashboard.thread_card_id(unit) else {
            return;
        };
        if !self.dashboard.node_is_open(card.clone()) {
            return;
        }
        let thread = self
            .slack
            .session()
            .map(|session| journal_thread_labelled(session.read(cx).model(), key))
            .unwrap_or_else(|| journal_thread(&Source::Thread(key.clone())));
        rho_journal::record(rho_journal::Event::SlackThreadIgnored {
            thread,
            by: rho_journal::IgnoredBy::Slack,
        });
        self.refresh_dashboard(cx);
    }

    /// `mark read before`: the backlog older than a cutoff, marked read in
    /// Slack and closed here. The prompt shows what it would touch before
    /// anything happens, because the action is not reversible in Slack.
    pub(crate) fn prompt_slack_mark_read_before(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.open_prompt(
            format!("mark read before ({DEFAULT_MARK_CUTOFF}):"),
            std::rc::Rc::new(|workspace: &Workspace, needle: &str, cx: &gpui::App| {
                let cutoff = mark_cutoff_text(needle);
                match workspace.slack_mark_counts(&cutoff, cx) {
                    Some((conversations, threads)) => vec![Candidate {
                        value: cutoff,
                        description: format!(
                            "{} · {} · enter",
                            plural(conversations, "conversation"),
                            plural(threads, "thread")
                        ),
                    }],
                    None => vec![Candidate {
                        value: cutoff,
                        description: "an age like 7d, or a date like 2026-08-15".to_owned(),
                    }],
                }
            }),
            std::rc::Rc::new(|workspace: &mut Workspace, input, window, cx| {
                workspace.slack_mark_read_before(&input, window, cx);
            }),
            window,
            cx,
        );
    }

    /// What the prompt line counts: conversations and threads Slack would be
    /// told about. `None` means the input is not a cutoff yet.
    fn slack_mark_counts(&self, input: &str, cx: &gpui::App) -> Option<(usize, usize)> {
        let cutoff = parse_mark_cutoff(input, chrono::Local::now())?;
        let plan = self
            .slack
            .session()?
            .read(cx)
            .model()
            .mark_plan(cutoff.timestamp() as f64);
        Some((plan.conversations.len(), plan.threads.len()))
    }

    pub(crate) fn slack_mark_read_before(
        &mut self,
        input: &str,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let text = mark_cutoff_text(input);
        let Some(cutoff) = parse_mark_cutoff(&text, chrono::Local::now()) else {
            self.echo(
                &format!("mark read before: {text} is not an age or a date"),
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        let Some(session) = self.slack.session() else {
            self.echo("slack: no session", StyleClass::SystemInfo, cx);
            return;
        };
        let before = cutoff.timestamp() as f64;
        let plan = session.read(cx).model().mark_plan(before);
        let conversations = plan.conversations.len();
        let threads = plan.threads.len();
        // Reading is not a verdict, so the cursor has to move as well as the
        // marking being sent: what the user just said they are done with is
        // handled here too, and without the cursor the backlog is dealt
        // again on the next start. Every unit the plan covers gets one, at
        // the newest message at or before the cutoff, so anything that
        // arrived since is still theirs.
        let host = self.hosts.owner();
        let workspace_name = session.read(cx).model().workspace().clone();
        let mut nodes: Vec<(rho_desk::cells::Id, rho_desk::cells::SlackTs)> = plan
            .conversations
            .iter()
            .map(|(channel, ts)| {
                (
                    rho_desk::cells::Id::Slack(SlackUnit {
                        workspace: workspace_name.0.clone(),
                        channel: channel.0.clone(),
                        thread: None,
                    }),
                    rho_desk::cells::SlackTs(ts.0.clone()),
                )
            })
            .chain(plan.threads.iter().map(|(key, ts)| {
                (
                    rho_desk::cells::Id::Slack(store_unit_of(key)),
                    rho_desk::cells::SlackTs(ts.0.clone()),
                )
            }))
            .collect();
        // A card older than the cutoff whose unit Slack has nothing unread
        // for is backlog just the same, closed at its own newest.
        let model = session.read(cx).model();
        for (node, cursor) in cards_before(self.dashboard.open_thread_cards(), model, host, before)
        {
            if !nodes.iter().any(|(known, _)| known == &node) {
                nodes.push((node, cursor));
            }
        }
        if conversations == 0 && threads == 0 && nodes.is_empty() {
            self.echo(
                &format!("mark read before {text}: nothing that old"),
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        session.update(cx, |session, cx| session.mark_read_before(plan, cx));
        let closed = match host {
            Some(host) => {
                self.mark_cards_done(host, nodes, "mark read before".to_owned(), window, cx)
            }
            None => 0,
        };
        rho_journal::record(rho_journal::Event::SlackMarkedReadBefore {
            cutoff: text.clone(),
            conversations,
            threads,
        });
        self.echo(
            &format!(
                "marked read before {text}: {} · {} · {closed} closed",
                plural(conversations, "conversation"),
                plural(threads, "thread")
            ),
            StyleClass::SystemInfo,
            cx,
        );
    }

    /// How many matching names the minibuffer offers while the reader
    /// types. A screenful and a little: the list itself narrows to every
    /// match, and this is only the reminder of what is being reached for.
    const SLACK_MATCHES_OFFERED: usize = 64;

    /// Opens workspace-wide message search. Sidebar search uses this even
    /// when a conversation happens to be the active surface.
    pub(crate) fn prompt_slack_find_all(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.prompt_slack_find_with(None, rho_slack::ui::SearchKind::Messages, window, cx);
    }

    /// `shift-s` in a conversation: find a message in that conversation.
    pub(crate) fn prompt_slack_find(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
            return;
        };
        let channel = view.read(cx).source().channel().clone();
        let Some(session) = self.slack_session(window, cx) else {
            return;
        };
        let model = session.read(cx).model();
        let Some(scope) = model.search_scope(&channel) else {
            return;
        };
        let label = model.label(&channel);
        self.prompt_slack_find_with(
            Some((scope, label)),
            rho_slack::ui::SearchKind::Messages,
            window,
            cx,
        );
    }

    /// Opens Slack's standalone file search. It is separate from message
    /// search because a file can match without an associated message match.
    pub(crate) fn prompt_slack_find_files(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.prompt_slack_find_with(None, rho_slack::ui::SearchKind::Files, window, cx);
    }

    fn prompt_slack_find_with(
        &mut self,
        scope: Option<(String, String)>,
        kind: rho_slack::ui::SearchKind,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.slack_session(window, cx).is_none() {
            return;
        }
        let noun = match kind {
            rho_slack::ui::SearchKind::Messages => "messages",
            rho_slack::ui::SearchKind::Files => "files",
        };
        let prompt = match &scope {
            Some((_, label)) => {
                format!("slack {noun} in {label} (from: before: after: has: is:):")
            }
            None => format!("slack {noun} (from: in: before: after: has: is:):"),
        };
        self.open_prompt(
            prompt,
            std::rc::Rc::new(|_: &Workspace, typed: &str, _: &gpui::App| {
                slack_filter_candidates(typed)
            }),
            std::rc::Rc::new(move |workspace: &mut Workspace, input, window, cx| {
                let query = match (input.trim(), &scope) {
                    ("", _) => String::new(),
                    (input, Some((scope, _))) => format!("{input} in:{scope}"),
                    (_, None) => input,
                };
                workspace.slack_find(&query, kind, window, cx);
            }),
            window,
            cx,
        );
    }

    /// Runs a search and opens the surface its answer will be drawn on. The
    /// surface is opened now rather than when the answer lands: the reader
    /// asked, and a screen saying so is the honest thing to show them while
    /// Slack is thinking.
    fn slack_find(
        &mut self,
        query: &str,
        kind: rho_slack::ui::SearchKind,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let query = query.trim().to_owned();
        if query.is_empty() {
            return;
        }
        let Some(session) = self.slack_session(window, cx) else {
            return;
        };
        self.active_context = ContextId::Slack;
        let key = SurfaceKey::SlackResults {
            query: query.clone(),
            kind,
        };
        let surface = match self.find_surface(|surface| surface.key == key).cloned() {
            Some(surface) => surface,
            None => {
                let hooks = Self::slack_hooks();
                let view = cx
                    .new(|cx| rho_slack::ui::ResultsView::new(session.clone(), hooks, window, cx));
                self._slack_view_subscriptions.push(cx.subscribe_in(
                    &view,
                    window,
                    |workspace, _, event: &rho_slack::ui::results::Event, window, cx| {
                        let target = match event {
                            rho_slack::ui::results::Event::Open(place) => {
                                rho_slack::ui::Target::Message(place.clone())
                            }
                            rho_slack::ui::results::Event::OpenFile(file) => {
                                rho_slack::ui::Target::File(file.clone())
                            }
                        };
                        workspace.open_slack_search_target(target, window, cx);
                    },
                ));
                Self::wrap_surface(key, SurfaceView::SlackResults(view))
            }
        };
        self.show_slack_surface(surface, cx);
        self.focus_active_surface(window, cx);
        if let SurfaceView::SlackResults(view) = &self.active_surface().view {
            view.clone()
                .update(cx, |view, cx| view.asking(&query, kind, window, cx));
        }
        session.update(cx, |session, cx| match kind {
            rho_slack::ui::SearchKind::Messages => session.search(&query, 1, cx),
            rho_slack::ui::SearchKind::Files => session.search_files(&query, 1, cx),
        });
        cx.notify();
    }

    /// Requests the adjacent numbered search page. Slack's search endpoints
    /// use page numbers rather than cursors.
    pub(crate) fn slack_search_page(
        &mut self,
        offset: i32,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let SurfaceView::SlackResults(view) = &self.active_surface().view else {
            return false;
        };
        view.clone()
            .update(cx, |view, cx| view.request_adjacent(offset, window, cx))
    }

    /// `enter` on a hit: the conversation, opened at that message. The
    /// window is fetched when the mirror has never held it, so the reader
    /// lands where they chose rather than at the newest messages.
    pub(crate) fn slack_open_found(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let SurfaceView::SlackResults(view) = &self.active_surface().view else {
            return false;
        };
        let Some(target) = view.clone().update(cx, |view, cx| view.cursor_target(cx)) else {
            return false;
        };
        self.open_slack_search_target(target, window, cx)
    }

    fn open_slack_search_target(
        &mut self,
        target: rho_slack::ui::Target,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        match target {
            rho_slack::ui::Target::Message(place) => {
                self.open_slack_source(place.source, window, cx);
                let SurfaceView::SlackConversation(view) = &self.active_surface().view else {
                    return false;
                };
                view.clone()
                    .update(cx, |view, cx| view.reveal_found(place.ts, window, cx));
            }
            rho_slack::ui::Target::File(file) => {
                if file.is_image() {
                    self.open_slack_file_image(file, window, cx);
                } else if let Some(session) = self.slack.session() {
                    session.update(cx, |session, cx| session.open_file(&file, cx));
                }
            }
        }
        true
    }

    /// What the search answered, drawn on the surface the reader is waiting
    /// on. An answer for a query whose surface has been closed is dropped:
    /// nothing is opened behind their back.
    fn slack_found(
        &mut self,
        found: &rho_slack::session::Found,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let key = SurfaceKey::SlackResults {
            query: found.query.clone(),
            kind: found.kind,
        };
        let Some(surface) = self.find_surface(|surface| surface.key == key).cloned() else {
            return;
        };
        let SurfaceView::SlackResults(view) = surface.view else {
            return;
        };
        let found = found.clone();
        view.update(cx, |view, cx| match &found.page {
            Ok(rho_slack::session::FoundPage::Messages(page)) => {
                view.found(&found.query, page, window, cx)
            }
            Ok(rho_slack::session::FoundPage::Files(page)) => {
                view.found_files(&found.query, page, window, cx)
            }
            Err(why) => view.refused(&found.query, why, window, cx),
        });
        cx.notify();
    }

    pub(crate) fn prompt_slack_search(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let SurfaceView::SlackList(view) = &self.active_surface().view else {
            return;
        };
        // What the list stood at before the reader started typing. Escape
        // puts this back: the prompt owns what "back" means, because only
        // it knows the narrowing is a state of the list behind it.
        self.slack_search_before = Some(view.clone().update(cx, |view, cx| view.filter(cx)));
        self.open_prompt_watching(
            "slack:",
            // The matches as the reader types, answered off the crate's
            // word index: a range scan per typed word and the matches, so
            // a keystroke here costs what it reaches and not the
            // workspace. The minibuffer asks this on every edit.
            std::rc::Rc::new(|workspace: &Workspace, typed: &str, cx: &gpui::App| {
                let SurfaceView::SlackList(view) = &workspace.active_surface().view else {
                    return Vec::new();
                };
                view.read(cx)
                    .reached_by(typed, Self::SLACK_MATCHES_OFFERED, cx)
                    .into_iter()
                    .map(|row| crate::minibuffer::Candidate {
                        value: row.label,
                        description: String::new(),
                    })
                    .collect()
            }),
            // The narrowing itself, once per keystroke. The list is not
            // drawn again: the model says which rows left and which
            // arrived, and only those lines are rewritten.
            Some(std::rc::Rc::new(
                |workspace: &mut Workspace, typed: &str, window: &mut gpui::Window, cx| {
                    workspace.slack_narrow(typed, window, cx);
                },
            )),
            std::rc::Rc::new(|workspace: &mut Workspace, input, window, cx| {
                workspace.slack_narrow(&input, window, cx);
            }),
            window,
            cx,
        );
    }

    /// Narrows the Slack list to a query. The one place the narrowing
    /// happens, so a keystroke, a submit and escape putting the old query
    /// back are the same code and cannot drift apart.
    /// The narrowing, for a test that drives it without the prompt.
    #[cfg(test)]
    pub(crate) fn slack_narrow_for_test(
        &mut self,
        query: &str,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.slack_narrow(query, window, cx);
    }

    fn slack_narrow(
        &mut self,
        query: &str,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let SurfaceView::SlackList(view) = &self.active_surface().view else {
            return;
        };
        let query = query.to_owned();
        view.clone()
            .update(cx, |view, cx| view.set_filter(query, window, cx));
    }

    /// Escape out of the search prompt: the list goes back to the narrowing
    /// it stood at when the prompt opened, by the same diff that narrowed
    /// it, so putting it back costs what the narrowing cost.
    pub(crate) fn restore_slack_search(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(before) = self.slack_search_before.take() else {
            return;
        };
        self.slack_narrow(&before, window, cx);
    }

    fn open_slack_view_prompt(
        &mut self,
        view: &serde_json::Value,
        prompt: String,
        complete: crate::minibuffer::CandidateSource,
        on_submit: crate::minibuffer::SubmitHandler,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let closing = view.clone();
        self.open_prompt_cancellable(
            prompt,
            complete,
            on_submit,
            std::rc::Rc::new(move |workspace, _window, cx| {
                if let Some(session) = workspace.slack.session() {
                    session.update(cx, |session, cx| session.close_view(closing.clone(), cx));
                }
            }),
            window,
            cx,
        );
    }

    fn prompt_slack_view(
        &mut self,
        view: serde_json::Value,
        index: usize,
        values: serde_json::Map<String, serde_json::Value>,
        error: Option<String>,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let blocks = view["blocks"].as_array().cloned().unwrap_or_default();
        let Some(block) = blocks.get(index).cloned() else {
            self.prompt_slack_view_submit(view, values, window, cx);
            return;
        };
        if block["type"].as_str() != Some("input") {
            self.prompt_slack_view(view, index + 1, values, error, window, cx);
            return;
        }
        let element = block["element"].clone();
        let kind = element["type"].as_str().unwrap_or_default().to_owned();
        let block_id = block["block_id"].as_str().unwrap_or_default().to_owned();
        let action_id = element["action_id"].as_str().unwrap_or_default().to_owned();
        let label = block["label"]["text"]
            .as_str()
            .unwrap_or(&block_id)
            .to_owned();
        if block_id.is_empty() || action_id.is_empty() {
            self.echo(
                "slack: app modal input has no block or action id",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }

        let prompt = slack_view_field_prompt(&label, false, error.as_deref());

        match kind.as_str() {
            "plain_text_input" => {
                let retry_view = view.clone();
                let next_view = view.clone();
                let retry_values = values.clone();
                let field_block = block.clone();
                let field_element = element.clone();
                let prefill = slack_view_prefill(&values, &block_id, &action_id, &kind, &[]);
                self.open_slack_view_prompt(
                    &view,
                    prompt.clone(),
                    std::rc::Rc::new(|_, _, _| Vec::new()),
                    std::rc::Rc::new(move |workspace, input, window, cx| {
                        if let Some(error) =
                            slack_view_text_error(&field_block, &field_element, &input)
                        {
                            let mut retry_values = retry_values.clone();
                            retry_values.insert(
                                block_id.clone(),
                                slack_view_action_value(
                                    &action_id,
                                    "plain_text_input",
                                    &input,
                                    &input,
                                ),
                            );
                            workspace.prompt_slack_view(
                                retry_view.clone(),
                                index,
                                retry_values,
                                Some(error),
                                window,
                                cx,
                            );
                            return;
                        }
                        let mut values = values.clone();
                        values.insert(
                            block_id.clone(),
                            slack_view_action_value(&action_id, "plain_text_input", &input, &input),
                        );
                        workspace.prompt_slack_view(
                            next_view.clone(),
                            index + 1,
                            values,
                            None,
                            window,
                            cx,
                        );
                    }),
                    window,
                    cx,
                );
                if let (Some(prefill), Some(minibuffer)) = (prefill, &mut self.minibuffer) {
                    minibuffer.set_input(prefill, window, cx);
                }
            }
            "datepicker" => {
                let retry_view = view.clone();
                let next_view = view.clone();
                let retry_values = values.clone();
                let prefill = slack_view_prefill(&values, &block_id, &action_id, &kind, &[]);
                self.open_slack_view_prompt(
                    &view,
                    slack_view_field_prompt(&label, true, error.as_deref()),
                    std::rc::Rc::new(|_, _, _| Vec::new()),
                    std::rc::Rc::new(move |workspace, input, window, cx| {
                        if chrono::NaiveDate::parse_from_str(&input, "%Y-%m-%d").is_err() {
                            let error = "enter a date as YYYY-MM-DD".to_owned();
                            let mut retry_values = retry_values.clone();
                            retry_values.insert(
                                block_id.clone(),
                                slack_view_action_value(&action_id, "datepicker", &input, &input),
                            );
                            workspace.prompt_slack_view(
                                retry_view.clone(),
                                index,
                                retry_values,
                                Some(error),
                                window,
                                cx,
                            );
                            return;
                        }
                        let mut values = values.clone();
                        values.insert(
                            block_id.clone(),
                            slack_view_action_value(&action_id, "datepicker", &input, &input),
                        );
                        workspace.prompt_slack_view(
                            next_view.clone(),
                            index + 1,
                            values,
                            None,
                            window,
                            cx,
                        );
                    }),
                    window,
                    cx,
                );
                if let (Some(prefill), Some(minibuffer)) = (prefill, &mut self.minibuffer) {
                    minibuffer.set_input(prefill, window, cx);
                }
            }
            "static_select" | "users_select" | "channels_select" | "conversations_select" => {
                let options = if kind == "static_select" {
                    element["options"]
                        .as_array()
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|option| {
                            Some(rho_slack::block::InteractionOption {
                                label: option["text"]["text"].as_str()?.to_owned(),
                                value: option["value"].as_str()?.to_owned(),
                            })
                        })
                        .collect()
                } else {
                    self.slack
                        .session()
                        .map(|session| session.read(cx).model().interaction_options(&kind))
                        .unwrap_or_default()
                };
                let prefill = slack_view_prefill(&values, &block_id, &action_id, &kind, &options);
                let choices = options.clone();
                let retry_view = view.clone();
                let next_view = view.clone();
                let retry_values = values.clone();
                let selected_kind = kind.clone();
                self.open_slack_view_prompt(
                    &view,
                    prompt,
                    std::rc::Rc::new(move |_, needle, _| {
                        let needle = needle.to_lowercase();
                        choices
                            .iter()
                            .filter(|option| option.label.to_lowercase().contains(&needle))
                            .map(|option| Candidate {
                                value: option.label.clone(),
                                description: option.value.clone(),
                            })
                            .collect()
                    }),
                    std::rc::Rc::new(move |workspace, input, window, cx| {
                        let Some(option) =
                            options.iter().find(|option| option.label == input).cloned()
                        else {
                            workspace.prompt_slack_view(
                                retry_view.clone(),
                                index,
                                retry_values.clone(),
                                Some("choose one of the offered values".to_owned()),
                                window,
                                cx,
                            );
                            return;
                        };
                        let selection = slack_view_action_value(
                            &action_id,
                            &selected_kind,
                            &option.value,
                            &option.label,
                        );
                        let mut values = values.clone();
                        values.insert(block_id.clone(), selection);
                        workspace.prompt_slack_view(
                            next_view.clone(),
                            index + 1,
                            values,
                            None,
                            window,
                            cx,
                        );
                    }),
                    window,
                    cx,
                );
                if let Some(minibuffer) = &mut self.minibuffer {
                    minibuffer.set_complete_whole_input();
                    if let Some(prefill) = prefill {
                        minibuffer.set_input(prefill, window, cx);
                    }
                }
            }
            _ => self.echo(
                &format!(
                    "slack: {kind} modal inputs are not supported here; use Slack to complete it"
                ),
                StyleClass::SystemInfo,
                cx,
            ),
        }
    }

    fn prompt_slack_view_submit(
        &mut self,
        view: serde_json::Value,
        values: serde_json::Map<String, serde_json::Value>,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let submit = view["submit"]["text"]
            .as_str()
            .unwrap_or("Submit")
            .to_owned();
        let close = view["close"]["text"]
            .as_str()
            .unwrap_or("Cancel")
            .to_owned();
        let title = view["title"]["text"]
            .as_str()
            .unwrap_or("app modal")
            .to_owned();
        let choices = vec![
            Candidate {
                value: submit.clone(),
                description: "send to app".to_owned(),
            },
            Candidate {
                value: close.clone(),
                description: "close without submitting".to_owned(),
            },
        ];
        let submitted_view = view.clone();
        self.open_slack_view_prompt(
            &view,
            format!("{title}:"),
            std::rc::Rc::new(move |_, needle, _| {
                let needle = needle.to_lowercase();
                choices
                    .iter()
                    .filter(|choice| choice.value.to_lowercase().contains(&needle))
                    .cloned()
                    .collect()
            }),
            std::rc::Rc::new(move |workspace, input, _window, cx| {
                let Some(session) = workspace.slack.session() else {
                    return;
                };
                if input == submit {
                    session.update(cx, |session, cx| {
                        session.submit_view(
                            submitted_view.clone(),
                            serde_json::json!({"values": values.clone()}),
                            cx,
                        )
                    });
                } else if input == close {
                    session.update(cx, |session, cx| {
                        session.close_view(submitted_view.clone(), cx)
                    });
                }
            }),
            window,
            cx,
        );
        if let Some(minibuffer) = &mut self.minibuffer {
            minibuffer.set_complete_whole_input();
        }
    }

    fn handle_slack_view_submission(
        &mut self,
        view: serde_json::Value,
        state: serde_json::Value,
        result: rho_slack::api::ViewSubmission,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if let Some(toast) = result.toast_message {
            self.echo(&format!("slack: {toast}"), StyleClass::SystemInfo, cx);
        }
        match result.response_action.as_deref() {
            None | Some("clear") => {}
            Some("update" | "push") => {
                if let Some(view) = result.view {
                    self.prompt_slack_view(view, 0, serde_json::Map::new(), None, window, cx);
                } else {
                    self.echo(
                        "slack: the app changed the modal but returned no view",
                        StyleClass::SystemInfo,
                        cx,
                    );
                }
            }
            Some("errors") => {
                let message = result
                    .view_error
                    .or_else(|| {
                        (!result.errors.is_empty()).then(|| {
                            result
                                .errors
                                .values()
                                .filter_map(|error| error.as_str())
                                .collect::<Vec<_>>()
                                .join("; ")
                        })
                    })
                    .unwrap_or_else(|| "the app rejected the submitted fields".to_owned());
                let first_error = result.errors.keys().next().cloned();
                let index = first_error
                    .as_deref()
                    .and_then(|block_id| {
                        view["blocks"]
                            .as_array()?
                            .iter()
                            .position(|block| block["block_id"].as_str() == Some(block_id))
                    })
                    .unwrap_or(0);
                let values = state["values"].as_object().cloned().unwrap_or_default();
                self.prompt_slack_view(view, index, values, Some(message), window, cx);
            }
            Some(action) => self.echo(
                &format!("slack: unsupported modal response action {action}"),
                StyleClass::SystemInfo,
                cx,
            ),
        }
    }

    fn prompt_slack_dialog(
        &mut self,
        dialog_id: String,
        dialog: serde_json::Value,
        index: usize,
        submission: serde_json::Map<String, serde_json::Value>,
        correction: Option<(String, String)>,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let elements = dialog["elements"].as_array().cloned().unwrap_or_default();
        let Some(element) = elements.get(index).cloned() else {
            self.prompt_slack_dialog_submit(dialog_id, dialog, submission, window, cx);
            return;
        };
        let kind = element["type"].as_str().unwrap_or_default();
        let name = element["name"].as_str().unwrap_or_default().to_owned();
        let label = element["label"].as_str().unwrap_or(&name).to_owned();
        let (error, prefill) = correction
            .map(|(error, input)| (Some(error), Some(input)))
            .unwrap_or_default();
        if name.is_empty() {
            self.echo(
                "slack: app dialog field has no name",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        match kind {
            "text" | "textarea" => {
                let submitted_dialog = dialog.clone();
                let retry_dialog = dialog.clone();
                let retry_id = dialog_id.clone();
                let retry_submission = submission.clone();
                self.open_prompt(
                    slack_dialog_field_prompt(&label, error.as_deref()),
                    std::rc::Rc::new(|_, _, _| Vec::new()),
                    std::rc::Rc::new(move |workspace, input, window, cx| {
                        if let Some(error) = slack_dialog_text_error(&element, &input) {
                            workspace.prompt_slack_dialog(
                                retry_id.clone(),
                                retry_dialog.clone(),
                                index,
                                retry_submission.clone(),
                                Some((error, input)),
                                window,
                                cx,
                            );
                            return;
                        }
                        let mut submission = submission.clone();
                        submission.insert(name.clone(), serde_json::json!(input));
                        workspace.prompt_slack_dialog(
                            dialog_id.clone(),
                            submitted_dialog.clone(),
                            index + 1,
                            submission,
                            None,
                            window,
                            cx,
                        );
                    }),
                    window,
                    cx,
                );
                if let (Some(prefill), Some(minibuffer)) = (prefill, &mut self.minibuffer) {
                    minibuffer.set_input(prefill, window, cx);
                }
            }
            "select" if element["data_source"].as_str() == Some("static") => {
                let options = element["options"].as_array().cloned().unwrap_or_default();
                let choices = options.clone();
                let next_dialog = dialog.clone();
                let retry_dialog = dialog.clone();
                let retry_id = dialog_id.clone();
                let retry_submission = submission.clone();
                self.open_prompt(
                    slack_dialog_field_prompt(&label, error.as_deref()),
                    std::rc::Rc::new(move |_, needle, _| {
                        let needle = needle.to_lowercase();
                        choices
                            .iter()
                            .filter_map(|option| {
                                let label = option["label"].as_str()?;
                                label.to_lowercase().contains(&needle).then(|| Candidate {
                                    value: label.to_owned(),
                                    description: option["value"]
                                        .as_str()
                                        .unwrap_or_default()
                                        .to_owned(),
                                })
                            })
                            .collect()
                    }),
                    std::rc::Rc::new(move |workspace, input, window, cx| {
                        let Some(value) = options.iter().find_map(|option| {
                            (option["label"].as_str() == Some(&input))
                                .then(|| option["value"].as_str().map(str::to_owned))
                                .flatten()
                        }) else {
                            workspace.prompt_slack_dialog(
                                retry_id.clone(),
                                retry_dialog.clone(),
                                index,
                                retry_submission.clone(),
                                Some(("choose one of the offered values".to_owned(), input)),
                                window,
                                cx,
                            );
                            return;
                        };
                        let mut submission = submission.clone();
                        submission.insert(name.clone(), serde_json::json!(value));
                        workspace.prompt_slack_dialog(
                            dialog_id.clone(),
                            next_dialog.clone(),
                            index + 1,
                            submission,
                            None,
                            window,
                            cx,
                        );
                    }),
                    window,
                    cx,
                );
                if let Some(minibuffer) = &mut self.minibuffer {
                    minibuffer.set_complete_whole_input();
                    if let Some(prefill) = prefill {
                        minibuffer.set_input(prefill, window, cx);
                    }
                }
            }
            _ => self.echo(
                &format!("slack: {kind} app dialog fields are not supported yet"),
                StyleClass::SystemInfo,
                cx,
            ),
        }
    }

    fn prompt_slack_dialog_submit(
        &mut self,
        dialog_id: String,
        dialog: serde_json::Value,
        submission: serde_json::Map<String, serde_json::Value>,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let submit = dialog["submit_label"]
            .as_str()
            .unwrap_or("Submit")
            .to_owned();
        let choices = vec![
            Candidate {
                value: submit.clone(),
                description: "send to app".to_owned(),
            },
            Candidate {
                value: "Cancel".to_owned(),
                description: "discard".to_owned(),
            },
        ];
        let title = dialog["title"].as_str().unwrap_or("app dialog").to_owned();
        self.open_prompt(
            format!("{title}:"),
            std::rc::Rc::new(move |_, needle, _| {
                let needle = needle.to_lowercase();
                choices
                    .iter()
                    .filter(|choice| choice.value.to_lowercase().contains(&needle))
                    .cloned()
                    .collect()
            }),
            std::rc::Rc::new(move |workspace, input, _window, cx| {
                if input != submit {
                    return;
                }
                if let Some(session) = workspace.slack.session() {
                    session.update(cx, |session, cx| {
                        session.submit_dialog(
                            dialog_id.clone(),
                            serde_json::Value::Object(submission.clone()),
                            cx,
                        )
                    });
                }
            }),
            window,
            cx,
        );
        if let Some(minibuffer) = &mut self.minibuffer {
            minibuffer.set_complete_whole_input();
        }
    }

    fn on_slack_event(
        &mut self,
        session: gpui::Entity<Session>,
        event: &SessionEvent,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        // The roster can land after a surface was opened, so every cached
        // title is re-read from the model: a conversation is never left
        // reading "#a conversation" once its people are known.
        for source in self.slack_labels.keys().cloned().collect::<Vec<_>>() {
            let label = session.read(cx).label(&source);
            self.slack_labels.insert(source, label);
        }
        match event {
            SessionEvent::OpenConversation(channel) => {
                self.open_slack_source(Source::Conversation(channel.clone()), window, cx);
                self.slack_compose(window, cx);
                self.enter_insert_when_shown(window, cx);
            }
            SessionEvent::Directory(channels) => {
                self.prompt_slack_directory(channels.clone(), window, cx);
            }
            SessionEvent::Found(found) => {
                self.slack_found(found, window, cx);
            }
            SessionEvent::ExternalOptions {
                message,
                action,
                options,
            } => {
                if options.is_empty() {
                    self.echo(
                        "slack: the app offered no matching choices",
                        StyleClass::SystemInfo,
                        cx,
                    );
                } else if let SurfaceView::SlackConversation(view) = &self.active_surface().view {
                    let mut action = action.clone();
                    action.options = options.clone();
                    self.prompt_slack_select(
                        view.clone(),
                        rho_slack::ui::conversation::MessageInteraction {
                            message: message.clone(),
                            action,
                        },
                        window,
                        cx,
                    );
                }
            }
            SessionEvent::View { view } => {
                self.prompt_slack_view(view.clone(), 0, serde_json::Map::new(), None, window, cx);
            }
            SessionEvent::ViewSubmitted {
                view,
                state,
                result,
            } => {
                self.handle_slack_view_submission(
                    view.clone(),
                    state.clone(),
                    result.clone(),
                    window,
                    cx,
                );
            }
            SessionEvent::Dialog { dialog_id, dialog } => {
                self.prompt_slack_dialog(
                    dialog_id.clone(),
                    dialog.clone(),
                    0,
                    serde_json::Map::new(),
                    None,
                    window,
                    cx,
                );
            }
            SessionEvent::Connected => {
                rho_journal::record(rho_journal::Event::SlackConnected {
                    workspace: session.read(cx).model().workspace().0.clone(),
                });
            }
            SessionEvent::Disconnected(reason) => {
                rho_journal::record(rho_journal::Event::SlackDisconnected {
                    workspace: session.read(cx).model().workspace().0.clone(),
                    reason: reason.clone(),
                });
            }
            SessionEvent::Changed(changes) => {
                for change in changes {
                    if let Change::Raised(unit) | Change::Updated(unit) = change
                        && let Some(card) = session
                            .read(cx)
                            .model()
                            .card(unit, chrono::Local::now().timestamp_millis())
                        && !matches!(
                            card.attention,
                            Some(rho_slack::model::Attention::ChannelTraffic) | None
                        )
                    {
                        let focused = match &self.active_surface().key {
                            SurfaceKey::SlackConversation(source) => Some(source),
                            _ => None,
                        };
                        let alert = should_notify_slack(
                            self.slack.notified.get(unit),
                            focused,
                            unit,
                            &card.newest,
                        );
                        let remember = self
                            .slack
                            .notified
                            .get(unit)
                            .is_none_or(|previous| card.newest.is_newer_than(previous));
                        if remember {
                            self.slack
                                .notified
                                .insert(unit.clone(), card.newest.clone());
                        }
                        if alert {
                            let thread = unit.thread.as_ref().map(Ts::as_str).unwrap_or("");
                            cx.show_system_notification(gpui::SystemNotification {
                                tag: format!(
                                    "rho-slack-{}-{}-{thread}",
                                    session.read(cx).model().workspace().0,
                                    unit.channel.0,
                                )
                                .into(),
                                title: card.conversation.clone().into(),
                                body: {
                                    let summary = session.read(cx).unit_summary(unit);
                                    match (summary.is_empty(), card.attention) {
                                        (true, Some(reason)) => rho_slack::model::reason_text(
                                            reason,
                                            &card.conversation,
                                        ),
                                        _ => summary,
                                    }
                                }
                                .into(),
                                actions: Vec::new(),
                            });
                        }
                    }
                    // A thread that starts to matter needs nothing written:
                    // it is addressable as its unit, and the view shows it
                    // because the mirror says it is open.
                    match change {
                        Change::Raised(_) | Change::Updated(_) => {}
                        // Slack said the thread is not the user's any more.
                        // That is a verdict made in another client, so the
                        // card closes here without asking.
                        Change::Muted(unit) => {
                            let unit = store_unit(session.read(cx).model().workspace(), unit);
                            self.slack_thread_muted(&unit, cx);
                        }
                        Change::Replied(_) => {}
                    }
                }
                // The mirror moved, so the join every Slack card is derived
                // from has to be rebuilt: a unit that started to matter is a
                // row the moment the message lands, with nothing written.
                if let Some(host) = self.hosts.owner() {
                    self.sync_tree_dashboard(host, window, cx);
                }
                self.invalidate_dealer_signals(cx);
            }
            SessionEvent::Notice(text) => {
                self.notice_on(None, text, StyleClass::StatusError, cx);
            }
            SessionEvent::Replied(key) => {
                let thread = journal_thread_labelled(session.read(cx).model(), key);
                rho_journal::record(rho_journal::Event::SlackReplied { thread });
            }
            SessionEvent::Health(signal) => match signal {
                Signal::Degraded(reason) => {
                    self.slack.fell_behind(reason.clone());
                    self.notice_on(None, reason, StyleClass::StatusError, cx);
                    self.invalidate_dealer_signals(cx);
                }
                Signal::Recovered => {
                    self.slack.caught_up();
                    self.echo("slack: caught up", StyleClass::SystemInfo, cx);
                    self.invalidate_dealer_signals(cx);
                }
            },
        }
        cx.notify();
    }
}

/// The journal's name for a thread. The conversation is the label a person
/// would recognise; the thread key is kept so two threads in one channel do
/// not merge in the record.
pub(crate) fn journal_thread(source: &Source) -> rho_journal::SlackThread {
    match source {
        Source::Conversation(channel) => rho_journal::SlackThread {
            workspace: String::new(),
            conversation: channel.0.clone(),
            thread: String::new(),
        },
        Source::Thread(key) => rho_journal::SlackThread {
            workspace: key.workspace.0.clone(),
            conversation: key.channel.0.clone(),
            thread: key.thread_ts.0.clone(),
        },
    }
}

fn journal_thread_labelled(model: &Model, key: &ThreadKey) -> rho_journal::SlackThread {
    rho_journal::SlackThread {
        workspace: key.workspace.0.clone(),
        conversation: model.label(&key.channel),
        thread: key.thread_ts.0.clone(),
    }
}

impl Workspace {
    /// The unit a conversation surface stands for: the desk's own id for
    /// what is on screen, whether or not the desk has a card for it. Only
    /// the workspace's name comes from the session, so this is `None`
    /// exactly when there is no session at all.
    pub(crate) fn slack_surface_unit(&self, source: &Source, cx: &gpui::App) -> Option<SlackUnit> {
        let session = self.slack.session()?;
        let workspace = session.read(cx).model().workspace().clone();
        Some(unit_of_source(&workspace, source))
    }

    /// What every tracked unit is currently about. The dealer reads this
    /// live from the mirror rather than storing any of it in the tree.
    ///
    /// Whether a unit is a *card* is not decided here any more: each one
    /// carries `reason`, which is the crate's answer to whether Slack
    /// itself would be badging it, and the desk closes the ones it says
    /// nothing for. A mention read on the phone this morning has a reason
    /// of `None` and is not handed to anybody.
    /// The Slack facts the desk is built from. The crate answers what it is
    /// asking about and in what words; this is the map from its cards onto
    /// the desk's own cells, and it decides nothing.
    pub(crate) fn slack_thread_facts(
        &self,
        cx: &gpui::App,
    ) -> std::collections::HashMap<SlackUnit, SlackFacts> {
        let Some(session) = self.slack.session() else {
            return std::collections::HashMap::new();
        };
        let now = chrono::Local::now();
        let session = session.read(cx);
        let workspace = session.model().workspace();
        session
            .tracked_cards(now.timestamp_millis())
            .into_iter()
            .filter_map(|card| {
                let raised_at = chrono::DateTime::from_timestamp_millis(card.first_seen_ms)?
                    .with_timezone(&now.timezone())
                    .fixed_offset();
                Some((
                    store_unit(workspace, &card.unit),
                    SlackFacts {
                        title: card.title,
                        conversation: card.conversation.clone(),
                        reason: card.attention,
                        raised_at,
                        wait_days: card.wait_days,
                        latest: card.newest.0,
                        newest_from_other: card.newest_from_other.map(|ts| ts.0),
                        others_replied: card.others_replied,
                    },
                ))
            })
            .collect()
    }
}

/// Which open thread cards this host's cutoff closes. Whether the cutoff
/// closes a unit at all is Slack's question and `Model::closed_by` answers
/// it — a unit the mirror has nothing to say about is left alone; which
/// cards belong to this host is the desk's, and that is all that is decided
/// here.
fn cards_before(
    cards: Vec<(crate::dashboard::DealCardId, SlackUnit)>,
    model: &Model,
    host: Option<rho_agents::HostId>,
    before: f64,
) -> Vec<(rho_desk::cells::Id, rho_desk::cells::SlackTs)> {
    cards
        .into_iter()
        .filter(|(card, _)| Some(card.host) == host)
        .filter_map(|(card, thread)| {
            let closed = model.closed_by(&model_unit(&thread), before)?;
            Some((card.node_id, rho_desk::cells::SlackTs(closed.0)))
        })
        .collect()
}

/// `1 conversation`, `2 conversations`: the count line is read as a
/// sentence, not as a table.
fn plural(count: usize, noun: &str) -> String {
    match count {
        1 => format!("1 {noun}"),
        _ => format!("{count} {noun}s"),
    }
}

/// The age the prompt offers when the user says nothing: a week of backlog
/// is the one the question is usually about.
const DEFAULT_MARK_CUTOFF: &str = "7d";

fn mark_cutoff_text(input: &str) -> String {
    match input.trim() {
        "" => DEFAULT_MARK_CUTOFF.to_owned(),
        text => text.to_owned(),
    }
}

fn slack_view_field_prompt(label: &str, date: bool, error: Option<&str>) -> String {
    let format = if date { " (YYYY-MM-DD)" } else { "" };
    match error {
        Some(error) => format!("{label}{format} — {error}:"),
        None => format!("{label}{format}:"),
    }
}

fn slack_view_prefill(
    values: &serde_json::Map<String, serde_json::Value>,
    block_id: &str,
    action_id: &str,
    kind: &str,
    options: &[rho_slack::block::InteractionOption],
) -> Option<String> {
    let action = values.get(block_id)?.get(action_id)?;
    let value = match kind {
        "plain_text_input" => action["value"].as_str(),
        "datepicker" => action["selected_date"].as_str(),
        "users_select" => action["selected_user"].as_str(),
        "channels_select" => action["selected_channel"].as_str(),
        "conversations_select" => action["selected_conversation"].as_str(),
        _ => action["selected_option"]["value"].as_str(),
    }?;
    if matches!(kind, "plain_text_input" | "datepicker") {
        Some(value.to_owned())
    } else {
        options
            .iter()
            .find(|option| option.value == value)
            .map(|option| option.label.clone())
    }
}

fn slack_view_action_value(
    action_id: &str,
    kind: &str,
    value: &str,
    label: &str,
) -> serde_json::Value {
    let value = match kind {
        "plain_text_input" => serde_json::json!({"type": kind, "value": value}),
        "datepicker" => serde_json::json!({"type": kind, "selected_date": value}),
        "users_select" => serde_json::json!({"type": kind, "selected_user": value}),
        "channels_select" => serde_json::json!({"type": kind, "selected_channel": value}),
        "conversations_select" => {
            serde_json::json!({"type": kind, "selected_conversation": value})
        }
        _ => serde_json::json!({
            "type": "static_select",
            "selected_option": {
                "text": {"type": "plain_text", "text": label},
                "value": value
            }
        }),
    };
    serde_json::json!({action_id: value})
}

fn slack_view_text_error(
    block: &serde_json::Value,
    element: &serde_json::Value,
    input: &str,
) -> Option<String> {
    let label = block["label"]["text"].as_str().unwrap_or("field");
    let optional = block["optional"].as_bool().unwrap_or(false);
    let count = input.chars().count();
    if !optional && input.trim().is_empty() {
        return Some(format!("{label} is required"));
    }
    if let Some(minimum) = element["min_length"].as_u64()
        && count < minimum as usize
    {
        return Some(format!("{label} needs at least {minimum} characters"));
    }
    if let Some(maximum) = element["max_length"].as_u64()
        && count > maximum as usize
    {
        return Some(format!("{label} allows at most {maximum} characters"));
    }
    None
}

fn slack_dialog_field_prompt(label: &str, error: Option<&str>) -> String {
    match error {
        Some(error) => format!("{label} — {error}:"),
        None => format!("{label}:"),
    }
}

fn slack_dialog_text_error(element: &serde_json::Value, input: &str) -> Option<String> {
    let label = element["label"].as_str().unwrap_or("field");
    let optional = element["optional"].as_bool().unwrap_or(false);
    let count = input.chars().count();
    if !optional && input.trim().is_empty() {
        return Some(format!("{label} is required"));
    }
    if let Some(minimum) = element["min_length"].as_u64()
        && count < minimum as usize
    {
        return Some(format!("{label} needs at least {minimum} characters"));
    }
    if let Some(maximum) = element["max_length"].as_u64()
        && count > maximum as usize
    {
        return Some(format!("{label} allows at most {maximum} characters"));
    }
    None
}

/// `7d` is an age counted back from now; `2026-08-15` is a date, and the
/// cutoff is its first moment, so the day itself is left alone.
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
    let milliseconds = crate::workspace::parse_duration_ms(text)?;
    now.checked_sub_signed(chrono::TimeDelta::try_milliseconds(milliseconds as i64)?)
}

/// The Slack subscription a unit stands for, or `None` when the unit is a
/// conversation: following and unfollowing are things Slack only knows how
/// to do to a thread.
pub(crate) fn thread_key(unit: &SlackUnit) -> Option<ThreadKey> {
    Some(ThreadKey {
        workspace: rho_slack::config::WorkspaceName(unit.workspace.clone()),
        channel: ChannelId(unit.channel.clone()),
        thread_ts: Ts(unit.thread.clone()?),
    })
}

/// The store's id for a unit the model named.
pub(crate) fn store_unit(workspace: &WorkspaceName, unit: &Unit) -> SlackUnit {
    SlackUnit {
        workspace: workspace.0.clone(),
        channel: unit.channel.0.clone(),
        thread: unit.thread.as_ref().map(|ts| ts.0.clone()),
    }
}

/// Which surface a unit opens: a followed thread has its own, and a
/// conversation, whether it is a direct message or a channel someone
/// mentioned the user in, opens as the room it is.
pub(crate) fn unit_source(unit: &SlackUnit) -> Source {
    match thread_key(unit) {
        Some(key) => Source::Thread(key),
        None => Source::Conversation(ChannelId(unit.channel.clone())),
    }
}

/// The unit a surface shows, which is the one a mute on it is about.
pub(crate) fn unit_of_source(workspace: &WorkspaceName, source: &Source) -> SlackUnit {
    match source {
        Source::Thread(key) => store_unit_of(key),
        Source::Conversation(channel) => SlackUnit {
            workspace: workspace.0.clone(),
            channel: channel.0.clone(),
            thread: None,
        },
    }
}

/// The store's id for a thread surface.
pub(crate) fn store_unit_of(key: &ThreadKey) -> SlackUnit {
    SlackUnit {
        workspace: key.workspace.0.clone(),
        channel: key.channel.0.clone(),
        thread: Some(key.thread_ts.0.clone()),
    }
}

/// The model's unit for a store id.
pub(crate) fn model_unit(unit: &SlackUnit) -> Unit {
    Unit {
        channel: ChannelId(unit.channel.clone()),
        thread: unit.thread.clone().map(Ts),
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use rho_slack::api::parse_message;
    use rho_slack::types::{Conversation, ConversationKind, UserId};
    use serde_json::json;

    use super::*;
    use crate::tests::{bind_test_keymaps, test_workspace};

    fn model() -> Model {
        let mut model = Model::new(WorkspaceName("acme".into()));
        model.set_self(UserId("ME".into()));
        model.add_conversations([Conversation {
            id: ChannelId("C1".into()),
            kind: ConversationKind::Channel,
            name: "design".into(),
            user: None,
            members: Vec::new(),
        }]);
        model.add_users([rho_slack::types::User {
            id: UserId("ME".into()),
            name: "Manmeet".into(),
            handle: "manmeet".into(),
        }]);
        model
    }

    fn thread_ref_of(thread_ts: &str) -> SlackUnit {
        SlackUnit {
            workspace: "acme".to_owned(),
            channel: "C1".to_owned(),
            thread: Some(thread_ts.to_owned()),
        }
    }

    /// The cutoff is the whole of what the command touches: a card whose
    /// newest message is newer than it stays open, however old the card is.
    /// Which units the cutoff reaches is now the crate's answer, so the
    /// model is seeded with the messages rather than the map being written
    /// out by hand.
    #[test]
    fn only_cards_older_than_the_cutoff_are_closed() {
        let mut model = model();
        model.set_followed(
            ["100.0", "900.0"].map(|ts| (ChannelId("C1".into()), Ts(ts.to_owned()), None)),
        );
        for ts in ["100.0", "900.0"] {
            model.note_message(&message(ts, Some(ts), "U1", "any update?"), 0);
        }
        let host = rho_agents::HostId::default();
        let node = |counter: u8| rho_desk::cells::Id::Note(rho_desk::cells::Uuid([counter; 16]));
        let card = |node_id| crate::dashboard::DealCardId { host, node_id };
        let cards = vec![
            (card(node(1)), thread_ref_of("100.0")),
            (card(node(2)), thread_ref_of("900.0")),
            (card(node(3)), thread_ref_of("50.0")),
        ];

        assert_eq!(
            cards_before(cards, &model, Some(host), 500.0),
            vec![(node(1), rho_desk::cells::SlackTs("100.0".to_owned()))],
            "the newer thread stays, and one the mirror has nothing on is left alone"
        );
    }

    #[gpui::test]
    fn legacy_dialog_corrections_keep_invalid_text_and_select_input_in_the_prompt(
        cx: &mut TestAppContext,
    ) {
        cx.update(bind_test_keymaps);
        let workspace = test_workspace(cx);
        let text_dialog = json!({
            "elements": [{
                "type": "text",
                "name": "summary",
                "label": "Summary",
                "min_length": 3
            }]
        });
        workspace
            .update(cx, |workspace, window, cx| {
                workspace.prompt_slack_dialog(
                    "dialog-text".to_owned(),
                    text_dialog,
                    0,
                    serde_json::Map::new(),
                    None,
                    window,
                    cx,
                );
            })
            .unwrap();

        cx.simulate_keystrokes(*workspace, "n o enter");
        cx.run_until_parked();
        workspace
            .update(cx, |workspace, _, cx| {
                let prompt = workspace.minibuffer.as_ref().expect("text retry prompt");
                assert_eq!(
                    prompt.prompt(),
                    "Summary — Summary needs at least 3 characters:"
                );
                assert_eq!(prompt.input(cx), "no");
            })
            .unwrap();

        let select_dialog = json!({
            "elements": [{
                "type": "select",
                "name": "color",
                "label": "Color",
                "data_source": "static",
                "options": [{"label": "Red", "value": "red"}]
            }]
        });
        workspace
            .update(cx, |workspace, window, cx| {
                workspace.prompt_slack_dialog(
                    "dialog-select".to_owned(),
                    select_dialog,
                    0,
                    serde_json::Map::new(),
                    None,
                    window,
                    cx,
                );
            })
            .unwrap();

        cx.simulate_keystrokes(*workspace, "B l u e enter");
        cx.run_until_parked();
        workspace
            .update(cx, |workspace, _, cx| {
                let prompt = workspace.minibuffer.as_ref().expect("select retry prompt");
                assert_eq!(prompt.prompt(), "Color — choose one of the offered values:");
                assert_eq!(prompt.input(cx), "Blue");
            })
            .unwrap();
    }

    #[test]
    fn modern_modal_corrections_keep_the_error_and_entered_value_in_the_editor_prompt() {
        assert_eq!(
            slack_view_field_prompt(
                "Deployment note",
                false,
                Some("The app rejected this deployment note")
            ),
            "Deployment note — The app rejected this deployment note:"
        );
        let values = json!({"deploy_note": {
            "note": {"type": "plain_text_input", "value": "reject"}
        }});
        assert_eq!(
            slack_view_prefill(
                values.as_object().unwrap(),
                "deploy_note",
                "note",
                "plain_text_input",
                &[]
            )
            .as_deref(),
            Some("reject")
        );
    }

    #[test]
    fn modern_modal_values_use_slacks_action_state_shapes() {
        assert_eq!(
            slack_view_action_value("owner", "users_select", "U1", "@Ada"),
            json!({"owner": {"type": "users_select", "selected_user": "U1"}})
        );
        assert_eq!(
            slack_view_action_value("urgency", "static_select", "urgent", "Urgent"),
            json!({"urgency": {
                "type": "static_select",
                "selected_option": {
                    "text": {"type": "plain_text", "text": "Urgent"},
                    "value": "urgent"
                }
            }})
        );
        assert_eq!(
            slack_view_action_value("date", "datepicker", "2026-08-15", "2026-08-15"),
            json!({"date": {"type": "datepicker", "selected_date": "2026-08-15"}})
        );
    }

    #[test]
    fn modern_modal_text_fields_enforce_required_and_length_bounds() {
        let block = json!({
            "label": {"type": "plain_text", "text": "Deployment note"}
        });
        let element = json!({"min_length": 3, "max_length": 5});
        assert_eq!(
            slack_view_text_error(&block, &element, ""),
            Some("Deployment note is required".to_owned())
        );
        assert_eq!(
            slack_view_text_error(&block, &element, "go"),
            Some("Deployment note needs at least 3 characters".to_owned())
        );
        assert!(slack_view_text_error(&block, &element, "ship").is_none());
        assert_eq!(
            slack_view_text_error(&block, &element, "launch"),
            Some("Deployment note allows at most 5 characters".to_owned())
        );
    }

    #[test]
    fn a_cutoff_is_an_age_or_a_date() {
        let now = chrono::Local::now();
        assert_eq!(
            mark_cutoff_text("  "),
            "7d",
            "nothing typed means the default"
        );
        let week = parse_mark_cutoff("7d", now).unwrap();
        assert_eq!((now - week).num_days(), 7);
        let date = parse_mark_cutoff("2026-08-15", now).unwrap();
        assert_eq!(
            date.format("%Y-%m-%d %H:%M").to_string(),
            "2026-08-15 00:00"
        );
        assert!(parse_mark_cutoff("last tuesday", now).is_none());
    }

    fn message(
        ts: &str,
        thread_ts: Option<&str>,
        user: &str,
        text: &str,
    ) -> rho_slack::types::Message {
        let mut value = json!({"ts": ts, "user": user, "text": text});
        if let Some(thread_ts) = thread_ts {
            value["thread_ts"] = json!(thread_ts);
        }
        parse_message(&value, &ChannelId("C1".into())).unwrap()
    }

    #[test]
    fn a_mention_and_its_follow_ups_name_one_unit_to_bind() {
        // One card per unit: the mention that raised the channel and the
        // reply that keeps it alive are the same unit, so the client acts on
        // one row rather than one per message.
        let mut model = model();
        let raised = model
            .note_message(&message("100.0", None, "U1", "hey <@ME> look"), 0)
            .expect("a mention raises");
        // A second mention in the same channel is the same card, which is
        // the whole point of a unit: three mentions are one row, not three.
        let updated = model
            .note_message(&message("101.0", None, "U1", "<@ME> still stuck"), 0)
            .expect("a newer mention updates");
        let binds = [raised, updated]
            .iter()
            .filter_map(|change| match change {
                Change::Raised(unit) | Change::Updated(unit) => {
                    Some(store_unit(model.workspace(), unit))
                }
                Change::Replied(_) | Change::Muted(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(binds.len(), 2, "a raise and an update both bind");
        assert_eq!(binds[0], binds[1], "one unit is one row");
        assert_eq!(binds[0].channel, "C1");
        assert_eq!(
            binds[0].thread, None,
            "a reply in a thread nobody follows is traffic in the channel"
        );

        // Your own reply is not a verdict, and it binds nothing: the node
        // already exists, and a thread you closed stays closed until they
        // write again.
        let replied = model
            .note_message(&message("102.0", None, "ME", "on it"), 0)
            .expect("your own reply is announced");
        assert!(matches!(replied, Change::Replied(_)));
    }

    #[test]
    fn a_slack_deal_opens_the_thread_only_when_the_unit_is_one() {
        // The unit says which surface it is. A conversation unit is the room
        // it names, whether the message in it was a mention or a direct
        // message; only a followed thread opens as a thread.
        let conversation = SlackUnit {
            workspace: "acme".to_owned(),
            channel: "C1".to_owned(),
            thread: None,
        };
        assert!(matches!(
            unit_source(&conversation),
            Source::Conversation(ChannelId(channel)) if channel == "C1"
        ));
        let Source::Thread(key) = unit_source(&thread_ref_of("500.0")) else {
            panic!("a followed thread must deal in its thread");
        };
        assert_eq!(key.channel.0, "C1");
        assert_eq!(key.thread_ts.0, "500.0");
    }
}

#[cfg(test)]
mod awareness_tests {
    use rho_slack::model::Unit;
    use rho_slack::types::{ChannelId, Ts};

    use super::{Source, should_notify_slack};

    #[test]
    fn desktop_notification_requires_a_new_timestamp_away_from_its_conversation() {
        let channel = ChannelId("C1".into());
        let unit = Unit::conversation(&channel);
        let old = Ts("100.0".into());
        let new = Ts("200.0".into());
        assert!(should_notify_slack(Some(&old), None, &unit, &new));
        assert!(!should_notify_slack(Some(&new), None, &unit, &new));
        assert!(!should_notify_slack(Some(&new), None, &unit, &old));
        assert!(!should_notify_slack(
            Some(&old),
            Some(&Source::Conversation(channel)),
            &unit,
            &new,
        ));
    }
}

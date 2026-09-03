//! Slack in the GUI: registering workspaces, and (from here on) the live
//! session, the items it raises, and the thread surface.
//!
//! The Slack client itself lives in `rho-slack`; this module is only the
//! seam where it meets the workspace, the inbox, and the journal.

use gpui::AppContext as _;
use rho_slack::config::{CredentialStore, Credentials, WorkspaceName};
use rho_slack::health::Signal;
use rho_slack::model::{Change, Model, ThreadCard, Waiting};
use rho_slack::session::{Session, SessionEvent, Source};
use rho_slack::types::{ChannelId, ThreadKey, Ts};

use crate::inbox::{
    CapturedContext, InboxDraft, InboxId, InboxKind, InboxStore, SourceReference, now_ms,
};
use crate::minibuffer::Candidate;
use crate::pane::SurfaceKey;
use crate::style::StyleClass;
use crate::workspace::{ContextId, SurfaceView, Workspace};

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

    pub(crate) fn slack_credentials(&self) -> anyhow::Result<CredentialStore> {
        CredentialStore::open_default()
    }

    pub(crate) fn slack_workspaces(&self) -> Vec<WorkspaceName> {
        self.slack_credentials()
            .map(|store| store.workspaces().collect())
            .unwrap_or_default()
    }
}

impl Workspace {
    /// Opens the conversation list, starting the session on first entry.
    /// This is the way in: everything else is reached from a row.
    pub(crate) fn open_slack(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        if self.slack_session(cx).is_none() {
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

    /// The live session, started from the first registered workspace. A
    /// session is per workspace; the prompt registers several, and the first
    /// is the one rho lives in.
    pub(crate) fn slack_session(
        &mut self,
        cx: &mut gpui::Context<Self>,
    ) -> Option<gpui::Entity<Session>> {
        if let Some(session) = &self.slack {
            return Some(session.clone());
        }
        let store = self.slack_credentials().ok()?;
        let name = store.workspaces().next()?;
        let credentials = store.get(&name)?.clone();
        let session = cx.new(|cx| Session::new(credentials, cx));
        self.slack_items.adopt(&self.inbox, &name);
        self._slack_subscription = Some(cx.subscribe(&session, |workspace, session, event, cx| {
            workspace.on_slack_event(session, event, cx);
        }));
        self.slack = Some(session.clone());
        Some(session)
    }

    /// The host services the Slack surfaces borrow, the same two the Zulip
    /// client borrows, so chat reads like every other buffer in the frame.
    pub(crate) fn slack_hooks() -> rho_slack::ui::Hooks {
        rho_slack::ui::Hooks {
            configure_editor: crate::editor_config::configure,
            configure_markdown: crate::render::markdown::configure_buffer,
        }
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
        let Some(session) = self.slack_session(cx) else {
            return;
        };
        self.active_context = ContextId::Slack;
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
        self.display_surface_with_method(surface, crate::journal::SurfaceShowMethod::Command, cx);
    }

    /// `enter` on a list row, or on a message: the row's conversation, or
    /// the thread the message is in.
    pub(crate) fn slack_open_row(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let source = match &self.active_pane().surface.view {
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

    /// `i`: into the composer, which is where a reply is written.
    pub(crate) fn slack_compose(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if let SurfaceView::SlackConversation(view) = &self.active_pane().surface.view {
            view.clone()
                .update(cx, |view, cx| view.select_compose(window, cx));
            // `i` is vim's own insert key, and this binding took it: without
            // this the cursor lands in the composer still in normal mode and
            // the first character typed is swallowed as a motion.
            if let Ok(action) = cx.build_action("vim::InsertBefore", None) {
                window.dispatch_action(action, cx);
            }
        }
    }

    /// `enter` in the composer: send.
    pub(crate) fn slack_submit(&mut self, cx: &mut gpui::Context<Self>) {
        if let SurfaceView::SlackConversation(view) = &self.active_pane().surface.view {
            view.clone().update(cx, |view, cx| view.submit(cx));
        }
    }

    pub(crate) fn slack_load_older(&mut self, cx: &mut gpui::Context<Self>) {
        if let SurfaceView::SlackConversation(view) = &self.active_pane().surface.view {
            view.clone().update(cx, |view, cx| view.load_older(cx));
        }
    }

    /// `s`: narrow the listing to what the user types. The prompt is the
    /// search, so there is nothing extra to dismiss afterwards.
    pub(crate) fn prompt_slack_search(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if !matches!(self.active_pane().surface.view, SurfaceView::SlackList(_)) {
            return;
        }
        self.open_prompt(
            "slack:",
            std::rc::Rc::new(|_, _, _| Vec::new()),
            std::rc::Rc::new(|workspace: &mut Workspace, input, window, cx| {
                if let SurfaceView::SlackList(view) = &workspace.active_pane().surface.view {
                    let input = input.to_owned();
                    view.clone()
                        .update(cx, |view, cx| view.set_filter(input, window, cx));
                }
            }),
            window,
            cx,
        );
    }

    fn on_slack_event(
        &mut self,
        session: gpui::Entity<Session>,
        event: &SessionEvent,
        cx: &mut gpui::Context<Self>,
    ) {
        match event {
            SessionEvent::Connected => {
                crate::journal::record(crate::journal::Event::SlackConnected {
                    workspace: session.read(cx).model().workspace().0.clone(),
                });
            }
            SessionEvent::Disconnected(reason) => {
                crate::journal::record(crate::journal::Event::SlackDisconnected {
                    workspace: session.read(cx).model().workspace().0.clone(),
                    reason: reason.clone(),
                });
            }
            SessionEvent::Changed(changes) => {
                let now = now_ms();
                for change in changes {
                    let ingested = {
                        let model = session.read(cx).model();
                        self.slack_items.apply(&mut self.inbox, model, change, now)
                    };
                    match ingested {
                        Ok(Some(id)) => {
                            if matches!(change, rho_slack::model::Change::Raised(_)) {
                                let thread = journal_thread_labelled(
                                    session.read(cx).model(),
                                    change_key(change),
                                );
                                crate::journal::record(crate::journal::Event::SlackItemIngested {
                                    thread,
                                    inbox_id: id.0.clone(),
                                });
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            tracing::error!(%error, "slack inbox write failed");
                        }
                    }
                }
                self.invalidate_dealer_signals(cx);
            }
            SessionEvent::Replied(key) => {
                let thread = journal_thread_labelled(session.read(cx).model(), key);
                crate::journal::record(crate::journal::Event::SlackReplied { thread });
            }
            SessionEvent::Health(signal) => match signal {
                Signal::Degraded(reason) => {
                    self.slack_degraded = Some(reason.clone());
                    self.notice_on(None, reason, StyleClass::StatusError, cx);
                    self.invalidate_dealer_signals(cx);
                }
                Signal::Recovered => {
                    self.slack_degraded = None;
                    self.echo("slack: caught up", StyleClass::SystemInfo, cx);
                    self.invalidate_dealer_signals(cx);
                }
            },
        }
        cx.notify();
    }
}

fn change_key(change: &Change) -> &ThreadKey {
    match change {
        Change::Raised(key) | Change::Updated(key) | Change::Quieted(key) => key,
    }
}

/// The journal's name for a thread. The conversation is the label a person
/// would recognise; the thread key is kept so two threads in one channel do
/// not merge in the record.
pub(crate) fn journal_thread(source: &Source) -> crate::journal::SlackThread {
    match source {
        Source::Conversation(channel) => crate::journal::SlackThread {
            workspace: String::new(),
            conversation: channel.0.clone(),
            thread: String::new(),
        },
        Source::Thread(key) => crate::journal::SlackThread {
            workspace: key.workspace.0.clone(),
            conversation: key.channel.0.clone(),
            thread: key.thread_ts.0.clone(),
        },
    }
}

fn journal_thread_labelled(model: &Model, key: &ThreadKey) -> crate::journal::SlackThread {
    crate::journal::SlackThread {
        workspace: key.workspace.0.clone(),
        conversation: model.label(&key.channel),
        thread: key.thread_ts.0.clone(),
    }
}

/// The inbox side of a Slack session: which thread is which item.
///
/// The Slack session owns these items outright — it appends, updates, and
/// retires them from the wire — so the user never files a stale obligation
/// and never has to dismiss one they already answered.
#[derive(Default)]
pub(crate) struct SlackItems {
    items: std::collections::BTreeMap<ThreadKey, InboxId>,
}

impl SlackItems {
    /// Applies one model change to the inbox. Returns the item behind the
    /// thread when one is now live, which is what the journal records.
    pub(crate) fn apply(
        &mut self,
        store: &mut InboxStore,
        model: &Model,
        change: &Change,
        now_ms: i64,
    ) -> anyhow::Result<Option<InboxId>> {
        match change {
            Change::Raised(key) | Change::Updated(key) => {
                let Some(card) = model.card(key, now_ms) else {
                    return Ok(None);
                };
                let draft = draft_for(&card);
                match self.items.get(key) {
                    Some(id) if store.get(id).is_some() => {
                        let id = id.clone();
                        store.update(&id, draft)?;
                        Ok(Some(id))
                    }
                    _ => {
                        let id = store.append(draft)?;
                        self.items.insert(key.clone(), id.clone());
                        Ok(Some(id))
                    }
                }
            }
            Change::Quieted(key) => {
                if let Some(id) = self.items.remove(key) {
                    store.retire(&id)?;
                }
                Ok(None)
            }
        }
    }

    /// Rebuilds the thread→item map from what the store already holds, so a
    /// restart does not raise a second card for a thread already in the
    /// inbox.
    pub(crate) fn adopt(&mut self, store: &InboxStore, workspace: &WorkspaceName) {
        for item in store.items() {
            let SourceReference::SlackThread {
                workspace: item_workspace,
                channel,
                thread_ts,
                ..
            } = &item.source
            else {
                continue;
            };
            if item_workspace != &workspace.0 {
                continue;
            }
            self.items.insert(
                ThreadKey {
                    workspace: workspace.clone(),
                    channel: ChannelId(channel.clone()),
                    thread_ts: Ts(thread_ts.clone()),
                },
                item.id.clone(),
            );
        }
    }

    pub(crate) fn item(&self, key: &ThreadKey) -> Option<&InboxId> {
        self.items.get(key)
    }
}

fn draft_for(card: &ThreadCard) -> InboxDraft {
    InboxDraft {
        kind: InboxKind::Slack,
        text: card.summary.clone(),
        source: SourceReference::SlackThread {
            workspace: card.key.workspace.0.clone(),
            channel: card.key.channel.0.clone(),
            thread_ts: card.key.thread_ts.0.clone(),
            latest_ts: card.verdict_key.0.clone(),
        },
        context: CapturedContext {
            host: None,
            room: Some(card.conversation.clone()),
            focused_surface: String::new(),
        },
        // The dealer reads this as "the ball is in their court"; a thread
        // waiting on them is only ever an item because the user has not yet
        // given it a verdict.
        waiting_on: match card.waiting {
            Waiting::OnThem => Some(card.conversation.clone()),
            Waiting::OnYou => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use rho_slack::api::parse_message;
    use rho_slack::types::{Conversation, ConversationKind, UserId};
    use serde_json::json;

    use super::*;

    fn model() -> Model {
        let mut model = Model::new(WorkspaceName("acme".into()));
        model.set_self(UserId("ME".into()));
        model.add_conversations([Conversation {
            id: ChannelId("C1".into()),
            kind: ConversationKind::Channel,
            name: "design".into(),
            user: None,
        }]);
        model
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
    fn a_mention_becomes_an_item_and_your_reply_retires_it() {
        let mut model = model();
        let mut store = InboxStore::memory();
        let mut items = SlackItems::default();

        let change = model
            .note_message(&message("100.0", None, "U1", "hey <@ME> look"), 0)
            .expect("a mention raises");
        items.apply(&mut store, &model, &change, 0).unwrap();
        assert_eq!(store.items().len(), 1);
        let item = &store.items()[0];
        assert_eq!(item.kind, InboxKind::Slack);
        assert_eq!(item.text, "hey @you look");
        assert_eq!(item.context.room.as_deref(), Some("#design"));
        assert_eq!(item.waiting_on, None, "the mention waits on you");
        assert!(matches!(
            &item.source,
            SourceReference::SlackThread { channel, thread_ts, latest_ts, .. }
                if channel == "C1" && thread_ts == "100.0" && latest_ts == "100.0"
        ));

        // A later reply from them updates the same item, and moves the
        // verdict key so a skip on the old card cannot hide the new one.
        let change = model
            .note_message(&message("101.0", Some("100.0"), "U1", "still stuck"), 0)
            .expect("a newer reply updates");
        items.apply(&mut store, &model, &change, 0).unwrap();
        assert_eq!(store.items().len(), 1, "one thread is one item");
        assert!(matches!(
            &store.items()[0].source,
            SourceReference::SlackThread { latest_ts, .. } if latest_ts == "101.0"
        ));

        let change = model
            .note_message(&message("102.0", Some("100.0"), "ME", "on it"), 0)
            .expect("your own reply quiets");
        items.apply(&mut store, &model, &change, 0).unwrap();
        assert!(
            store.items().is_empty(),
            "answering is the done verdict; nothing is left to file"
        );
    }

    #[test]
    fn a_restart_adopts_the_items_it_already_raised() {
        let mut model = model();
        let mut store = InboxStore::memory();
        let mut items = SlackItems::default();
        let change = model
            .note_message(&message("200.0", None, "U1", "<@ME> ping"), 0)
            .unwrap();
        let id = items
            .apply(&mut store, &model, &change, 0)
            .unwrap()
            .unwrap();

        let mut adopted = SlackItems::default();
        adopted.adopt(&store, &WorkspaceName("acme".into()));
        let key = model.key(&ChannelId("C1".into()), &Ts("200.0".into()));
        assert_eq!(adopted.item(&key), Some(&id));

        let change = model
            .note_message(&message("201.0", Some("200.0"), "U1", "still?"), 0)
            .unwrap();
        adopted.apply(&mut store, &model, &change, 0).unwrap();
        assert_eq!(
            store.items().len(),
            1,
            "a restart must not raise the thread twice"
        );
    }
}

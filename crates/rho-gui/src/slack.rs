//! Slack in the GUI: registering workspaces, and (from here on) the live
//! session, the items it raises, and the thread surface.
//!
//! The Slack client itself lives in `rho-slack`; this module is only the
//! seam where it meets the workspace, the inbox, and the journal.

use gpui::AppContext as _;
use rho_slack::config::{CredentialStore, Credentials, WorkspaceName};
use rho_slack::health::Signal;
use rho_slack::model::{Change, Model, Waiting};
use rho_slack::session::{Session, SessionEvent, Source};
use rho_slack::types::{ChannelId, ThreadKey, Ts};

use crate::dashboard::{DealerThread, ThreadRef};
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

    /// Where a Slack card's message lives. A thread only when someone has
    /// replied under the root: a mention in a channel with nothing under it
    /// is a channel message, and opening a one-message "thread" for it would
    /// hide the room it was said in.
    pub(crate) fn slack_deal_source(
        workspace: &str,
        channel: &str,
        thread_ts: &str,
        latest_ts: &str,
    ) -> Source {
        if latest_ts == thread_ts {
            return Source::Conversation(ChannelId(channel.to_owned()));
        }
        Source::Thread(ThreadKey {
            workspace: WorkspaceName(workspace.to_owned()),
            channel: ChannelId(channel.to_owned()),
            thread_ts: Ts(thread_ts.to_owned()),
        })
    }

    /// Opens the conversation a Slack card is about and puts the reader on
    /// the message that raised it: the deal is the conversation, so the
    /// surface is the ordinary one, opened the ordinary way.
    pub(crate) fn open_slack_deal(
        &mut self,
        workspace: &str,
        channel: &str,
        thread_ts: &str,
        latest_ts: &str,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        if self.slack_session(cx).is_none() {
            return false;
        }
        let source = Self::slack_deal_source(workspace, channel, thread_ts, latest_ts);
        self.open_slack_source(source, window, cx);
        let SurfaceView::SlackConversation(view) = &self.active_pane().surface.view else {
            return false;
        };
        let latest = Ts(latest_ts.to_owned());
        view.clone()
            .update(cx, |view, cx| view.reveal(latest, window, cx));
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
                self._slack_view_subscriptions.push(cx.subscribe(
                    &view,
                    |workspace, view, event, cx| match event {
                        rho_slack::ui::conversation::Event::OpenFile(file) => {
                            workspace.open_slack_image(view, file.clone(), cx);
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
        self.display_surface_with_method(surface, crate::journal::SurfaceShowMethod::Command, cx);
    }

    /// `enter` on a list row, or on a message: the row's conversation, or
    /// the thread the message is in.
    pub(crate) fn slack_open_row(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        // A file line is a file: the reader who put the cursor there asked
        // for the attachment, not for the thread it hangs under.
        if let SurfaceView::SlackConversation(view) = &self.active_pane().surface.view {
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
        if let SurfaceView::SlackConversation(view) = &self.active_pane().surface.view {
            let view = view.clone();
            let link = view.update(cx, |view, cx| view.cursor_link(cx));
            if let Some(link) = link {
                self.create_browser_page(link, None, window, cx);
                return;
            }
        }
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
                        crate::style::StyleClass::SystemInfo,
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
                let view = cx.new(|cx| crate::image_view::ImageView::new(path, cx));
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
        // The roster can land after a surface was opened, so every cached
        // title is re-read from the model: a conversation is never left
        // reading "#a conversation" once its people are known.
        for source in self.slack_labels.keys().cloned().collect::<Vec<_>>() {
            let label = session.read(cx).label(&source);
            self.slack_labels.insert(source, label);
        }
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
                for change in changes {
                    // A thread that starts to matter becomes a node, and the
                    // dealer deals the node. The daemon answers with the node
                    // it already had when the thread was raised before, so a
                    // reconnect never makes a second one.
                    if let Change::Raised(key) | Change::Updated(key) = change {
                        self.bind_slack_thread(key, cx);
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

impl Workspace {
    /// Asks the daemon for this thread's node. Nothing is stored here: the
    /// tree holds the thread, and the daemon's answer is idempotent.
    pub(crate) fn bind_slack_thread(&mut self, key: &ThreadKey, cx: &mut gpui::Context<Self>) {
        let Some(host) = self.hosts.primary() else {
            return;
        };
        // A thread already dealt as an open node needs nothing; one that a
        // verdict quieted is re-bound, and the daemon reopens it.
        if self
            .dashboard
            .thread_card_id(&thread_ref(key))
            .is_some_and(|card| self.dashboard.node_is_open(card))
        {
            return;
        }
        let conversation = self.slack.as_ref().map_or_else(String::new, |session| {
            session.read(cx).model().label(&key.channel)
        });
        let request_id = self.next_binding_request_id;
        self.next_binding_request_id = self.next_binding_request_id.wrapping_add(1);
        self.pending_bindings.insert(
            (host, request_id),
            crate::workspace::PendingBinding::SlackThread {
                thread: crate::journal::SlackThread {
                    workspace: key.workspace.0.clone(),
                    conversation,
                    thread: key.thread_ts.0.clone(),
                },
            },
        );
        self.send_to_host(
            host,
            rho_ui_proto::ClientMessage::DeskThreadBind {
                request_id,
                parent: None,
                workspace: key.workspace.0.clone(),
                channel: key.channel.0.clone(),
                thread_ts: key.thread_ts.0.clone(),
            },
        );
    }

    /// What every tracked thread is currently about. The dealer reads this
    /// live from the mirror rather than storing any of it in the tree.
    pub(crate) fn slack_thread_facts(
        &self,
        cx: &gpui::App,
    ) -> std::collections::HashMap<ThreadRef, DealerThread> {
        let Some(session) = self.slack.as_ref() else {
            return std::collections::HashMap::new();
        };
        let now = chrono::Local::now();
        let model = session.read(cx).model();
        model
            .tracked()
            .into_iter()
            .filter_map(|key| {
                let card = model.card(&key, now.timestamp_millis())?;
                let thread = model.thread(&key)?;
                let raised_at = chrono::DateTime::from_timestamp_millis(thread.first_seen_ms)?
                    .with_timezone(&now.timezone())
                    .fixed_offset();
                Some((
                    thread_ref(&key),
                    DealerThread {
                        title: card.summary,
                        conversation: card.conversation.clone(),
                        raised_at,
                        waiting_on: match card.waiting {
                            Waiting::OnThem => Some(card.conversation),
                            Waiting::OnYou => None,
                        },
                        latest: card.verdict_key.0,
                    },
                ))
            })
            .collect()
    }
}

pub(crate) fn thread_ref(key: &ThreadKey) -> ThreadRef {
    ThreadRef {
        workspace: key.workspace.0.clone(),
        channel: key.channel.0.clone(),
        thread_ts: key.thread_ts.0.clone(),
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
            members: Vec::new(),
        }]);
        model.add_users([rho_slack::types::User {
            id: UserId("ME".into()),
            name: "Manmeet".into(),
            handle: "manmeet".into(),
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
    fn a_mention_and_its_follow_ups_name_one_thread_to_bind() {
        // The tree holds one node per thread, so every change the client
        // acts on has to name the same thread: the mention that raised it
        // and the reply that keeps it alive are one `thread` node.
        let mut model = model();
        let raised = model
            .note_message(&message("100.0", None, "U1", "hey <@ME> look"), 0)
            .expect("a mention raises");
        let updated = model
            .note_message(&message("101.0", Some("100.0"), "U1", "still stuck"), 0)
            .expect("a newer reply updates");
        let binds = [raised, updated]
            .iter()
            .filter_map(|change| match change {
                Change::Raised(key) | Change::Updated(key) => Some(thread_ref(key)),
                Change::Quieted(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(binds.len(), 2, "a raise and an update both bind");
        assert_eq!(binds[0], binds[1], "one thread is one node");
        assert_eq!(binds[0].thread_ts, "100.0");

        // Your own reply is the done verdict; nothing is bound for it.
        let quieted = model
            .note_message(&message("102.0", Some("100.0"), "ME", "on it"), 0)
            .expect("your own reply quiets");
        assert!(matches!(quieted, Change::Quieted(_)));
    }

    #[test]
    fn a_slack_deal_opens_the_thread_only_when_the_reply_is_in_one() {
        // The card carries the message that raised it. A card whose latest
        // message is the root is a channel message with no thread under it,
        // so the deal opens the channel, not a thread of one.
        assert!(matches!(
            Workspace::slack_deal_source("acme", "C1", "500.0", "500.0"),
            Source::Conversation(ChannelId(channel)) if channel == "C1"
        ));
        let Source::Thread(key) = Workspace::slack_deal_source("acme", "C1", "500.0", "700.0")
        else {
            panic!("a reply under a root must deal in its thread");
        };
        assert_eq!(key.channel.0, "C1");
        assert_eq!(key.thread_ts.0, "500.0");
    }
}

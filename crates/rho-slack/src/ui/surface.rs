//! One Slack screen: one conversation or thread, or the results of a
//! search. Each is a view from this crate wrapped as a frame surface;
//! the wrapper holds the keys that reach outside the view, into the frame.

use std::rc::Rc;

use editor::Editor;
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, Subscription, Window, div};
use rho_dealer::Address;
use rho_frame::thread::{FocusComposer, FocusLog, SendMessage};
use rho_frame::{Candidate, Frame, FrameLink, Row, Rows, Subject, Surface};
use rho_window::style::StyleClass;

use super::conversation::{self, Attaching, Submitted};
use super::results::{self, Place, Target};
use super::system::{
    self, FindFile, FindMessage, NewMessage, NextPage, NextUnread, OpenRow, PreviousPage,
    SlackSystem,
};
use super::{ConversationView, ReactionChoices, ResultsView, SearchKind};
use crate::address::Listing;
use crate::session::{FoundPage, Session, SessionEvent, Source};
use crate::types::{FileSummary, ThreadKey, Ts};

/// The message a rewrite cannot carry a file past.
const NOT_WHILE_EDITING: &str =
    "slack: a rewrite cannot carry a picture; finish or leave the edit first";

/// The message a rewrite was open on has been deleted from somewhere else.
const REWRITE_LOST: &str = "slack: that message was deleted; your rewrite is in the composer";

/// The keys a reaction menu offers its emoji under: the home row first.
const REACTION_KEYS: [&str; 12] = ["a", "s", "d", "f", "g", "h", "j", "k", "l", "q", "w", "e"];

pub enum View {
    Conversation(Entity<ConversationView>),
    Results {
        view: Entity<ResultsView>,
        listing: Listing,
        /// The query this listing is waiting on the answer to, for a
        /// search; an answer to any other is someone else's.
        asked: Option<(String, SearchKind)>,
    },
}

pub struct SlackSurface {
    system: Entity<SlackSystem>,
    frame: FrameLink,
    session: Entity<Session>,
    view: View,
    _subscriptions: Vec<Subscription>,
}

/// The Slack surface the frame holds at `address`, if it has one.
pub(super) fn surface_at(frame: &Frame, address: &Address) -> Option<Entity<SlackSurface>> {
    frame
        .surface(address)?
        .view()
        .downcast::<SlackSurface>()
        .ok()
}

impl SlackSurface {
    /// A conversation or a thread, landed on `land` when it is asking for
    /// the reader.
    pub(super) fn conversation(
        system: Entity<SlackSystem>,
        frame: FrameLink,
        session: Entity<Session>,
        source: Source,
        land: Option<Ts>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = cx.new(|cx| {
            let mut view =
                ConversationView::new(session.clone(), source, SlackSystem::hooks(), window, cx);
            if let Some(ts) = land {
                view.reveal(ts, window, cx);
            }
            view
        });
        let subscription = cx.subscribe_in(&view, window, Self::conversation_event);
        Self {
            system,
            frame,
            session,
            view: View::Conversation(view),
            _subscriptions: vec![subscription],
        }
    }

    pub(super) fn listing(
        system: Entity<SlackSystem>,
        frame: FrameLink,
        session: Entity<Session>,
        listing: Listing,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = cx.new(|cx| ResultsView::new(session.clone(), SlackSystem::hooks(), window, cx));
        let subscriptions = vec![
            cx.subscribe_in(&view, window, |this, _, event, window, cx| {
                let target = match event {
                    results::Event::Open(place) => Target::Message(place.clone()),
                    results::Event::OpenFile(file) => Target::File(file.clone()),
                };
                this.open_target(target, window, cx);
            }),
            cx.subscribe_in(&session, window, Self::session_event),
        ];
        Self {
            system,
            frame,
            session,
            view: View::Results {
                view,
                listing,
                asked: None,
            },
            _subscriptions: subscriptions,
        }
    }

    /// Asks Slack, drawing that it is asking until the answer comes.
    pub(super) fn ask(
        &mut self,
        query: &str,
        kind: SearchKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let View::Results { view, asked, .. } = &mut self.view else {
            return;
        };
        *asked = Some((query.to_owned(), kind));
        view.update(cx, |view, cx| view.asking(query, kind, window, cx));
        self.session.update(cx, |session, cx| match kind {
            SearchKind::Messages => session.search(query, 1, cx),
            SearchKind::Files => session.search_files(query, 1, cx),
        });
    }

    fn session_event(
        &mut self,
        _: &Entity<Session>,
        event: &SessionEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let SessionEvent::Found(found) = event else {
            return;
        };
        let View::Results {
            view,
            asked: Some((query, kind)),
            ..
        } = &self.view
        else {
            return;
        };
        if (&found.query, found.kind) != (query, *kind) {
            return;
        }
        view.update(cx, |view, cx| match &found.page {
            Ok(FoundPage::Messages(page)) => view.found(&found.query, page, window, cx),
            Ok(FoundPage::Files(page)) => view.found_files(&found.query, page, window, cx),
            Err(why) => view.refused(&found.query, why, window, cx),
        });
    }

    fn conversation_event(
        &mut self,
        view: &Entity<ConversationView>,
        event: &conversation::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let echo = |text: &str, class, cx: &mut App| self.frame.echo(text.to_owned(), class, cx);
        match event {
            conversation::Event::OpenFile(file) => {
                view.update(cx, |view, cx| view.open_file(file.clone(), cx))
            }
            conversation::Event::AttachRefused => {
                echo(NOT_WHILE_EDITING, StyleClass::SystemInfo, cx)
            }
            conversation::Event::RewriteLost => echo(REWRITE_LOST, StyleClass::SystemInfo, cx),
            conversation::Event::AttachFailed(said) => {
                echo(&format!("slack: {said}"), StyleClass::SystemImportant, cx)
            }
            conversation::Event::BroadcastWithFilesUnsupported => echo(
                "slack: files cannot also be sent to the channel",
                StyleClass::SystemImportant,
                cx,
            ),
            conversation::Event::ActivateRequested => self.open_row(&OpenRow, window, cx),
        }
    }

    fn conversation_view(&self) -> Option<&Entity<ConversationView>> {
        match &self.view {
            View::Conversation(view) => Some(view),
            _ => None,
        }
    }

    fn open_source(&self, source: &Source, cx: &mut App) {
        let address = system::address_of(self.session.read(cx).model(), source);
        self.frame.open(address, cx);
    }

    /// Opens a hit: the message in its conversation, or the file.
    fn open_target(&mut self, target: Target, _window: &mut Window, cx: &mut Context<Self>) {
        match target {
            Target::Message(Place { source, ts }) => {
                let address = system::address_of(self.session.read(cx).model(), &source);
                system::open_then(&self.frame, address, cx, move |surface, window, cx| {
                    if let Some(view) = surface.conversation_view() {
                        view.update(cx, |view, cx| view.reveal_found(ts, window, cx));
                    }
                });
            }
            Target::File(file) => self.open_file(&file, cx),
        }
    }

    fn open_file(&self, file: &FileSummary, cx: &mut App) {
        self.session
            .update(cx, |session, cx| session.open_file(file, cx));
    }

    /// `enter`: whatever the cursor is on, most specific first. A preview
    /// unfolds, an app's control runs, a file or link opens, and otherwise
    /// the thread the line is in.
    fn open_row(&mut self, _: &OpenRow, window: &mut Window, cx: &mut Context<Self>) {
        self.open_line(window, cx)
    }

    fn open_thread_row(
        &mut self,
        _: &rho_frame::thread::OpenRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_line(window, cx)
    }

    fn open_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.view {
            View::Results { view, .. } => {
                if let Some(target) = view.read(cx).cursor_target(cx) {
                    self.open_target(target, window, cx);
                }
            }
            View::Conversation(view) => {
                let view = view.clone();
                if view.update(cx, |view, cx| view.cursor_on_crumb(cx)) {
                    let channel = view.read(cx).source().channel().clone();
                    self.open_source(&Source::Conversation(channel), cx);
                    return;
                }
                if view.update(cx, |view, cx| view.toggle_cursor_preview(window, cx)) {
                    return;
                }
                if let Some(interaction) = view.update(cx, |view, cx| view.cursor_interaction(cx)) {
                    let action = &interaction.action;
                    match (action.element_type.as_str(), action.payload["url"].as_str()) {
                        ("button", Some(url)) => cx.open_url(url),
                        ("button", None) if action.confirmation.is_none() => {
                            view.update(cx, |view, cx| view.run_interaction(interaction, None, cx))
                        }
                        (kind, _) => self.frame.echo(
                            format!("slack: {kind} app controls are not supported here yet"),
                            StyleClass::SystemInfo,
                            cx,
                        ),
                    }
                    return;
                }
                if let Some(file) = view.update(cx, |view, cx| view.cursor_file(cx)) {
                    view.update(cx, |view, cx| view.open_file(file, cx));
                    return;
                }
                if let Some(link) = view.update(cx, |view, cx| view.cursor_link(cx)) {
                    // A link to a message in this workspace opens it here.
                    let archive = archive_target(self.session.read(cx).model(), &link);
                    match archive {
                        Some(target) => self.open_target(target, window, cx),
                        None => cx.open_url(&link),
                    }
                    return;
                }
                if let Some(key) = view.update(cx, |view, cx| view.cursor_thread(cx)) {
                    self.open_source(&Source::Thread(key), cx);
                }
            }
        }
    }

    pub(super) fn compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.conversation_view() {
            view.update(cx, |view, cx| view.focus_composer(window, cx));
        }
    }

    fn focus_composer(&mut self, _: &FocusComposer, window: &mut Window, cx: &mut Context<Self>) {
        self.compose(window, cx)
    }

    /// Back to the messages. A rewrite open in the box is left, and the
    /// box holds again what it held before.
    fn focus_log(&mut self, _: &FocusLog, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.conversation_view().cloned() else {
            return cx.propagate();
        };
        view.update(cx, |view, cx| {
            view.cancel_edit(cx);
            view.focus_log(window, cx);
        });
    }

    fn send(&mut self, _: &SendMessage, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.conversation_view().cloned() else {
            return;
        };
        let channel = view.read(cx).source().channel().clone();
        let broadcast = view.read(cx).also_send_to_channel();
        let submitting = view.update(cx, |view, cx| view.send(broadcast, window, cx));
        let session = self.session.downgrade();
        cx.spawn(async move |_, cx| {
            let submitted = submitting.await;
            let Some(conversation) = session
                .read_with(cx, |session, _| session.model().label(&channel))
                .ok()
            else {
                return;
            };
            // What the reader did, not what was tried: a send is journaled
            // by the session, and a refusal is not theirs.
            match submitted {
                Submitted::Edited(ts) => {
                    rho_journal::record(rho_journal::Event::SlackMessageEdited {
                        conversation,
                        ts: ts.0,
                    })
                }
                Submitted::FileSent(bytes) => {
                    rho_journal::record(rho_journal::Event::SlackFileSent {
                        conversation,
                        bytes,
                    })
                }
                Submitted::Sent | Submitted::Refused | Submitted::Nothing | Submitted::Sending => {}
            }
        })
        .detach();
    }

    fn next_unread(&mut self, _: &NextUnread, _window: &mut Window, cx: &mut Context<Self>) {
        let here = self
            .conversation_view()
            .map(|view| view.read(cx).source().channel().clone());
        let session = self.session.clone();
        self.frame.with(cx, move |frame, window, cx| {
            system::next_unread(&session, here.as_ref(), frame, window, cx)
        });
    }

    fn find_message(&mut self, _: &FindMessage, _window: &mut Window, cx: &mut Context<Self>) {
        self.find(SearchKind::Messages, cx)
    }

    fn find_file(&mut self, _: &FindFile, _window: &mut Window, cx: &mut Context<Self>) {
        self.find(SearchKind::Files, cx)
    }

    /// Asks Slack, inside the conversation the reader is in when they are
    /// in one, and everywhere otherwise.
    fn find(&self, kind: SearchKind, cx: &mut App) {
        let scope = self.conversation_view().and_then(|view| {
            let channel = view.read(cx).source().channel().clone();
            let model = self.session.read(cx).model();
            Some((model.search_scope(&channel)?, model.label(&channel)))
        });
        let session = self.session.clone();
        self.frame.with(cx, move |frame, window, cx| {
            system::prompt_find(session, scope, kind, frame, window, cx)
        });
    }

    fn new_message(&mut self, _: &NewMessage, _window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session.clone();
        self.frame.with(cx, move |frame, window, cx| {
            system::prompt_people(session, frame, window, cx)
        });
    }

    fn page(&mut self, offset: i32, window: &mut Window, cx: &mut Context<Self>) {
        let View::Results { view, .. } = &self.view else {
            return;
        };
        if !view.update(cx, |view, cx| view.request_adjacent(offset, window, cx)) {
            self.frame
                .echo("slack: no more pages that way", StyleClass::SystemInfo, cx);
        }
    }

    fn previous_page(&mut self, _: &PreviousPage, window: &mut Window, cx: &mut Context<Self>) {
        self.page(-1, window, cx)
    }

    fn next_page(&mut self, _: &NextPage, window: &mut Window, cx: &mut Context<Self>) {
        self.page(1, window, cx)
    }

    /// The rows only a conversation has: what is under the cursor, and the
    /// composer's options.
    fn conversation_rows(&self, view: &Entity<ConversationView>, cx: &mut App) -> Vec<Row> {
        let source = view.read(cx).source().clone();
        let channel = source.channel().clone();
        let mut rows = Vec::new();
        if let Some(actions) = view.update(cx, |view, cx| view.message_actions(cx)) {
            rows.push(Row::menu(
                "e",
                "message actions…",
                message_menu(view, &self.frame, &actions),
            ));
        }
        let broadcast = view.read(cx).also_send_to_channel();
        if matches!(source, Source::Thread(_)) {
            let view = view.clone();
            rows.push(Row::run(
                "b",
                match broadcast {
                    true => "broadcast reply: on",
                    false => "broadcast reply: off",
                },
                move |frame, _, cx| {
                    view.update(cx, |view, cx| view.set_also_send_to_channel(!broadcast, cx));
                    frame.echo(
                        match broadcast {
                            true => "slack: replies stay in the thread",
                            false => "slack: replies also go to the channel",
                        },
                        StyleClass::SystemInfo,
                        cx,
                    );
                },
            ));
        }
        {
            let session = self.session.clone();
            let starred = session.read(cx).favorite(&channel);
            rows.push(Row::run(
                "z",
                match starred {
                    true => "unstar conversation",
                    false => "star conversation",
                },
                move |frame, _, cx| {
                    session.update(cx, |session, cx| session.toggle_favorite(&channel, cx));
                    frame.echo(
                        match starred {
                            true => "slack: conversation unstarred",
                            false => "slack: conversation starred",
                        },
                        StyleClass::SystemInfo,
                        cx,
                    );
                },
            ));
        }
        if let Source::Thread(key) = &source {
            rows.push(follow_row(&self.session, key, cx));
        }
        {
            let view = view.clone();
            rows.push(Row::run("a", "attach file…", move |frame, window, cx| {
                prompt_attach(view.clone(), frame, window, cx)
            }));
        }
        if !view.read(cx).attachments().is_empty() {
            let view = view.clone();
            rows.push(Row::run(
                "x",
                "remove attachment…",
                move |frame, window, cx| prompt_detach(view.clone(), frame, window, cx),
            ));
        }
        rows
    }
}

/// Follows a thread rho does not, or lets go of one it does: Slack's own
/// subscription, so every client agrees.
fn follow_row(session: &Entity<Session>, key: &ThreadKey, cx: &App) -> Row {
    let follows = session.read(cx).model().follows(key);
    let (session, key) = (session.clone(), key.clone());
    Row::run(
        "t",
        match follows {
            true => "unfollow thread",
            false => "follow thread",
        },
        move |frame, _, cx| {
            session.update(cx, |session, cx| match follows {
                true => session.ignore_thread(&key, cx),
                false => session.follow_thread(&key, cx),
            });
            frame.echo(
                match follows {
                    true => "slack: thread unfollowed",
                    false => "slack: thread followed",
                },
                StyleClass::SystemInfo,
                cx,
            );
        },
    )
}

/// `react…`: the emoji already on the message, then the reader's own, then
/// any by name.
fn react_menu(view: &Entity<ConversationView>, choices: &ReactionChoices) -> Rows {
    let mut menu = Rows::new("react");
    let offered = choices.on_message.iter().chain(&choices.recent);
    for (key, choice) in REACTION_KEYS.iter().zip(offered) {
        let description = match choice.mine {
            true => format!("{} {} — remove", choice.glyph, choice.name),
            false => format!("{} {}", choice.glyph, choice.name),
        };
        let (view, ts, name) = (view.clone(), choices.ts.clone(), choice.name.clone());
        menu = menu.row(Row::run(*key, description, move |_, _, cx| {
            view.update(cx, |view, cx| view.react(&ts, &name, cx))
        }));
    }
    let (view, ts) = (view.clone(), choices.ts.clone());
    menu.row(Row::run("/", "by name…", move |frame, window, cx| {
        prompt_react(view.clone(), ts.clone(), frame, window, cx)
    }))
}

/// Any emoji, by name: the table the composer completes `:` from.
fn prompt_react(
    view: Entity<ConversationView>,
    ts: Ts,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    let reading = view.clone();
    frame.open_prompt(
        "react:",
        Rc::new(move |_, typed, cx| {
            let view = reading.read(cx);
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
        Rc::new(move |_, input, _, cx| {
            let name = input.trim().trim_matches(':').to_owned();
            if !name.is_empty() {
                view.update(cx, |view, cx| view.react(&ts, &name, cx));
            }
        }),
        window,
        cx,
    );
}

/// What can be done to the message under the cursor.
fn message_menu(
    view: &Entity<ConversationView>,
    frame: &FrameLink,
    actions: &conversation::MessageActions,
) -> Rows {
    let ts = actions.ts.clone();
    let mut menu = Rows::new("message");
    let answer = |frame: FrameLink, done: &'static str| {
        move |result: anyhow::Result<()>, cx: &mut App| match result {
            Ok(()) => frame.echo(done, StyleClass::SystemInfo, cx),
            Err(error) => frame.echo(format!("slack: {error:#}"), StyleClass::SystemImportant, cx),
        }
    };
    if actions.can_edit {
        let (view, ts) = (view.clone(), ts.clone());
        menu = menu.row(Row::run("e", "edit", move |_, window, cx| {
            view.update(cx, |view, cx| view.start_edit_at(&ts, window, cx));
        }));
    }
    {
        let (view, ts) = (view.clone(), ts.clone());
        menu = menu.row(Row::run("r", "react…", move |frame, window, cx| {
            if let Some(choices) = view.update(cx, |view, cx| view.reaction_choices_for(&ts, cx)) {
                frame.open_menu(react_menu(&view, &choices), window, cx);
            }
        }));
    }
    if actions.can_copy_link {
        let (view, ts, link) = (view.clone(), ts.clone(), frame.clone());
        menu = menu.row(Row::run("c", "copy link", move |_, _, cx| {
            let linking = view.update(cx, |view, cx| view.message_link(ts.clone(), cx));
            let link = link.clone();
            cx.spawn(async move |_, cx| {
                let linked = linking.await;
                cx.update(|cx| match linked {
                    Ok(url) => {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(url));
                        link.echo("slack: message link copied", StyleClass::SystemInfo, cx);
                    }
                    Err(error) => {
                        link.echo(format!("slack: {error:#}"), StyleClass::SystemImportant, cx)
                    }
                })
            })
            .detach();
        }));
    }
    if actions.can_forward {
        let (view, ts, link) = (view.clone(), ts.clone(), frame.clone());
        menu = menu.row(Row::run("f", "forward…", move |frame, window, cx| {
            let session = view.read(cx).session().clone();
            let (view, ts, link) = (view.clone(), ts.clone(), link.clone());
            let reading = session.clone();
            frame.open_prompt(
                "forward to:",
                Rc::new(move |_, needle, cx| {
                    let needle = needle.to_lowercase();
                    reading
                        .read(cx)
                        .rows()
                        .into_iter()
                        .filter(|row| row.label.to_lowercase().contains(&needle))
                        .map(|row| Candidate {
                            value: row.label,
                            description: row.id.0,
                        })
                        .collect()
                }),
                Rc::new(move |frame, input, _, cx| {
                    let destination = session
                        .read(cx)
                        .rows()
                        .into_iter()
                        .find(|row| row.label == input.trim() || row.id.0 == input.trim())
                        .map(|row| row.id);
                    let Some(destination) = destination else {
                        frame.echo("slack: choose a conversation", StyleClass::SystemInfo, cx);
                        return;
                    };
                    let forwarding = view.update(cx, |view, cx| {
                        view.forward_message(ts.clone(), destination, cx)
                    });
                    let said = answer(link.clone(), "slack: message forwarded");
                    cx.spawn(async move |_, cx| {
                        let forwarded = forwarding.await;
                        cx.update(|cx| said(forwarded, cx))
                    })
                    .detach();
                }),
                window,
                cx,
            );
        }));
    }
    if actions.can_delete {
        let (view, ts, link) = (view.clone(), ts.clone(), frame.clone());
        menu = menu.row(Row::run("d", "delete…", move |frame, window, cx| {
            let (view, ts, link) = (view.clone(), ts.clone(), link.clone());
            frame.open_prompt(
                "delete message? type delete:",
                Rc::new(|_, typed, _| {
                    vec![Candidate {
                        value: "delete".to_owned(),
                        description: match typed.trim() == "delete" {
                            true => "enter permanently deletes your message".to_owned(),
                            false => "confirmation required".to_owned(),
                        },
                    }]
                }),
                Rc::new(move |frame, input, _, cx| {
                    if input.trim() != "delete" {
                        frame.echo("slack: message not deleted", StyleClass::SystemInfo, cx);
                        return;
                    }
                    let deleting = view.update(cx, |view, cx| view.delete_message(ts.clone(), cx));
                    let said = answer(link.clone(), "slack: message deleted");
                    cx.spawn(async move |_, cx| {
                        let deleted = deleting.await;
                        cx.update(|cx| said(deleted, cx))
                    })
                    .detach();
                }),
                window,
                cx,
            );
        }));
    }
    let (view, link) = (view.clone(), frame.clone());
    menu.row(Row::run("u", "mark unread", move |_, _, cx| {
        view.update(cx, |view, cx| view.mark_unread_from(&ts, cx));
        link.echo("slack: marked unread", StyleClass::SystemInfo, cx);
    }))
}

/// A file from disk, to go with the next message.
fn prompt_attach(
    view: Entity<ConversationView>,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    frame.open_prompt(
        "attach file:",
        Rc::new(|_, needle, _| {
            let path = std::path::Path::new(needle.trim());
            vec![Candidate {
                value: needle.trim().to_owned(),
                description: match path.is_file() {
                    true => "enter attaches it".to_owned(),
                    false => "a path to a file".to_owned(),
                },
            }]
        }),
        Rc::new(move |frame, input, _, cx| {
            let path = std::path::PathBuf::from(input.trim());
            let said = match view.update(cx, |view, cx| view.attach_path(&path, cx)) {
                Ok(Attaching::Attached | Attaching::Replaced) => {
                    format!("slack: {} attached", path.display())
                }
                Ok(Attaching::NotWhileEditing) => NOT_WHILE_EDITING.to_owned(),
                Ok(Attaching::Sending) => "slack: wait for the current send to finish".to_owned(),
                Ok(Attaching::TooMany) => format!(
                    "slack: at most {} files can be attached",
                    conversation::MAX_ATTACHMENTS
                ),
                Ok(Attaching::TooLarge) => "slack: attachment limit exceeded".to_owned(),
                // A path that cannot be read is worth saying now rather than
                // at send time.
                Err(error) => format!("slack: {error:#}"),
            };
            frame.echo(&said, StyleClass::SystemInfo, cx);
        }),
        window,
        cx,
    );
}

/// One of the waiting files, taken off the next message.
fn prompt_detach(
    view: Entity<ConversationView>,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    let files = Rc::new(view.read(cx).attachments());
    let offered = files.clone();
    frame.open_prompt(
        "remove attachment:",
        Rc::new(move |_, _, _| {
            offered
                .iter()
                .enumerate()
                .map(|(index, file)| Candidate {
                    value: (index + 1).to_string(),
                    description: file.name.clone(),
                })
                .collect()
        }),
        Rc::new(move |frame, input, _, cx| {
            let Some(index) = input
                .trim()
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
            else {
                return;
            };
            if view.update(cx, |view, cx| view.remove_attachment(index, cx)) {
                frame.echo("slack: attachment removed", StyleClass::SystemInfo, cx);
            }
        }),
        window,
        cx,
    );
}

/// A link to a message in this workspace, as the place it is.
fn archive_target(model: &crate::model::Model, link: &str) -> Option<Target> {
    let archive = model.archive_link(link)?;
    let source = match archive.thread_ts {
        Some(thread_ts) => Source::Thread(ThreadKey {
            workspace: model.workspace().clone(),
            channel: archive.channel,
            thread_ts,
        }),
        None => Source::Conversation(archive.channel),
    };
    Some(Target::Message(Place {
        source,
        ts: archive.ts,
    }))
}

impl Surface for SlackSurface {
    fn title(&self, cx: &App) -> String {
        let session = self.session.read(cx);
        match &self.view {
            View::Conversation(view) => session.label(view.read(cx).source()),
            View::Results { listing, asked, .. } => match (listing, asked) {
                (_, Some((query, _))) => format!("slack: {query}"),
                (Listing::Messages, None) => "slack messages".to_owned(),
                (Listing::Files, None) => "slack files".to_owned(),
            },
        }
    }

    fn editor(&self, cx: &App) -> Option<Entity<Editor>> {
        Some(match &self.view {
            View::Conversation(view) => view.read(cx).focused_editor().clone(),
            View::Results { view, .. } => view.read(cx).editor().clone(),
        })
    }

    /// The conversation under the cursor in the list, the place under it
    /// in a listing, and the conversation or thread itself when in one.
    fn subject(&self, cx: &App) -> Subject {
        let model = self.session.read(cx).model();
        match &self.view {
            View::Conversation(_) => Subject::Own,
            View::Results { view, .. } => match view.read(cx).cursor_target(cx) {
                Some(Target::Message(place)) => system::subject_of(model, Some(place.source)),
                _ => Subject::None,
            },
        }
    }

    /// The `slack` menu, with what this screen adds to it.
    fn rows(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        let mut rows = Vec::new();
        let mut menu = Rows::new("slack");
        if let Some(view) = self.conversation_view().cloned() {
            // The thread's own row, as every thread has it.
            let this = cx.entity();
            rows.push(Row::run("ctrl-enter", "send", move |_, window, cx| {
                this.update(cx, |surface, cx| surface.send(&SendMessage, window, cx));
            }));
            menu.extend(self.conversation_rows(&view, cx));
        }
        menu.extend(SlackSystem::menu_rows(
            &self.system,
            Some(self.session.clone()),
        ));
        rows.push(Row::menu("s", "slack", menu));
        rows
    }
}

impl Focusable for SlackSurface {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor(cx)
            .expect("every Slack surface is an editor")
            .focus_handle(cx)
    }
}

impl Render for SlackSurface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let inner = match &self.view {
            View::Conversation(view) => view.clone().into_any_element(),
            View::Results { view, .. } => view.clone().into_any_element(),
        };
        div()
            .when(matches!(self.view, View::Conversation(_)), |div| {
                div.key_context("RhoSlackConversation")
            })
            .size_full()
            .on_action(cx.listener(Self::open_row))
            .on_action(cx.listener(Self::open_thread_row))
            .on_action(cx.listener(Self::focus_composer))
            .on_action(cx.listener(Self::focus_log))
            .on_action(cx.listener(Self::send))
            .on_action(cx.listener(Self::next_unread))
            .on_action(cx.listener(Self::find_message))
            .on_action(cx.listener(Self::find_file))
            .on_action(cx.listener(Self::new_message))
            .on_action(cx.listener(Self::previous_page))
            .on_action(cx.listener(Self::next_page))
            .child(inner)
    }
}

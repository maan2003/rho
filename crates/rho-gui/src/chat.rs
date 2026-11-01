//! Chat beyond one reply: what the user has read, the stream and topic
//! browser, search over the messages this GUI holds, `#N` links, and the
//! edits a topic takes (its name, stream, resolved, members, messages).
//!
//! Read state is a fact in the ledger ([`Fact::Read`]), so every device
//! agrees on it, and only the user says it: opening a topic marks nothing.
//! Unread is everything after the newest of what the user marked read,
//! what they settled, and what they wrote, less their own messages.

use std::collections::BTreeMap;

use editor::Editor;
use gpui::{App, Context, Entity, Window};
use rho_agent_types::{ChatSeq, ContentPart, MessageId, StreamId, TopicId, UnixMs};
use rho_agents_client::protocol::worksets::{
    ChangeTopic, CreateStream, Leave, MoveMessages, MoveTo,
};
use rho_agents_client::remote::AgentsLink;
use rho_agents_client::state::UiBlock;
use rho_agents_client::{HostId, HostTopic, Topic};
use rho_agents_view::AgentModel;
use rho_agents_view::transcript::StorePoint;
use rho_dealer::NodeId;
use rho_dealer::facts::{Fact, Seen};
use rho_window::style::StyleClass;

use crate::minibuffer::Candidate;
use crate::pane::SurfaceKey;
use crate::topics_view::{BrowserRows, BrowserTarget, StreamRows, TopicRow, TopicsView};
use crate::workspace::{SurfaceView, Workspace, now_ms};

impl Workspace {
    /// The newest message of a topic or a direct conversation.
    pub(crate) fn newest_message(&self, conversation: HostTopic) -> Option<MessageId> {
        match self.chat.topic(conversation) {
            Some((_, topic)) => topic.newest.map(|(message, _)| message),
            None => self
                .chat
                .direct(conversation)
                .and_then(|(_, direct)| direct.newest.map(|(message, _)| message)),
        }
    }

    /// Where unread starts: the newest of what the user read, settled, and
    /// wrote.
    pub(crate) fn read_through(&self, conversation: HostTopic) -> Option<MessageId> {
        let read = self
            .attention
            .marks
            .get(&NodeId::Topic(conversation))
            .facts()
            .read_topic()
            .map(|read| MessageId(ChatSeq(read)));
        let wrote = self
            .chat
            .topic(conversation)
            .and_then(|(_, topic)| topic.user_wrote.map(|(message, _)| message));
        read.max(wrote)
    }

    /// The conversation is read through `through`. Said only when that
    /// moves where it is read, so saying it again writes nothing.
    pub(crate) fn mark_read(
        &mut self,
        conversation: HostTopic,
        through: MessageId,
        cx: &mut Context<Self>,
    ) {
        if self.read_through(conversation) >= Some(through) {
            return;
        }
        self.write_marks(
            vec![
                Fact::Read {
                    node: NodeId::Topic(conversation),
                    seen: Seen::Topic(through.0.0),
                }
                .into(),
            ],
            cx,
        );
    }
}

/// The text of a message, its text parts one after the other.
pub(crate) fn message_text(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            ContentPart::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `#N` the cursor is on in `line`, at character `column`.
pub(crate) fn link_at(line: &str, column: usize) -> Option<u64> {
    let chars = line.chars().collect::<Vec<_>>();
    let digit = |at: usize| chars.get(at).is_some_and(char::is_ascii_digit);
    // Back to the `#`, over the digits the cursor may be on.
    let mut hash = column.min(chars.len().saturating_sub(1));
    while hash > 0 && chars.get(hash) != Some(&'#') && digit(hash) {
        hash -= 1;
    }
    if chars.get(hash) != Some(&'#') || !digit(hash + 1) {
        return None;
    }
    // `a#1` is not a link: a word runs into it.
    if hash > 0 && chars[hash - 1].is_alphanumeric() {
        return None;
    }
    let end = (hash + 1..chars.len())
        .find(|at| !digit(*at))
        .unwrap_or(chars.len());
    if chars.get(end).is_some_and(|after| after.is_alphanumeric()) {
        return None;
    }
    chars[hash + 1..end].iter().collect::<String>().parse().ok()
}

/// A reply that quotes a message: its lines behind `> `, and where it is
/// from.
pub(crate) fn quote(text: &str, id: MessageId) -> String {
    let mut quoted = text
        .trim_end()
        .lines()
        .map(|line| match line.is_empty() {
            true => ">".to_owned(),
            false => format!("> {line}"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    quoted.push_str(&format!("\n#{}\n\n", id.0.0));
    quoted
}

/// One message search can find.
#[derive(Clone, Debug)]
struct Found {
    conversation: HostTopic,
    id: MessageId,
    at: UnixMs,
    title: String,
    text: String,
}

impl Found {
    /// Whether every word of the query is in it, its title included.
    fn matches(&self, words: &[String]) -> bool {
        let text = self.text.to_lowercase();
        let title = self.title.to_lowercase();
        words
            .iter()
            .all(|word| text.contains(word.as_str()) || title.contains(word.as_str()))
    }

    /// What its row says: its `#N`, then the line the query lands on.
    fn row(&self, words: &[String]) -> String {
        let line = self
            .text
            .lines()
            .find(|line| {
                let line = line.to_lowercase();
                words.iter().any(|word| line.contains(word.as_str()))
            })
            .or_else(|| self.text.lines().find(|line| !line.trim().is_empty()))
            .unwrap_or_default()
            .trim();
        let line = match line.chars().count() > 72 {
            true => format!("{}…", line.chars().take(71).collect::<String>()),
            false => line.to_owned(),
        };
        format!("#{} {line}", self.id.0.0)
    }
}

/// How many rows search shows.
const FOUND_CAP: usize = 50;

impl Workspace {
    /// The topic in view, with its model and editor.
    pub(crate) fn topic_in_view(&self) -> Option<(HostTopic, Entity<AgentModel>, Entity<Editor>)> {
        let surface = self.active_surface();
        match (&surface.key, &surface.view) {
            (SurfaceKey::Topic(topic), SurfaceView::Transcript { model, editor }) => {
                Some((*topic, model.clone(), editor.clone()))
            }
            _ => None,
        }
    }

    /// The message the point is on in the topic in view, and its text.
    fn message_here(&self, cx: &App) -> Option<(HostTopic, MessageId, String)> {
        let (topic, model, editor) = self.topic_in_view()?;
        let point = model.read(cx).store_point(&editor, cx)?;
        match self
            .transcripts
            .topic(topic)?
            .blocks
            .get(point.block)?
            .as_ref()
        {
            UiBlock::ChatMessage { id, text, .. } => Some((topic, *id, text.clone())),
            _ => None,
        }
    }

    /// `space o y`: the message at point's `#N`, on the clipboard.
    pub(crate) fn yank_message_link(&mut self, cx: &mut Context<Self>) {
        let Some((_, id, _)) = self.message_here(cx) else {
            self.notice_on(None, "no message at point", StyleClass::SystemInfo, cx);
            return;
        };
        let link = format!("#{}", id.0.0);
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(link.clone()));
        self.notice_on(None, &format!("yanked {link}"), StyleClass::SystemInfo, cx);
    }

    /// `space o q`: the message at point, quoted at the end of the reply.
    pub(crate) fn quote_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some((_, id, text)), Some((_, model, _))) =
            (self.message_here(cx), self.topic_in_view())
        else {
            self.notice_on(None, "no message at point", StyleClass::SystemInfo, cx);
            return;
        };
        let quoted = quote(&text, id);
        model.update(cx, |model, cx| {
            let held = model.prompt_text(cx);
            let text = match held.trim_end() {
                "" => quoted,
                held => format!("{held}\n\n{quoted}"),
            };
            model.set_prompt_text(&text, cx);
        });
        let editor = self.topic_in_view().map(|(_, _, editor)| editor);
        if let Some(editor) = editor {
            model.update(cx, |model, cx| model.focus_prompt(&editor, window, cx));
        }
    }

    /// Enter on `#N` in a topic: opens topic N, or the topic of message N
    /// at that message, on the host of the topic in view. Answers whether
    /// the point was on a link.
    pub(crate) fn follow_link(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some((topic, _, editor)) = self.topic_in_view() else {
            return false;
        };
        let (line, column) = editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let head = editor
                .selections
                .newest::<multi_buffer::MultiBufferPoint>(&editor.display_snapshot(cx))
                .head();
            let start = multi_buffer::MultiBufferPoint::new(head.row, 0);
            let end = multi_buffer::MultiBufferPoint::new(
                head.row,
                snapshot.line_len(multi_buffer::MultiBufferRow(head.row)),
            );
            let line = snapshot.text_for_range(start..end).collect::<String>();
            let column = line
                .char_indices()
                .take_while(|(at, _)| *at < head.column as usize)
                .count();
            (line, column)
        });
        let Some(n) = link_at(&line, column) else {
            return false;
        };
        let Some((host, _)) = self
            .chat
            .topic(topic)
            .map(|(host, _)| ((host), ()))
            .or_else(|| self.chat.direct(topic).map(|(host, _)| (host, ())))
        else {
            return false;
        };
        self.open_link(host, n, window, cx);
        true
    }

    /// Opens what `#n` names on `host`.
    pub(crate) fn open_link(
        &mut self,
        host: HostId,
        n: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let named = TopicId(ChatSeq(n));
        if let Some(conversation) = self.chat.host_topic(host, named) {
            self.open_topic(conversation, window, cx);
            return;
        }
        match self.message_conversation(host, MessageId(ChatSeq(n))) {
            Some(conversation) => {
                self.open_topic_at(conversation, MessageId(ChatSeq(n)), window, cx)
            }
            None => self.notice_on(
                None,
                &format!("#{n}: no such topic or message here"),
                StyleClass::SystemInfo,
                cx,
            ),
        }
    }

    /// The topic or direct conversation a message is in now: the one in
    /// view if it holds it, else where the copy says it is.
    fn message_conversation(&self, host: HostId, id: MessageId) -> Option<HostTopic> {
        let in_view = self
            .topic_in_view()
            .map(|(topic, _, _)| topic)
            .filter(|topic| self.topic_messages(*topic).contains(&id));
        in_view.or_else(|| {
            rho_agents_client::cache::message_topic(self.registry.host_name(host), id)
                .and_then(|topic| self.chat.host_topic(host, topic))
        })
    }

    /// The messages of a conversation as its view holds them, in id
    /// order: what the reader sees and picks from.
    fn topic_messages(&self, conversation: HostTopic) -> Vec<MessageId> {
        let Some(state) = self.transcripts.topic(conversation) else {
            return Vec::new();
        };
        let mut ids = state
            .blocks
            .iter()
            .filter_map(|block| match block.as_ref() {
                UiBlock::ChatMessage { id, .. } => Some(*id),
                _ => None,
            })
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    /// Every topic and direct conversation of a host.
    fn conversations(&self, host: HostId) -> Vec<HostTopic> {
        self.chat
            .topics()
            .filter(|(_, on, _)| *on == host)
            .map(|(topic, _, _)| topic)
            .chain(
                self.chat
                    .directs()
                    .filter(|(_, on, _)| *on == host)
                    .map(|(direct, _, _)| direct),
            )
            .collect()
    }

    /// Opens a conversation with the point on one of its messages.
    pub(crate) fn open_topic_at(
        &mut self,
        conversation: HostTopic,
        id: MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_topic(conversation, window, cx);
        let Some((_, model, editor)) = self.topic_in_view() else {
            return;
        };
        let block = self.transcripts.topic(conversation).and_then(|state| {
            state.blocks.iter().position(
                |block| matches!(block.as_ref(), UiBlock::ChatMessage { id: at, .. } if *at == id),
            )
        });
        if let Some(block) = block {
            model.update(cx, |model, cx| {
                model.go_to_store_point(&editor, StorePoint { block, offset: 0 }, window, cx)
            });
        }
    }

    /// `space o /`: the messages this GUI holds, every host's, newest
    /// first, narrowed by every word typed. Enter opens the topic at the
    /// message.
    pub(crate) fn prompt_search_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut found = Vec::new();
        let hosts = self
            .registry
            .hosts()
            .map(|(host, name)| (host, name.to_owned()))
            .collect::<Vec<_>>();
        for (host, name) in hosts {
            for conversation in self.conversations(host) {
                let title = self.topic_title(conversation);
                for message in self.agents_client.read_topic(&name, conversation.topic) {
                    found.push(Found {
                        conversation,
                        id: message.id,
                        at: message.at,
                        title: title.clone(),
                        text: message_text(&message.content),
                    });
                }
            }
        }
        found.sort_by(|one, other| (other.at, other.id).cmp(&(one.at, one.id)));
        let found = std::rc::Rc::new(found);
        let rows = |found: &[Found], input: &str| {
            let words = input
                .split_whitespace()
                .map(str::to_lowercase)
                .collect::<Vec<_>>();
            found
                .iter()
                .filter(|found| found.matches(&words))
                .take(FOUND_CAP)
                .map(|found| (found.row(&words), found))
                .map(|(row, found)| (row, found.clone()))
                .collect::<Vec<_>>()
        };
        let complete = {
            let found = found.clone();
            std::rc::Rc::new(move |_: &Workspace, input: &str, _: &App| {
                rows(&found, input)
                    .into_iter()
                    .map(|(row, found)| Candidate {
                        value: row,
                        description: found.title,
                    })
                    .collect()
            })
        };
        let on_submit = std::rc::Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  window: &mut Window,
                  cx: &mut Context<Workspace>| {
                // The row chosen, or the first row for what was typed.
                let chosen = found
                    .iter()
                    .find(|found| {
                        found.row(&[]) == input || {
                            let words = input
                                .split_whitespace()
                                .map(str::to_lowercase)
                                .collect::<Vec<_>>();
                            found.row(&words) == input
                        }
                    })
                    .cloned()
                    .or_else(|| {
                        rows(&found, &input)
                            .into_iter()
                            .next()
                            .map(|(_, found)| found)
                    });
                match chosen {
                    Some(found) => {
                        workspace.open_topic_at(found.conversation, found.id, window, cx)
                    }
                    None => workspace.notice_on(
                        None,
                        &format!("no message holds `{}`", input.trim()),
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
            },
        );
        self.open_prompt("search chat:", complete, on_submit, window, cx);
        self.set_prompt_complete_whole_input();
    }
}

impl Workspace {
    /// How many messages of a conversation are unread: those not from the
    /// user after where it is read through.
    pub(crate) fn unread(&self, conversation: HostTopic) -> usize {
        let from = self.read_through(conversation);
        let others = match self.chat.topic(conversation) {
            Some((_, topic)) => &topic.others,
            None => match self.chat.direct(conversation) {
                Some((_, direct)) => &direct.others,
                None => return 0,
            },
        };
        others
            .iter()
            .filter(|message| Some(**message) > from)
            .count()
    }

    /// Every stream with its topics newest first, and the directs.
    pub(crate) fn browser_rows(&self) -> BrowserRows {
        let now = now_ms() as i64;
        let age = |at: UnixMs| crate::home::elapsed_label(at.0 as i64, now);
        let hosts = self.registry.hosts().count();
        let mut streams: BTreeMap<(String, HostId, StreamId), Vec<(UnixMs, TopicRow)>> =
            BTreeMap::new();
        for (conversation, host, held) in self.chat.topics() {
            let Some(stream) = self
                .chat
                .host_chat(host)
                .and_then(|chat| chat.streams.get(&held.stream))
            else {
                continue;
            };
            // Two hosts may each have a stream of one name.
            let name = match hosts > 1 {
                true => format!("{stream} · {}", self.registry.host_name(host)),
                false => stream.clone(),
            };
            let moved = held.newest.map_or(held.started_at, |(_, at)| at);
            streams.entry((name, host, held.stream)).or_default().push((
                moved,
                TopicRow {
                    topic: conversation,
                    name: held.name.clone(),
                    unread: self.unread(conversation),
                    resolved: held.resolved,
                    age: age(moved),
                },
            ));
        }
        let newest_first = |mut topics: Vec<(UnixMs, TopicRow)>| {
            topics.sort_by(|one, other| (other.0, other.1.topic).cmp(&(one.0, one.1.topic)));
            topics.into_iter().map(|(_, row)| row).collect::<Vec<_>>()
        };
        let streams = streams
            .into_iter()
            .map(|((name, _, _), topics)| StreamRows {
                name,
                topics: newest_first(topics),
            })
            .collect();
        let directs = self
            .chat
            .directs()
            .map(|(direct, _, held)| {
                let moved = held.newest.map_or(held.started_at, |(_, at)| at);
                (
                    moved,
                    TopicRow {
                        topic: direct,
                        name: self.topic_title(direct),
                        unread: self.unread(direct),
                        resolved: false,
                        age: age(moved),
                    },
                )
            })
            .collect();
        BrowserRows {
            streams,
            directs: newest_first(directs),
        }
    }

    fn browser_view(&self) -> Option<Entity<TopicsView>> {
        self.find_surface(|surface| surface.key == SurfaceKey::Topics)
            .and_then(|surface| match &surface.view {
                SurfaceView::Topics(view) => Some(view.clone()),
                _ => None,
            })
    }

    pub(crate) fn refresh_browser(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.browser_view() else {
            return;
        };
        let rows = self.browser_rows();
        view.update(cx, |view, cx| view.set_rows(rows, cx));
    }

    /// `space o o`: the streams and their topics.
    pub(crate) fn open_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let surface = self.make_surface(SurfaceKey::Topics, window, cx);
        self.display_surface(surface, cx);
        self.refresh_browser(cx);
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    /// The browser's row under the cursor, when the browser is in view.
    fn browser_target(&self, cx: &mut Context<Self>) -> Option<HostTopic> {
        let SurfaceView::Topics(view) = &self.active_surface().view else {
            return None;
        };
        match view.update(cx, |view, cx| view.cursor_target(cx)) {
            BrowserTarget::Topic(topic) => Some(topic),
            BrowserTarget::None => None,
        }
    }

    /// Enter in the browser: opens the topic under the cursor.
    pub(crate) fn browser_open_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(topic) = self.browser_target(cx) {
            self.open_topic(topic, window, cx);
        }
    }

    pub(crate) fn browser_toggle_resolved(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.browser_view() {
            view.update(cx, |view, cx| view.toggle_resolved(cx));
        }
    }

    /// What the chat commands act on: the topic in view, or the browser's
    /// row under the cursor.
    pub(crate) fn chat_subject(&self, cx: &mut Context<Self>) -> Option<HostTopic> {
        self.topic_in_view()
            .map(|(topic, _, _)| topic)
            .or_else(|| self.browser_target(cx))
    }

    /// `space o r`, and `r` in the browser: the topic is read through its
    /// newest message.
    pub(crate) fn mark_topic_read(&mut self, cx: &mut Context<Self>) {
        let Some(conversation) = self.chat_subject(cx) else {
            self.notice_on(None, "mark read: no topic here", StyleClass::SystemInfo, cx);
            return;
        };
        let Some(newest) = self.newest_message(conversation) else {
            return;
        };
        self.mark_read(conversation, newest, cx);
    }

    /// `space o shift-r`: the topic in view is read through the message at
    /// point.
    pub(crate) fn mark_read_through_here(&mut self, cx: &mut Context<Self>) {
        let Some((conversation, id, _)) = self.message_here(cx) else {
            self.notice_on(None, "no message at point", StyleClass::SystemInfo, cx);
            return;
        };
        self.mark_read(conversation, id, cx);
    }
}

/// Which messages a move takes, as Zulip offers them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MoveMode {
    /// The message at point.
    This,
    /// It and every later message of its topic.
    ThisAndLater,
    /// Every message of the topic.
    Topic,
}

/// The messages a move of `mode` from `at` takes, out of a topic's
/// messages in id order.
pub(crate) fn moved_messages(
    messages: &[MessageId],
    at: MessageId,
    mode: MoveMode,
) -> Vec<MessageId> {
    match mode {
        MoveMode::This => messages.iter().copied().filter(|id| *id == at).collect(),
        MoveMode::ThisAndLater => messages.iter().copied().filter(|id| *id >= at).collect(),
        MoveMode::Topic => messages.to_vec(),
    }
}

/// Where a move sends messages, as typed: an existing topic's title, or
/// `stream › name` for a new one, or a bare name for a new one in
/// `stream`.
pub(crate) fn move_target(input: &str, stream: &str) -> (String, String) {
    match input.split_once('›') {
        Some((stream, name)) => (stream.trim().to_owned(), name.trim().to_owned()),
        None => (stream.to_owned(), input.trim().to_owned()),
    }
}

impl Workspace {
    /// The host link of a conversation's host, while it answers; a notice
    /// when it does not.
    fn online_agents(
        &mut self,
        host: HostId,
        what: &str,
        cx: &mut Context<Self>,
    ) -> Option<AgentsLink> {
        let agents = self.agents(host).filter(|_| self.hosts.is_online(host));
        if agents.is_none() {
            let label = self.hosts.host_label(host);
            self.notice_on(
                None,
                &format!("{what}: {label} is not connected"),
                StyleClass::SystemInfo,
                cx,
            );
        }
        agents
    }

    /// The topic the chat commands act on, with its host and fold; a
    /// notice for a direct conversation or nothing.
    fn topic_subject(
        &mut self,
        what: &str,
        cx: &mut Context<Self>,
    ) -> Option<(HostTopic, HostId, Topic)> {
        let subject = self.chat_subject(cx);
        let held = subject.and_then(|topic| {
            self.chat
                .topic(topic)
                .map(|(host, held)| (topic, host, held.clone()))
        });
        if held.is_none() {
            let why = match subject {
                Some(_) => "a direct conversation is its two agents' own",
                None => "no topic here",
            };
            self.notice_on(None, &format!("{what}: {why}"), StyleClass::SystemInfo, cx);
        }
        held
    }

    /// Asks the host a chat call and says what went wrong, if anything.
    fn chat_call<R: 'static>(
        &mut self,
        host: HostId,
        reply: impl std::future::Future<Output = anyhow::Result<R>> + 'static,
        then: impl FnOnce(&mut Self, R, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let reply = reply.await;
            this.update_in(cx, |this, window, cx| match reply {
                Ok(reply) => then(this, reply, window, cx),
                Err(error) => {
                    let source = this.error_source(host);
                    this.notice_on(
                        None,
                        &format!("[{source} error: {error:#}]"),
                        StyleClass::SystemImportant,
                        cx,
                    );
                }
            })
            .ok();
        })
        .detach();
    }

    /// A topic as it is to be, whole: the host answers with
    /// `TopicChanged`, which redraws it.
    fn change_topic(
        &mut self,
        host: HostId,
        change: ChangeTopic,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(agents) = self.online_agents(host, "change topic", cx) else {
            return;
        };
        let reply = agents.call(change);
        self.chat_call(host, reply, |_, (), _, _| {}, window, cx);
    }

    /// `space o v`: resolves the topic, or opens it again.
    pub(crate) fn toggle_resolved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((topic, host, held)) = self.topic_subject("resolve", cx) else {
            return;
        };
        let change = ChangeTopic {
            topic: topic.topic,
            stream: held.stream,
            name: held.name,
            resolved: !held.resolved,
        };
        self.change_topic(host, change, window, cx);
    }

    /// `space o n`: a new name for the topic, edited from its old one.
    pub(crate) fn prompt_rename_topic(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((topic, host, held)) = self.topic_subject("rename", cx) else {
            return;
        };
        let complete = std::rc::Rc::new(|_: &Workspace, _: &str, _: &App| Vec::new());
        let on_submit = std::rc::Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  window: &mut Window,
                  cx: &mut Context<Workspace>| {
                let name = input.trim().to_owned();
                if name.is_empty() || name == held.name {
                    return;
                }
                let change = ChangeTopic {
                    topic: topic.topic,
                    stream: held.stream,
                    name,
                    resolved: held.resolved,
                };
                workspace.change_topic(host, change, window, cx);
            },
        );
        let name = self
            .chat
            .topic(topic)
            .map(|(_, held)| held.name.clone())
            .unwrap_or_default();
        self.open_prompt("rename topic:", complete, on_submit, window, cx);
        if let Some(minibuffer) = &mut self.minibuffer {
            minibuffer.set_input(name, window, cx);
        }
    }

    /// The streams of a host by name, the ones holding what was typed.
    fn stream_candidates(&self, host: HostId, input: &str) -> Vec<Candidate> {
        let input = input.trim().to_lowercase();
        self.chat
            .host_chat(host)
            .map(|chat| {
                chat.streams
                    .values()
                    .filter(|name| name.to_lowercase().contains(&input))
                    .map(|name| Candidate {
                        value: name.clone(),
                        description: "stream".to_owned(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The id of stream `name` on `host`, made first if the host has none
    /// by that name.
    fn with_stream(
        &mut self,
        host: HostId,
        name: String,
        then: impl FnOnce(&mut Self, StreamId, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(stream) = self
            .chat
            .host_chat(host)
            .and_then(|chat| chat.stream_named(&name))
        {
            then(self, stream, window, cx);
            return;
        }
        let Some(agents) = self.online_agents(host, "stream", cx) else {
            return;
        };
        let reply = agents.call(CreateStream { name });
        self.chat_call(host, reply, then, window, cx);
    }

    /// `space o t`: moves the topic to another stream, made if new.
    pub(crate) fn prompt_topic_stream(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((topic, host, held)) = self.topic_subject("move topic", cx) else {
            return;
        };
        let complete = std::rc::Rc::new(move |workspace: &Workspace, input: &str, _: &App| {
            workspace.stream_candidates(host, input)
        });
        let on_submit = std::rc::Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  window: &mut Window,
                  cx: &mut Context<Workspace>| {
                let name = input.trim().to_owned();
                if name.is_empty() {
                    return;
                }
                let held = held.clone();
                workspace.with_stream(
                    host,
                    name,
                    move |workspace, stream, window, cx| {
                        if stream == held.stream {
                            return;
                        }
                        let change = ChangeTopic {
                            topic: topic.topic,
                            stream,
                            name: held.name,
                            resolved: held.resolved,
                        };
                        workspace.change_topic(host, change, window, cx);
                    },
                    window,
                    cx,
                );
            },
        );
        self.open_prompt("move topic to stream:", complete, on_submit, window, cx);
    }

    /// `space o l`: takes a member out of the topic.
    pub(crate) fn prompt_remove_member(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((topic, host, held)) = self.topic_subject("remove member", cx) else {
            return;
        };
        let members = held
            .members_in_order()
            .into_iter()
            .map(|agent| (self.registry.agent_id_label(agent), agent))
            .collect::<Vec<_>>();
        if members.is_empty() {
            self.notice_on(
                None,
                "remove member: nobody has joined",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        let rows = members.clone();
        let complete = std::rc::Rc::new(move |workspace: &Workspace, input: &str, _: &App| {
            let input = input.trim().trim_start_matches('@').to_lowercase();
            rows.iter()
                .filter(|(label, agent)| {
                    label.to_lowercase().contains(&input)
                        || workspace
                            .registry
                            .agent_display_label(*agent)
                            .to_lowercase()
                            .contains(&input)
                })
                .map(|(label, agent)| Candidate {
                    value: label.clone(),
                    description: workspace.registry.agent_display_label(*agent),
                })
                .collect()
        });
        let on_submit = std::rc::Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  window: &mut Window,
                  cx: &mut Context<Workspace>| {
                let input = input.trim().trim_start_matches('@').to_lowercase();
                // Enter takes the first row, as on every picker.
                let Some((_, agent)) = members
                    .iter()
                    .find(|(label, _)| label.to_lowercase() == input)
                    .or_else(|| {
                        members
                            .iter()
                            .find(|(label, _)| label.to_lowercase().contains(&input))
                    })
                else {
                    return;
                };
                let Some(agents) = workspace.online_agents(host, "remove member", cx) else {
                    return;
                };
                let reply = agents.call(Leave {
                    topic: topic.topic,
                    agent: *agent,
                });
                workspace.chat_call(host, reply, |_, (), _, _| {}, window, cx);
            },
        );
        self.open_prompt("remove member:", complete, on_submit, window, cx);
    }

    /// `space o x` and a mode: moves messages of the topic in view to
    /// another topic, picked, or to a new one named `stream › name`.
    pub(crate) fn prompt_move_messages(
        &mut self,
        mode: MoveMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((topic, host, held)) = self.topic_subject("move messages", cx) else {
            return;
        };
        // The whole topic needs no message at point; the others start at it.
        let at = match (mode, self.message_here(cx)) {
            (MoveMode::Topic, _) => MessageId(ChatSeq(0)),
            (_, Some((_, id, _))) => id,
            (_, None) => {
                self.notice_on(
                    None,
                    "move messages: no message at point",
                    StyleClass::SystemInfo,
                    cx,
                );
                return;
            }
        };
        let Some((in_view, _, _)) = self
            .topic_in_view()
            .filter(|(in_view, _, _)| *in_view == topic)
        else {
            self.notice_on(
                None,
                "move messages: open the topic first",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        let in_topic = self.topic_messages(in_view);
        let messages = moved_messages(&in_topic, at, mode);
        if messages.is_empty() {
            self.notice_on(
                None,
                "move messages: nothing to move",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        }
        let stream_name = self
            .chat
            .host_chat(host)
            .and_then(|chat| chat.streams.get(&held.stream).cloned())
            .unwrap_or_default();
        let targets = self
            .conversations(host)
            .into_iter()
            .filter(|other| *other != topic && self.chat.topic(*other).is_some())
            .map(|other| (self.topic_title(other).replace("✔ ", ""), other))
            .collect::<Vec<_>>();
        let rows = targets.clone();
        let complete = std::rc::Rc::new(move |_: &Workspace, input: &str, _: &App| {
            let input = input.trim().to_lowercase();
            rows.iter()
                .filter(|(title, _)| title.to_lowercase().contains(&input))
                .map(|(title, _)| Candidate {
                    value: title.clone(),
                    description: "topic".to_owned(),
                })
                .collect()
        });
        let count = messages.len();
        let on_submit = std::rc::Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  window: &mut Window,
                  cx: &mut Context<Workspace>| {
                let input = input.trim();
                if input.is_empty() {
                    return;
                }
                let messages = messages.clone();
                let first = messages[0];
                let send = move |workspace: &mut Workspace,
                                 to: MoveTo,
                                 window: &mut Window,
                                 cx: &mut Context<Workspace>| {
                    let Some(agents) = workspace.online_agents(host, "move messages", cx) else {
                        return;
                    };
                    let reply = agents.call(MoveMessages {
                        // Fresh per move: the host logs a client id once.
                        client_id: uuid::Uuid::new_v4().as_u64_pair().0,
                        messages,
                        to,
                    });
                    workspace.chat_call(
                        host,
                        reply,
                        move |workspace, to, window, cx| {
                            // Where they went is where the reader follows.
                            if let Some(to) = workspace.chat.host_topic(host, to) {
                                workspace.notice_on(
                                    None,
                                    &format!("moved {count} to {}", workspace.topic_title(to)),
                                    StyleClass::SystemInfo,
                                    cx,
                                );
                                workspace.open_topic_at(to, first, window, cx);
                            }
                        },
                        window,
                        cx,
                    );
                };
                match targets.iter().find(|(title, _)| title.as_str() == input) {
                    Some((_, to)) => send(workspace, MoveTo::Topic(to.topic), window, cx),
                    None => {
                        let (stream, name) = move_target(input, &stream_name);
                        workspace.with_stream(
                            host,
                            stream,
                            move |workspace, stream, window, cx| {
                                send(workspace, MoveTo::NewTopic { stream, name }, window, cx)
                            },
                            window,
                            cx,
                        );
                    }
                }
            },
        );
        let prompt = match mode {
            MoveMode::This => "move message to:",
            MoveMode::ThisAndLater => "move this and later to:",
            MoveMode::Topic => "move topic's messages to:",
        };
        self.open_prompt(prompt, complete, on_submit, window, cx);
        self.set_prompt_complete_whole_input();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(ids: &[u64]) -> Vec<MessageId> {
        ids.iter().map(|id| MessageId(ChatSeq(*id))).collect()
    }

    #[test]
    fn a_link_is_the_number_the_cursor_is_on() {
        let line = "see #42, and #7x or a#9 (#1234)";
        // On the `#`, on a digit, on the last digit.
        assert_eq!(link_at(line, 4), Some(42));
        assert_eq!(link_at(line, 5), Some(42));
        assert_eq!(link_at(line, 6), Some(42));
        assert_eq!(link_at(line, 7), None, "the comma after it is not");
        assert_eq!(link_at(line, 0), None);
        assert_eq!(link_at(line, 14), None, "a word runs on from #7x");
        assert_eq!(link_at(line, 21), None, "a#9 is a word");
        assert_eq!(link_at(line, 26), Some(1234), "inside parentheses");
        assert_eq!(link_at("#5", 1), Some(5));
        assert_eq!(link_at("#", 0), None);
        assert_eq!(link_at("", 0), None);
    }

    #[test]
    fn a_move_takes_the_messages_its_mode_names() {
        // Not contiguous: other topics' messages fall between.
        let topic = ids(&[3, 5, 9, 12, 20]);
        let at = MessageId(ChatSeq(9));
        assert_eq!(moved_messages(&topic, at, MoveMode::This), ids(&[9]));
        assert_eq!(
            moved_messages(&topic, at, MoveMode::ThisAndLater),
            ids(&[9, 12, 20]),
            "this and every later message, none before"
        );
        assert_eq!(moved_messages(&topic, at, MoveMode::Topic), topic);
    }

    #[test]
    fn a_move_target_names_its_stream_or_stays_in_this_one() {
        assert_eq!(
            move_target("ops › deploy notes ", "rho"),
            ("ops".to_owned(), "deploy notes".to_owned())
        );
        assert_eq!(
            move_target("aside", "rho"),
            ("rho".to_owned(), "aside".to_owned())
        );
    }

    #[test]
    fn a_quote_marks_every_line_and_says_where_it_is_from() {
        assert_eq!(
            quote("first\n\nsecond\n", MessageId(ChatSeq(17))),
            "> first\n>\n> second\n#17\n\n"
        );
    }

    #[test]
    fn search_wants_every_word_somewhere_and_shows_the_line_it_found() {
        let found = Found {
            conversation: HostTopic {
                machine_seed: 0,
                topic: TopicId(ChatSeq(2)),
            },
            id: MessageId(ChatSeq(31)),
            at: UnixMs(0),
            title: "rho › Deploy".to_owned(),
            text: "first line\nthe Flaky test again".to_owned(),
        };
        let words = |query: &str| {
            query
                .split_whitespace()
                .map(str::to_lowercase)
                .collect::<Vec<_>>()
        };
        assert!(found.matches(&words("flaky deploy")), "the title counts");
        assert!(!found.matches(&words("flaky nightly")));
        assert_eq!(found.row(&words("flaky")), "#31 the Flaky test again");
        assert_eq!(found.row(&[]), "#31 first line");
    }
}

//! What a search found: one place per hit, in an ordinary editor.
//!
//! A hit is a place and this surface is the way to it. Nothing here is a
//! fact about the workspace — the results are thrown away when the reader
//! leaves — so this view holds them itself rather than putting them in the
//! model, and asks the session for nothing but the names it draws with.

use std::ops::Range;

use editor::{Editor, EditorMode, SizingBehavior};
use gpui::prelude::*;
use gpui::{Context, Entity, EventEmitter, MouseButton, MouseUpEvent, Window, div};
use language::{Buffer, Capability, Point};
use multi_buffer::ToPoint as _;
use text::{Anchor, Bias};
use theme::ActiveTheme as _;

use crate::api::{FileSearchPage, SearchHit, SearchPage};
use crate::session::{ActivityEntry, SearchKind, SearchRefused, Session, Source};
use crate::types::{FileSummary, ThreadKey, Ts};
use crate::ui::{Class, Hooks, Span, apply_highlights, lay_out, when_label};

/// A place the reader chose: which conversation, and which message in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub source: Source,
    pub ts: Ts,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Message(Place),
    File(FileSummary),
}

#[derive(Clone, Debug)]
pub enum Event {
    Open(Place),
    OpenFile(FileSummary),
}

impl EventEmitter<Event> for ResultsView {}

pub struct ResultsView {
    session: Entity<Session>,
    buffer: Entity<Buffer>,
    multi_buffer: Entity<multi_buffer::MultiBuffer>,
    editor: Entity<Editor>,
    /// What each line of the buffer says, and where the line goes. One
    /// entry per line, so the line the cursor is on *is* the lookup.
    drawn: Vec<DrawnLine>,
    query: String,
    page: u32,
    pages: u32,
    loading: bool,
    kind: SearchKind,
    /// Last inventory inputs, so unrelated session notifications cost no
    /// buffer edit and cannot disturb scroll or selection.
    inventory_state: Option<(String, Vec<ActivityEntry>, Option<String>)>,
}

struct DrawnLine {
    /// The target this line opens, or `None` for a heading or failure line.
    target: Option<Target>,
    text: String,
    styles: Vec<(Class, Range<usize>)>,
}

impl ResultsView {
    pub fn new(
        session: Entity<Session>,
        hooks: Hooks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = multi_buffer::MultiBuffer::without_headers(Capability::Read);
            multi_buffer.set_excerpts_for_path(
                multi_buffer::PathKey::sorted(0),
                buffer.clone(),
                [Point::zero()..buffer.read(cx).max_point()],
                0,
                cx,
            );
            multi_buffer
        });
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: true,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer.clone(),
                None,
                window,
                cx,
            );
            (hooks.configure_editor)(&mut editor, window, cx);
            editor.disable_header_for_buffer(buffer.read(cx).remote_id(), cx);
            editor
        });
        Self {
            session,
            buffer,
            multi_buffer,
            editor,
            drawn: Vec::new(),
            query: String::new(),
            page: 0,
            pages: 0,
            loading: false,
            kind: SearchKind::Messages,
            inventory_state: None,
        }
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    /// The one line shown while the query is out. A search is a request over
    /// a network and the reader is owed the difference between "still
    /// asking" and "nothing".
    pub fn asking(
        &mut self,
        query: &str,
        kind: SearchKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.query = query.to_owned();
        self.kind = kind;
        self.page = 0;
        self.pages = 0;
        self.loading = true;
        self.draw(
            vec![(
                None,
                vec![Span::styled(format!("looking for {query}…"), Class::Muted)],
            )],
            window,
            cx,
        );
    }

    /// Draws an answer. The point goes to the first hit, which is the place
    /// the reader is most likely to want and the one they can act on with
    /// no motion at all.
    pub fn found(
        &mut self,
        query: &str,
        page: &SearchPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.query = query.to_owned();
        self.page = page.page;
        self.pages = page.pages;
        self.loading = false;
        self.kind = SearchKind::Messages;
        let hits = &page.hits;
        let mut lines = vec![(None, heading(query, hits.len(), page.total))];
        if hits.is_empty() {
            lines.push((None, vec![Span::styled("nobody said that", Class::Muted)]));
        }
        let session = self.session.read(cx);
        let now = now_seconds();
        for hit in hits {
            let you = hit.message.user.as_ref() == Some(session.model().self_id());
            let author = session.model().author(&hit.message);
            let at = hit.message.ts.epoch_seconds();
            let when = match at.is_finite() {
                true => when_label(at as i64, now),
                false => String::new(),
            };
            let source = match hit.message.thread_ts.as_ref() {
                Some(thread_ts) if thread_ts != &hit.message.ts => Source::Thread(ThreadKey {
                    workspace: session.model().workspace().clone(),
                    channel: hit.channel.clone(),
                    thread_ts: thread_ts.clone(),
                }),
                _ => Source::Conversation(hit.channel.clone()),
            };
            let place = Place {
                source,
                ts: hit.message.ts.clone(),
            };
            lines.push((
                Some(Target::Message(place.clone())),
                vec![
                    Span::styled(
                        author,
                        match you {
                            true => Class::You,
                            false => Class::Sender,
                        },
                    ),
                    Span::plain("  "),
                    Span::styled(label_of(hit), Class::Conversation),
                    Span::plain("  "),
                    Span::styled(when, Class::Time),
                ],
            ));
            // The message's own line under its header, indented so the two
            // read as one hit. The same place, so the reader landing on
            // either line opens the same message.
            lines.push((
                Some(Target::Message(place)),
                vec![Span::plain(format!(
                    "  {}",
                    session
                        .model()
                        .render_parts(&hit.message)
                        .0
                        .replace('\n', " ")
                ))],
            ));
        }
        append_navigation(&mut lines, page.page, page.pages);
        self.draw(lines, window, cx);
    }

    /// Draws standalone file matches from Slack's file index.
    pub fn found_files(
        &mut self,
        query: &str,
        page: &FileSearchPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.query = query.to_owned();
        self.page = page.page;
        self.pages = page.pages;
        self.loading = false;
        self.kind = SearchKind::Files;
        let mut lines = vec![(None, heading(query, page.files.len(), page.total))];
        if page.files.is_empty() {
            lines.push((None, vec![Span::styled("no files matched", Class::Muted)]));
        }
        for file in &page.files {
            lines.push((
                Some(Target::File(file.clone())),
                vec![Span::styled(file.line(), Class::Conversation)],
            ));
        }
        append_navigation(&mut lines, page.page, page.pages);
        self.draw(lines, window, cx);
    }

    /// The adjacent page to request, if one exists and no request is in flight.
    fn adjacent_page(
        &mut self,
        offset: i32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(String, u32, SearchKind)> {
        if self.loading || self.page == 0 {
            return None;
        }
        let page = i64::from(self.page) + i64::from(offset);
        if page < 1 || page > i64::from(self.pages) {
            return None;
        }
        let page = page as u32;
        self.loading = true;
        self.draw(
            vec![(
                None,
                vec![Span::styled(
                    format!("looking for {}, page {page}…", self.query),
                    Class::Muted,
                )],
            )],
            window,
            cx,
        );
        Some((self.query.clone(), page, self.kind))
    }

    pub fn request_adjacent(
        &mut self,
        offset: i32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((query, page, kind)) = self.adjacent_page(offset, window, cx) else {
            return false;
        };
        self.session.update(cx, |session, cx| match kind {
            SearchKind::Messages => session.search(&query, page, cx),
            SearchKind::Files => session.search_files(&query, page, cx),
        });
        true
    }

    /// Draws a durable Slack inventory. Unlike search results these rows
    /// come from the mirror and remain available while Slack is offline.
    pub fn inventory(
        &mut self,
        heading: &str,
        entries: &[ActivityEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let health = self.session.read(cx).health_reason().map(str::to_owned);
        let state = (heading.to_owned(), entries.to_vec(), health.clone());
        if self.inventory_state.as_ref() == Some(&state) {
            return;
        }
        self.inventory_state = Some(state);
        let selected = match self.cursor_target(cx) {
            Some(Target::Message(place)) => Some(place),
            _ => None,
        };
        let mut lines = vec![(None, vec![Span::styled(heading.to_owned(), Class::Muted)])];
        if let Some(reason) = health {
            lines.push((None, vec![Span::styled(reason, Class::Error)]));
        }
        if entries.is_empty() {
            lines.push((None, vec![Span::styled("nothing here", Class::Muted)]));
        }
        for entry in entries {
            let place = Place {
                source: entry.source.clone(),
                ts: entry.ts.clone(),
            };
            let mut head = vec![Span::styled(
                entry.conversation.clone(),
                Class::Conversation,
            )];
            if entry.unread {
                head.push(Span::plain("  "));
                head.push(Span::styled("unread", Class::Unread));
            }
            lines.push((Some(Target::Message(place.clone())), head));
            if !entry.summary.is_empty() {
                lines.push((
                    Some(Target::Message(place)),
                    vec![Span::plain(format!(
                        "  {}",
                        entry.summary.replace('\n', " ")
                    ))],
                ));
            }
        }
        self.draw(lines, window, cx);
        if let Some(row) = selected
            .as_ref()
            .and_then(|selected| row_for_place(&self.drawn, selected))
        {
            self.place_cursor(row, window, cx);
        }
    }

    /// What the reader is told when the search did not answer. One line,
    /// and for the one refusal they can act on, what to do about it.
    pub fn refused(
        &mut self,
        query: &str,
        why: &SearchRefused,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.loading = false;
        let said = match why {
            SearchRefused::NotAllowed => {
                "this slack session is not allowed to search; a fresh token and cookie will fix it"
            }
            SearchRefused::Failed => "slack did not answer the search",
        };
        self.draw(
            vec![
                (None, heading(query, 0, 0)),
                (None, vec![Span::styled(said, Class::Error)]),
            ],
            window,
            cx,
        );
    }

    /// The place the cursor is on: what `enter` opens.
    pub fn cursor_target(&self, cx: &mut Context<Self>) -> Option<Target> {
        // A buffer position, not a display one: see `ListView::cursor_row`.
        let head = self.editor.read(cx).selections.newest_anchor().head();
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let row = head.to_point(&snapshot).row as usize;
        self.drawn.get(row).and_then(|line| line.target.clone())
    }

    fn open_clicked(&mut self, position: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        let hit = self.editor.update(cx, |editor, cx| {
            let selection = editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx));
            selection
                .is_empty()
                .then(|| editor.buffer_location_for_window_position(position, Bias::Left))
                .flatten()
                .map(|(_, _, offset)| offset)
        });
        let Some(target) = target_for_click(&self.drawn, hit) else {
            return;
        };
        match target {
            Target::Message(place) => cx.emit(Event::Open(place)),
            Target::File(file) => cx.emit(Event::OpenFile(file)),
        }
    }

    /// The lines as the reader reads them, for a test outside this crate.
    #[cfg(any(test, feature = "fake"))]
    pub fn drawn_lines_for_test(&self) -> Vec<String> {
        self.drawn.iter().map(|line| line.text.clone()).collect()
    }

    /// Writes the buffer. The whole of it, every time: a page of hits is
    /// bounded by what one request returns and is replaced whole by the
    /// next answer, so there is nothing an incremental edit would save.
    fn draw(
        &mut self,
        lines: Vec<(Option<Target>, Vec<Span>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drawn = lines
            .into_iter()
            .map(|(target, spans)| {
                let (text, styles) = lay_out(&spans);
                DrawnLine {
                    target,
                    text,
                    styles,
                }
            })
            .collect();
        let text = self.drawn.iter().fold(String::new(), |mut text, line| {
            text.push_str(&line.text);
            text.push('\n');
            text
        });
        self.buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, text)], None, cx);
        });
        self.paint(cx);
        let first = self
            .drawn
            .iter()
            .position(|line| line.target.is_some())
            .unwrap_or_default();
        self.place_cursor(first, window, cx);
        cx.notify();
    }

    fn place_cursor(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let point = Point::new(row as u32, 0);
        let anchor = self.multi_buffer.read(cx).snapshot(cx).anchor_before(point);
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_anchor_ranges([anchor..anchor]);
            });
        });
    }

    fn paint(&mut self, cx: &mut Context<Self>) {
        let mut styles: Vec<(Class, Range<usize>)> = Vec::new();
        let mut base = 0usize;
        for line in &self.drawn {
            styles.extend(
                line.styles
                    .iter()
                    .map(|(class, range)| (*class, base + range.start..base + range.end)),
            );
            base += line.text.len() + 1;
        }
        let anchored = self.buffer.update(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            styles
                .into_iter()
                .map(|(class, range)| {
                    let clamp = |offset: usize| offset.min(snapshot.len());
                    (
                        class,
                        vec![
                            snapshot.anchor_before(clamp(range.start))
                                ..snapshot.anchor_after(clamp(range.end)),
                        ],
                    )
                })
                .collect::<Vec<(Class, Vec<Range<Anchor>>)>>()
        });
        apply_highlights(&self.editor, &self.multi_buffer, &anchored, cx);
    }
}

fn target_for_click(lines: &[DrawnLine], offset: Option<usize>) -> Option<Target> {
    target_at_offset(lines, offset?)
}

fn target_at_offset(lines: &[DrawnLine], offset: usize) -> Option<Target> {
    let mut start = 0;
    for line in lines {
        let end = start + line.text.len();
        if (start..end).contains(&offset) {
            return line.target.clone();
        }
        // `draw` writes one newline after every line.
        start = end + 1;
    }
    None
}

fn append_navigation(lines: &mut Vec<(Option<Target>, Vec<Span>)>, page: u32, pages: u32) {
    if pages <= 1 {
        return;
    }
    let mut navigation = Vec::new();
    if page > 1 {
        navigation.push("[ previous");
    }
    if page < pages {
        navigation.push("] next");
    }
    lines.push((
        None,
        vec![Span::styled(
            format!(
                "page {page} of {pages}{}",
                if navigation.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", navigation.join(" · "))
                }
            ),
            Class::Muted,
        )],
    ));
}

fn row_for_place(lines: &[DrawnLine], selected: &Place) -> Option<usize> {
    lines
        .iter()
        .position(|line| line.target.as_ref() == Some(&Target::Message(selected.clone())))
}

/// What the surface says it is showing. The count is Slack's, so a page of
/// forty out of three hundred says so rather than implying that is all there
/// was.
fn heading(query: &str, shown: usize, total: u32) -> Vec<Span> {
    let said = match total as usize > shown {
        true => format!("{shown} of {total} for {query}"),
        false => format!("{shown} for {query}"),
    };
    vec![Span::styled(said, Class::Muted)]
}

/// The conversation's name as the search answered it. Slack names the
/// channel inside every match, so this needs neither the roster nor a
/// conversation the reader has ever opened.
fn label_of(hit: &SearchHit) -> String {
    match hit.channel_label.is_empty() {
        true => "somewhere".to_owned(),
        false => format!("#{}", hit.channel_label),
    }
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default()
}

impl gpui::Render for ResultsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        div()
            .key_context("RhoSlackResults")
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.editor_background)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseUpEvent, _, cx| {
                            this.open_clicked(event.position, cx)
                        }),
                    )
                    .child(self.editor.clone()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> FileSummary {
        FileSummary {
            id: "F1".to_owned(),
            title: "report.pdf".to_owned(),
            filetype: "pdf".to_owned(),
            size: 1,
            url: String::new(),
            original_w: 0,
            original_h: 0,
            thumb_url: String::new(),
        }
    }

    #[test]
    fn only_text_inside_a_result_row_is_a_mouse_target() {
        let target = Target::File(file());
        let lines = vec![
            DrawnLine {
                target: None,
                text: "heading".to_owned(),
                styles: Vec::new(),
            },
            DrawnLine {
                target: Some(target.clone()),
                text: "report.pdf".to_owned(),
                styles: Vec::new(),
            },
        ];

        assert_eq!(target_for_click(&lines, Some(8)), Some(target));
        assert_eq!(
            target_for_click(&lines, Some(18)),
            None,
            "the newline or right-side padding after the result does not open it"
        );
        assert_eq!(
            target_for_click(&lines, None),
            None,
            "pagination is outside the editor hit region and cannot emit Open"
        );
    }
}

#[cfg(test)]
mod inventory_tests {
    use super::{DrawnLine, Place, Target, row_for_place};
    use crate::session::Source;
    use crate::types::{ChannelId, Ts};

    fn place(channel: &str, ts: &str) -> Place {
        Place {
            source: Source::Conversation(ChannelId(channel.into())),
            ts: Ts(ts.into()),
        }
    }

    fn line(place: Option<Place>) -> DrawnLine {
        DrawnLine {
            target: place.map(Target::Message),
            text: String::new(),
            styles: Vec::new(),
        }
    }

    #[test]
    fn unrelated_inventory_arrival_preserves_the_selected_target() {
        let selected = place("C2", "200.0");
        let redrawn = vec![
            line(None),
            line(Some(place("C3", "300.0"))),
            line(Some(place("C1", "100.0"))),
            line(Some(selected.clone())),
        ];

        assert_eq!(row_for_place(&redrawn, &selected), Some(3));
    }
}

//! What a search found: one place per hit, in an ordinary editor.
//!
//! A hit is a place and this surface is the way to it. Nothing here is a
//! fact about the workspace — the results are thrown away when the reader
//! leaves — so this view holds them itself rather than putting them in the
//! model, and asks the session for nothing but the names it draws with.

use std::ops::Range;

use editor::{Editor, EditorMode, SizingBehavior};
use gpui::prelude::*;
use gpui::{Context, Entity, Window, div};
use language::{Buffer, Capability, Point};
use multi_buffer::ToPoint as _;
use text::Anchor;
use theme::ActiveTheme as _;

use crate::api::SearchHit;
use crate::session::{SearchRefused, Session, Source};
use crate::types::Ts;
use crate::ui::{Class, Hooks, Span, apply_highlights, lay_out, when_label};

/// A place the reader chose: which conversation, and which message in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub source: Source,
    pub ts: Ts,
}

pub struct ResultsView {
    session: Entity<Session>,
    buffer: Entity<Buffer>,
    multi_buffer: Entity<multi_buffer::MultiBuffer>,
    editor: Entity<Editor>,
    /// What each line of the buffer says, and where the line goes. One
    /// entry per line, so the line the cursor is on *is* the lookup.
    drawn: Vec<DrawnLine>,
}

struct DrawnLine {
    /// The place this line opens, or `None` for a line that opens nothing:
    /// the heading, the failure, the second line of a hit.
    place: Option<Place>,
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
        }
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    /// The one line shown while the query is out. A search is a request over
    /// a network and the reader is owed the difference between "still
    /// asking" and "nothing".
    pub fn asking(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
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
        hits: &[SearchHit],
        total: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut lines = vec![(None, heading(query, hits.len(), total))];
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
            let place = Place {
                source: Source::Conversation(hit.channel.clone()),
                ts: hit.message.ts.clone(),
            };
            lines.push((
                Some(place.clone()),
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
                Some(place),
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
        self.draw(lines, window, cx);
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
    pub fn cursor_place(&self, cx: &mut Context<Self>) -> Option<Place> {
        // A buffer position, not a display one: see `ListView::cursor_row`.
        let head = self.editor.read(cx).selections.newest_anchor().head();
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let row = head.to_point(&snapshot).row as usize;
        self.drawn.get(row).and_then(|line| line.place.clone())
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
        lines: Vec<(Option<Place>, Vec<Span>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drawn = lines
            .into_iter()
            .map(|(place, spans)| {
                let (text, styles) = lay_out(&spans);
                DrawnLine {
                    place,
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
            .position(|line| line.place.is_some())
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
        div()
            .key_context("RhoSlackResults")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.editor.clone())
    }
}

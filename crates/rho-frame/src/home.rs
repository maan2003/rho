//! Home: the rooms, the one that wants the user most first.
//!
//! A room is a place the user works in: a host's channel they made or
//! wrote in, a Slack conversation they starred, or one that holds a card
//! or something for them unread. Each system lists its own
//! ([`System::rooms`](crate::system::System::rooms)); nothing here
//! scores anything. A root that is a room only while it holds a card
//! ([`Standing::WhileCard`]) is left off while the dealer's hand holds
//! none in it. A room with a card in the hand goes by that card, in the
//! hand's order, wearing the card's words; the rest go by what waits in
//! them, then by when they last moved. A room the user snoozed says
//! until when, so a quiet row is not a forgotten one. The surface is one
//! keyed transcript: a row is an item keyed by its room, so a change
//! edits that row and leaves the cursor, the scroll, and every other row
//! alone.

use std::ops::Range;

use editor::{Editor, EditorMode, HighlightKey, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, HighlightStyle, Window, div};
use jiff::Zoned;
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey, ToPoint as _};
use rho_dealer::rank::Hand;
use rho_dealer::{Address, Marks};
use rho_transcript::{Item, Transcript};
use theme::ActiveTheme as _;

use crate::surface::{Subject, Surface};
use crate::system::{RoomRow, Standing};
use crate::when::until_said;

/// Home's own highlight-key space, clear of the transcript's semantic slots
/// at zero and Slack's at the top. Buckets grow upwards from here.
const HOME_KEY_BASE: usize = usize::MAX / 3;

/// The most of a room's name a row shows. A note's name is its first
/// line, which can run on; the row is one line, and the width it is cut
/// to is measured on an average glyph, so a long name could still wrap.
const NAME_CAP: usize = 60;

/// One room as Home shows it: ordered, with the words beside its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomLine {
    pub address: Address,
    pub name: String,
    pub tag: String,
    /// The card's words when the dealer holds one in the room, the
    /// system's otherwise.
    pub label: String,
    /// The card in the hand that gave the row its words, when one did:
    /// what a verdict on the row is about.
    pub card: Option<Address>,
}

/// The rooms in Home's order: a room with a card in `hand` by its first
/// card, in the hand's order; then the rooms something waits in, newest
/// first; then the rest, newest first. A root that is a room only while
/// it holds a card is left off without one. A room without a card that
/// the user snoozed reads `snoozed · until 05:17` instead of its own words.
pub fn room_lines(rooms: Vec<RoomRow>, hand: &Hand, marks: &Marks, now: &Zoned) -> Vec<RoomLine> {
    let mut lines: Vec<(Option<usize>, bool, u64, RoomLine)> = rooms
        .into_iter()
        .filter_map(|room| {
            let card = hand.cards.iter().enumerate().find(|(_, card)| {
                card.address == room.address || card.room.as_ref() == Some(&room.address)
            });
            if card.is_none() && room.standing == Standing::WhileCard {
                return None;
            }
            let snoozed = marks
                .get(&room.address)
                .facts()
                .snooze()
                .filter(|snooze| snooze.until > now.timestamp());
            let label = match (card, snoozed) {
                (Some((_, card)), _) => card.label.clone(),
                (None, Some(snooze)) => format!(
                    "snoozed · {}",
                    until_said(&snooze.until.to_zoned(now.time_zone().clone()), now)
                ),
                (None, None) => room.waiting.clone(),
            };
            Some((
                card.map(|(index, _)| index),
                room.waiting.is_empty(),
                u64::MAX - room.at,
                RoomLine {
                    address: room.address,
                    name: room.name,
                    tag: room.tag,
                    label,
                    card: card.map(|(_, card)| card.address.clone()),
                },
            ))
        })
        .collect();
    lines.sort_by(|a, b| {
        // A card first, the earlier in the hand the sooner; no card last.
        let by_card = match (a.0, b.0) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        by_card.then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2))
    });
    lines.into_iter().map(|(_, _, _, line)| line).collect()
}

/// What a Home row opens when the cursor is on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HomeTarget {
    Address(Address),
    /// The empty line: nothing to open.
    None,
}

/// One item of the transcript.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum HomeKey {
    Room(Address),
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum HomeClass {
    Title,
    Label,
    Muted,
}

impl HomeClass {
    fn slot(self) -> usize {
        match self {
            Self::Title => 0,
            Self::Label => 1,
            Self::Muted => 2,
        }
    }

    const COUNT: usize = 3;
}

impl rho_transcript::Style for HomeClass {
    fn highlight_key(self, bucket: u32) -> HighlightKey {
        HighlightKey::SyntaxTreeView(HOME_KEY_BASE + bucket as usize * Self::COUNT + self.slot())
    }

    fn highlight_style(self, cx: &App) -> HighlightStyle {
        let colors = cx.theme().colors();
        let color = match self {
            Self::Title => colors.text,
            Self::Label => cx.theme().status().warning,
            Self::Muted => colors.text_muted,
        };
        HighlightStyle {
            color: Some(color.into()),
            ..HighlightStyle::default()
        }
    }
}

/// One buffer, one surface. The editor is an ordinary rho editor, so
/// motions, search and vim come from it rather than from list chrome.
pub struct HomeView {
    transcript: Transcript<HomeKey, HomeClass, HomeTarget>,
    editor: Entity<Editor>,
    rooms: Vec<RoomLine>,
    /// How many characters fit on one row, as the window last measured it;
    /// a room's name is cut to leave its row one line.
    width: usize,
}

impl HomeView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::Read);
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(0),
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
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor.disable_header_for_buffer(buffer.read(cx).remote_id(), cx);
            editor
        });
        let width = row_width(&editor, window, cx);
        let mut view = Self {
            transcript: Transcript::new(buffer),
            editor,
            rooms: Vec::new(),
            width,
        };
        view.transcript.attach(&view.editor.clone(), cx);
        // A Home nobody has told anything yet still answers the question.
        let items = view.items();
        view.transcript.reconcile(items, cx);
        view.focus_first_row(cx);
        view
    }

    /// What the cursor is on. The row under the cursor is the one Enter
    /// opens.
    pub fn cursor_target(&self, cx: &App) -> HomeTarget {
        self.transcript
            .line_meta(self.cursor_row(cx), cx)
            .cloned()
            .unwrap_or(HomeTarget::None)
    }

    fn cursor_row(&self, cx: &App) -> u32 {
        let editor = self.editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor
            .selections
            .newest_anchor()
            .head()
            .to_point(&snapshot)
            .row
    }

    pub fn set_rooms(&mut self, rooms: Vec<RoomLine>, cx: &mut Context<Self>) {
        if rooms == self.rooms {
            return;
        }
        self.rooms = rooms;
        let items = self.items();
        self.transcript.reconcile(items, cx);
        // A reader who has not moved is put on the first row, so the very
        // first Enter opens.
        if self.cursor_target(cx) == HomeTarget::None {
            self.focus_first_row(cx);
        }
        cx.notify();
    }

    /// Puts the cursor on the first row that stands for something.
    fn focus_first_row(&mut self, cx: &mut Context<Self>) {
        let last = self.transcript.buffer().read(cx).snapshot().max_point().row;
        let row = (0..=last)
            .find(|row| {
                !matches!(
                    self.transcript.line_meta(*row, cx),
                    None | Some(HomeTarget::None)
                )
            })
            .unwrap_or(0);
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.change_with(&snapshot, |selections| {
                selections.select_ranges([Point::new(row, 0)..Point::new(row, 0)]);
            });
            cx.notify();
        });
    }

    /// The rooms as transcript items. Names are padded to a common column
    /// so the words beside them line up and the eye reads down them.
    fn items(&self) -> Vec<Item<HomeKey, HomeClass, HomeTarget>> {
        if self.rooms.is_empty() {
            return vec![
                line(
                    HomeKey::Empty,
                    "no rooms yet · attach a host or register a Slack workspace from the palette (:)",
                    HomeClass::Muted,
                )
                    .with_lines(vec![HomeTarget::None]),
            ];
        }
        let column = column_of(self.rooms.iter().map(|room| room.name.clone()).collect());
        self.rooms
            .iter()
            .map(|room| {
                let label = match (room.label.is_empty(), room.tag.is_empty()) {
                    (true, _) => room.tag.clone(),
                    (false, true) => room.label.clone(),
                    (false, false) => format!("{} · {}", room.label, room.tag),
                };
                let class = match room.card {
                    Some(_) => HomeClass::Label,
                    None => HomeClass::Muted,
                };
                // The lead, the gap before the words and the words are
                // what the row keeps; the name has the rest.
                let room_for_name = self
                    .width
                    .saturating_sub(4 + label.chars().count())
                    .min(NAME_CAP);
                let name = cut(&room.name, room_for_name);
                let (text, styles) = columns(&[
                    (name.as_str(), HomeClass::Title, column.min(room_for_name)),
                    (label.as_str(), class, 0),
                ]);
                Item::new(HomeKey::Room(room.address.clone()), text)
                    .with_styles(styles)
                    .with_lines(vec![HomeTarget::Address(room.address.clone())])
            })
            .collect()
    }
}

impl Surface for HomeView {
    fn title(&self, _cx: &App) -> String {
        "home".to_owned()
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        Some(self.editor.clone())
    }

    fn subject(&self, cx: &App) -> Subject {
        match self.transcript.line_meta(self.cursor_row(cx), cx) {
            Some(HomeTarget::Address(address)) => Subject::Address(address.clone()),
            _ => Subject::None,
        }
    }

    /// The card that gave the row its words, or the room itself when
    /// none did.
    fn card(&self, cx: &App) -> Subject {
        let Subject::Address(room) = self.subject(cx) else {
            return Subject::None;
        };
        let card = self
            .rooms
            .iter()
            .find(|line| line.address == room)
            .and_then(|line| line.card.clone());
        Subject::Address(card.unwrap_or(room))
    }
}

impl Focusable for HomeView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

/// `text` in at most `room` characters, ending in an ellipsis when cut.
fn cut(text: &str, room: usize) -> String {
    if text.chars().count() <= room {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(room.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// How many characters of Home's text fit across the window. Measured on
/// `0`, the font's average width, less two for the editor's gutter.
fn row_width(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) -> usize {
    let style = editor.update(cx, |editor, cx| editor.style(cx).clone());
    let font_size = style.text.font_size.to_pixels(window.rem_size());
    let font_id = window.text_system().resolve_font(&style.text.font());
    let advance = window
        .text_system()
        .ch_advance(font_id, font_size)
        .expect("the editor's font resolves");
    let columns = f32::from(window.viewport_size().width) / f32::from(advance);
    (columns as usize).saturating_sub(2)
}

fn column_of(names: Vec<String>) -> usize {
    /// Past this a name is left to run into its own words rather than
    /// pushing every row on the screen sideways.
    const CAP: usize = 40;
    names
        .iter()
        .map(|name| name.chars().count())
        .max()
        .unwrap_or(0)
        .min(CAP)
}

fn line(key: HomeKey, text: &str, class: HomeClass) -> Item<HomeKey, HomeClass, HomeTarget> {
    Item::new(key, text.to_owned()).with_styles(vec![(class, 0..text.len())])
}

/// Lays out one row: two leading spaces, then each cell padded to its
/// column. Offsets are byte offsets into the row's own text, which is what
/// the transcript wants.
fn columns(cells: &[(&str, HomeClass, usize)]) -> (String, Vec<(HomeClass, Range<usize>)>) {
    let mut text = "  ".to_owned();
    let mut styles = Vec::new();
    for (index, (cell, class, column)) in cells.iter().enumerate() {
        if index > 0 {
            text.push_str("  ");
        }
        let start = text.len();
        text.push_str(cell);
        styles.push((*class, start..text.len()));
        let width = cell.chars().count();
        for _ in width..*column {
            text.push(' ');
        }
    }
    (text.trim_end().to_owned(), styles)
}

impl Render for HomeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // One measure per frame, and a relayout only when it changes: a
        // narrower window cuts the names shorter.
        let width = row_width(&self.editor, window, cx);
        if width != self.width {
            self.width = width;
            let items = self.items();
            self.transcript.reconcile(items, cx);
        }
        div()
            .key_context("RhoHome")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.editor.clone())
    }
}

#[cfg(test)]
mod tests {
    use rho_dealer::Card;

    use super::*;

    #[test]
    fn a_row_pads_its_cells_to_the_column() {
        let (text, styles) = columns(&[("ab", HomeClass::Title, 4), ("x", HomeClass::Label, 0)]);
        assert_eq!(text, "  ab    x");
        assert_eq!(styles[1].1, 8..9);
        assert_eq!(cut("abcdef", 4), "abc…");
        assert_eq!(cut("abc", 4), "abc");
    }

    fn room(name: &str, waiting: &str, at: u64) -> RoomRow {
        RoomRow {
            address: Address::new("thread", name),
            standing: Standing::Worked,
            name: name.to_owned(),
            tag: "local".to_owned(),
            waiting: waiting.to_owned(),
            at,
        }
    }

    fn now() -> Zoned {
        jiff::civil::date(2026, 8, 23)
            .at(4, 17, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap()
    }

    fn card(address: &str, room: Option<&str>, label: &str) -> Card {
        Card {
            address: Address::new("thread", address),
            title: String::new(),
            context: String::new(),
            label: label.to_owned(),
            priority: 1.0,
            cursor: String::new(),
            skipped: false,
            room: room.map(|room| Address::new("thread", room)),
        }
    }

    /// The hand's order first, by the card in a room or the room itself;
    /// then what waits, newest first; then the rest, newest first. A
    /// dealt room wears its first card's words and names that card, the
    /// others their own.
    #[test]
    fn rooms_go_by_the_hand_then_by_what_waits_then_by_age() {
        let hand = Hand {
            cards: vec![
                card("c/7", Some("c"), "asks · 5m"),
                card("a", None, "needs reply · 1h"),
                card("c/9", Some("c"), "result · 2h ago"),
            ],
            next_change: None,
        };
        let rooms = vec![
            room("quiet-old", "", 10),
            room("b", "2 unread · 3m", 50),
            room("a", "", 20),
            room("quiet-new", "", 40),
            room("c", "1 unread", 30),
            room("d", "mention", 60),
        ];
        let lines = room_lines(rooms, &hand, &Marks::default(), &now());
        let names: Vec<(&str, &str, Option<&str>)> = lines
            .iter()
            .map(|line| {
                (
                    line.name.as_str(),
                    line.label.as_str(),
                    line.card.as_ref().map(Address::rest),
                )
            })
            .collect();
        // The row of `c` wears the words of its first card, and that card
        // is what a verdict on the row is about, not the room.
        assert_eq!(
            names,
            [
                ("c", "asks · 5m", Some("c/7")),
                ("a", "needs reply · 1h", Some("a")),
                ("d", "mention", None),
                ("b", "2 unread · 3m", None),
                ("quiet-new", "", None),
                ("quiet-old", "", None),
            ]
        );
    }

    /// A root the user never worked in is a room only while the hand
    /// holds a card in it, whether the card is on the root or on a thread
    /// inside it; without one it is left off, however new it is.
    #[test]
    fn a_root_only_a_card_makes_a_room_needs_one_in_the_hand() {
        let hand = Hand {
            cards: vec![card("deploys/3", Some("deploys"), "asks · 1m")],
            next_change: None,
        };
        let only_with_card = |name: &str, at| RoomRow {
            standing: Standing::WhileCard,
            ..room(name, "", at)
        };
        let rooms = vec![
            room("worked", "", 10),
            only_with_card("deploys", 20),
            only_with_card("deploy 41", 90),
            RoomRow {
                standing: Standing::WhileCard,
                ..room("deploy 42", "1 unread · 1m", 99)
            },
        ];
        let names: Vec<String> = room_lines(rooms.clone(), &hand, &Marks::default(), &now())
            .into_iter()
            .map(|line| line.name)
            .collect();
        assert_eq!(names, ["deploys", "worked"]);
        let empty = Hand {
            cards: Vec::new(),
            next_change: None,
        };
        let names: Vec<String> = room_lines(rooms, &empty, &Marks::default(), &now())
            .into_iter()
            .map(|line| line.name)
            .collect();
        assert_eq!(names, ["worked"]);
    }

    /// A room the user snoozed says until when, in place of its own words
    /// and in the same grey, so a quiet row reads as put away rather than
    /// as nothing; once the snooze is over it has its own words again.
    #[test]
    fn a_snoozed_room_says_until_when() {
        use rho_dealer::Until;
        use rho_dealer::facts::{Device, Entry, Fact};

        let empty = Hand {
            cards: Vec::new(),
            next_change: None,
        };
        let mut marks = Marks::default();
        marks.apply([Entry {
            device: Device([0; 16]),
            at: now(),
            fact: Fact::Snooze {
                node: Address::new("thread", "random"),
                until: Until::In(jiff::SignedDuration::from_hours(1)),
            },
        }]);
        let rooms = vec![
            room("random", "1 unread · 2.4d", 10),
            room("design", "1 unread · 3m", 20),
        ];
        let labels = |now: &Zoned| {
            room_lines(rooms.clone(), &empty, &marks, now)
                .into_iter()
                .map(|line| (line.name, line.label, line.card))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(&now()),
            [
                ("design".to_owned(), "1 unread · 3m".to_owned(), None),
                (
                    "random".to_owned(),
                    "snoozed · until 05:17".to_owned(),
                    None
                ),
            ],
            "the snoozed room has no card, so its words are grey like any other"
        );
        let later = now()
            .checked_add(jiff::SignedDuration::from_hours(1))
            .unwrap();
        assert_eq!(
            labels(&later)[1].1,
            "1 unread · 2.4d",
            "a snooze that is over says nothing"
        );
    }
}

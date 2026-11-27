//! The Desk shows only what waits in the dealer's hand (`desk.Desk`): a
//! read-only sheet of one-line rows, card 1 on row 1, under its own name
//! and count in the top-left corner. It is a glance: no cursor and no
//! selected row. The deal walks the cards, and a click opens a row.

use std::collections::{HashMap, HashSet};

use gpui::prelude::*;
use gpui::{App, Context, FocusHandle, Focusable, SharedString, WeakEntity, Window, div, px};
use rho_dealer::Address;
use rho_dealer::rank::{Card, CardKind, Hand};
use rho_window::layout::{Notch, READING_INSET_PX, SHEET_MAX_CH, TOP_ROW_PX};
use theme::ActiveTheme as _;

use crate::frame::Frame;
use crate::hosts::Reach;
use crate::phone::Layout;
use crate::surface::{CardHere, Subject, Surface};
use crate::system::Project;

const WAITING_LIMIT: usize = 7;
/// The share of the sheet the project column may take at most.
const PROJECT_SHARE: f32 = 0.25;
/// The space between a subject and its tag; columns stand twice as far
/// apart.
const CELL_GAP_PX: f32 = 12.;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DeskKey {
    Waiting(Address),
    Stops(Address),
}

/// One waiting row: what it is about, why it is an exception when it is
/// one, where it is, and how long it has waited.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DeskRow {
    key: DeskKey,
    subject: String,
    /// `approve push`, `3 stopped`, `away`.
    tag: Option<String>,
    project: String,
    /// `3.0h`, as the card's words say it.
    age: String,
    flash: Option<String>,
    card: Address,
}

#[derive(Default, PartialEq, Eq)]
pub(crate) struct DeskRows {
    wait_count: usize,
    waiting: Vec<DeskRow>,
    overflow: Option<WaitingOverflow>,
}

#[derive(PartialEq, Eq)]
struct WaitingOverflow {
    first: Address,
    words: String,
}

impl DeskRows {
    fn keep_order(rows: &mut [DeskRow], previous: &[DeskRow]) {
        rows.sort_by_key(|row| {
            previous
                .iter()
                .position(|old| old.key == row.key)
                .unwrap_or(usize::MAX)
        });
    }
}

/// The attached hosts the deal cannot count on now. Some hosts away is an
/// everyday state: an empty deal says so after `nothing waits`, and the
/// global status bar names them, never the Desk's own rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HostsAway {
    away: Vec<String>,
    connecting: Vec<String>,
}

impl HostsAway {
    pub(crate) fn of(hosts: &rho_agent_hosts::hosts::Hosts) -> Self {
        Self::from_statuses(hosts.iter().map(|host| (host.name.as_str(), &host.status)))
    }

    /// A host that dropped, has gone quiet, or needs the user's hand is
    /// away; one dialling for the first time is connecting.
    fn from_statuses<'a>(
        hosts: impl IntoIterator<Item = (&'a str, &'a rho_agent_hosts::hosts::HostStatus)>,
    ) -> Self {
        let mut away = Self::default();
        for (name, status) in hosts {
            match Reach::of(status) {
                Reach::Online => {}
                Reach::Connecting => away.connecting.push(name.to_owned()),
                Reach::Away | Reach::NeedsYou => away.away.push(name.to_owned()),
            }
        }
        away
    }

    /// `lab away`, `2 hosts away · lab connecting`; nothing while every
    /// host answers.
    pub(crate) fn note(&self) -> Option<String> {
        let group = |names: &[String], state: &str| match names {
            [] => None,
            [name] => Some(format!("{name} {state}")),
            many => Some(format!("{} hosts {state}", many.len())),
        };
        let parts: Vec<String> = group(&self.away, "away")
            .into_iter()
            .chain(group(&self.connecting, "connecting"))
            .collect();
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// What the echo line says when a deal finds nothing: in the project
    /// the deal is focused on, when it is.
    pub(crate) fn nothing_waits(&self, in_project: Option<&str>) -> String {
        let nothing = match in_project {
            Some(project) => format!("nothing waits in {project}"),
            None => "nothing waits".to_owned(),
        };
        match self.note() {
            Some(note) => format!("{nothing} · {note}"),
            None => nothing,
        }
    }
}

/// How long `card` has waited, as its words end: `3.0h` of `waits · 3.0h`.
fn age_of(card: &Card) -> String {
    match card.kind {
        CardKind::Other => String::new(),
        _ => card
            .label
            .rsplit_once(" · ")
            .map_or_else(String::new, |(_, age)| age.to_owned()),
    }
}

/// Why a card is an exception rather than a plain wait: a push to approve
/// or a stop. A card cached from a host that is away says `away`.
fn tag_of(card: &Card, away: bool) -> Option<String> {
    match card.kind {
        CardKind::Approve(what) => Some(format!("approve {what}")),
        CardKind::Stop(word) => Some(word.to_owned()),
        CardKind::Waits | CardKind::Due | CardKind::Other => None,
    }
    .or_else(|| away.then(|| "away".to_owned()))
}

pub(crate) fn desk_rows(
    hand: &Hand,
    project_of: impl Fn(&Address) -> Option<Project>,
    flash_at: impl Fn(&Address) -> Option<String>,
    away: impl Fn(&Address) -> bool,
) -> DeskRows {
    // A project's stops occupy its first stop's place. Every pinging mail
    // remains a row, even when another came from the same project.
    let mut waiting: Vec<(DeskRow, CardKind, usize, bool)> = Vec::new();
    let mut stops = HashMap::new();
    for card in &hand.cards {
        let project = project_of(&card.address);
        let key = match (card.kind, &project) {
            (CardKind::Stop(word), Some(project)) => {
                if let Some(&index) = stops.get(&project.address) {
                    let (_, kind, count, mixed): &mut (DeskRow, CardKind, usize, bool) =
                        &mut waiting[index];
                    *count += 1;
                    *mixed |= *kind != CardKind::Stop(word);
                    continue;
                }
                stops.insert(project.address.clone(), waiting.len());
                DeskKey::Stops(project.address.clone())
            }
            _ => DeskKey::Waiting(card.address.clone()),
        };
        waiting.push((
            DeskRow {
                key,
                subject: card.title.clone(),
                tag: tag_of(card, away(&card.address)),
                project: project.map_or_else(|| card.context.clone(), |project| project.name),
                age: age_of(card),
                flash: flash_at(&card.address),
                card: card.address.clone(),
            },
            card.kind,
            1,
            false,
        ));
    }
    let overflow = waiting.get(WAITING_LIMIT..).and_then(|hidden| {
        let (first, ..) = hidden.first()?;
        let total: usize = hidden.iter().map(|(_, _, count, _)| count).sum();
        let mut words = format!("and {total} more");
        let mut projects = HashSet::new();
        for (row, ..) in hidden {
            if !row.project.is_empty() && projects.insert(row.project.as_str()) {
                words.push_str(" · ");
                words.push_str(&row.project);
            }
        }
        Some(WaitingOverflow {
            first: first.card.clone(),
            words,
        })
    });
    let waiting = waiting
        .into_iter()
        .take(WAITING_LIMIT)
        .map(|(mut row, kind, count, mixed)| {
            // A project's repeated stops are one row about the project.
            if let CardKind::Stop(reason) = kind
                && count > 1
            {
                row.subject = std::mem::take(&mut row.project);
                let reason = if mixed { "stops" } else { reason };
                row.tag = Some(format!("{count} {reason}"));
                row.flash = None;
            }
            row
        })
        .collect();
    DeskRows {
        wait_count: hand.cards.len(),
        waiting,
        overflow,
    }
}

/// The Desk's view: the rows of the hand, drawn as they are. Nothing on it
/// takes a cursor; its keys are the window's.
pub(crate) struct DeskView {
    rows: DeskRows,
    /// The frame, which opens a clicked row's card.
    frame: WeakEntity<Frame>,
    focus: FocusHandle,
    /// Whether the rows were last drawn for a phone, where a row keeps its
    /// place under the thumb.
    phone: bool,
}

impl DeskView {
    pub fn new(frame: WeakEntity<Frame>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            rows: DeskRows::default(),
            frame,
            focus: cx.focus_handle(),
            phone: Layout::of(window.viewport_size().width) == Layout::Phone,
        }
    }

    pub(crate) fn set_rows(&mut self, mut rows: DeskRows, hold: bool, cx: &mut Context<Self>) {
        if hold && self.phone {
            // Update displayed words in place. A newly ranked row can't
            // take the place of one the user is about to tap.
            DeskRows::keep_order(&mut rows.waiting, &self.rows.waiting);
        }
        if rows != self.rows {
            self.rows = rows;
            cx.notify();
        }
    }

    fn open(&self, card: Address, window: &mut Window, cx: &mut App) {
        if let Some(frame) = self.frame.upgrade() {
            frame.update(cx, |frame, cx| frame.open_row(&card, window, cx));
        }
    }

    /// The laptop sheet: one column of subjects, each with its exception
    /// tag, then the projects and the ages, each column as wide as its
    /// widest cell and right-aligned, so every row lines up without
    /// measuring a glyph. The project column takes at most its share of
    /// the sheet. A click on any cell of a row opens its card.
    fn sheet(&self, cx: &mut Context<Self>) -> gpui::Div {
        let (text, muted) = {
            let colors = cx.theme().colors();
            (colors.text, colors.text_muted)
        };
        let warning = cx.theme().status().warning;
        let subjects: Vec<_> = self
            .rows
            .waiting
            .iter()
            .map(|row| {
                Self::cell("subject", row, cx)
                    .flex()
                    .gap(px(CELL_GAP_PX))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(text)
                            .child(first_line(&row.subject).to_owned()),
                    )
                    .children(
                        row.tag
                            .clone()
                            .map(|tag| div().flex_none().text_color(warning).child(tag)),
                    )
            })
            .collect();
        let mut right = |column: &str, cell: fn(&DeskRow) -> &str| {
            if self.rows.waiting.iter().all(|row| cell(row).is_empty()) {
                return None;
            }
            let cells: Vec<_> = self
                .rows
                .waiting
                .iter()
                .map(|row| {
                    Self::cell(column, row, cx)
                        .text_right()
                        .truncate()
                        .text_color(muted)
                        // An empty cell still holds its row's line.
                        .child(match first_line(cell(row)) {
                            "" => "\u{00a0}".to_owned(),
                            words => words.to_owned(),
                        })
                })
                .collect();
            Some(div().flex_none().flex().flex_col().children(cells))
        };
        let projects = right("project", |row| &row.project)
            .map(|column| column.max_w(gpui::relative(PROJECT_SHARE)));
        let ages = right("age", |row| &row.age);
        div()
            .w_full()
            .flex()
            .gap(px(CELL_GAP_PX * 2.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .children(subjects),
            )
            .children(projects)
            .children(ages)
    }

    /// One cell of a laptop row: a single line that opens the row's card.
    fn cell(column: &str, row: &DeskRow, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let card = row.card.clone();
        div()
            .id(SharedString::from(format!("desk-{column}-{}", row.card)))
            .w_full()
            .whitespace_nowrap()
            .overflow_hidden()
            .on_click(cx.listener(move |this, _, window, cx| this.open(card.clone(), window, cx)))
    }

    /// A phone row: the subject and project, then the exception tag or the
    /// card's current words on a second line.
    fn compact(&self, row: &DeskRow, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let colors = cx.theme().colors();
        let card = row.card.clone();
        let extra = match &row.tag {
            Some(tag) => Some((tag.clone(), cx.theme().status().warning)),
            None => row.flash.clone().map(|words| (words, colors.text_muted)),
        };
        div()
            .id(SharedString::from(format!("desk-row-{}", row.card)))
            .w_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .w_full()
                    .flex()
                    .gap_4()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(colors.text)
                            .child(row.subject.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .max_w(gpui::relative(0.45))
                            .truncate()
                            .text_color(colors.text_muted)
                            .child(row.project.clone()),
                    ),
            )
            .children(extra.map(|(words, color)| {
                div()
                    .pl_4()
                    .truncate()
                    .text_color(color)
                    .child(first_line(&words).to_owned())
            }))
            .on_click(cx.listener(move |this, _, window, cx| this.open(card.clone(), window, cx)))
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

impl Surface for DeskView {
    fn title(&self, _cx: &App) -> String {
        "desk".into()
    }

    /// The Desk's name and count are its own top-left corner.
    fn draws_top_left(&self, _cx: &App) -> bool {
        true
    }

    /// No row is ever the Desk's subject: it has no cursor.
    fn subject(&self, _: Option<rho_dealer::facts::Seen>, _cx: &App) -> Subject {
        Subject::None
    }

    fn card(&self, _cx: &App) -> CardHere {
        CardHere::None
    }
}

impl Focusable for DeskView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for DeskView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size().width;
        self.phone = Layout::of(viewport) == Layout::Phone;
        let colors = cx.theme().colors();
        // The frame sets the buffer font on the screen's layer.
        let ch = rho_window::layout::ch(&window.text_style(), window);
        // The count is global and quiet. The dealer focus and hosts away
        // are the status bar's to say.
        let count = div()
            .flex_none()
            .text_color(colors.text_muted)
            .child(format!("{} wait", self.rows.wait_count));
        let overflow = self.rows.overflow.as_ref().map(|overflow| {
            let first = overflow.first.clone();
            div()
                .id("desk-more")
                .truncate()
                .text_color(colors.text_muted)
                .child(overflow.words.clone())
                .on_click(
                    cx.listener(move |this, _, window, cx| this.open(first.clone(), window, cx)),
                )
        });
        let root = div()
            .key_context(crate::keys::DESK)
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.editor_background);
        if self.phone {
            // A phone's status line already names the Desk.
            let rows: Vec<_> = self
                .rows
                .waiting
                .iter()
                .map(|row| self.compact(row, cx))
                .collect();
            return root.child(
                div()
                    .id("desk-sheet")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_4()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(count)
                    .children(rows)
                    .children(overflow),
            );
        }
        let sheet = (viewport - px(READING_INSET_PX) * 2.).min(ch * SHEET_MAX_CH);
        let rows = self.sheet(cx);
        // On a laptop the Desk's name and count are its top-left corner,
        // left of the notch; row 1 starts right under it.
        let heading = div()
            .flex_none()
            .h(px(TOP_ROW_PX))
            .w(Notch::of(viewport).start)
            .pl(px(READING_INSET_PX))
            .flex()
            .items_center()
            .gap(px(12.))
            .overflow_hidden()
            .child(
                div()
                    .flex_none()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(rho_window::style::unread_text_color(cx))
                    .child(self.title(cx)),
            )
            .child(count);
        root.child(heading).child(
            div()
                .id("desk-sheet")
                .flex_1()
                .min_h_0()
                .w(sheet)
                .mx_auto()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .child(rows)
                .children(overflow),
        )
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_hosts::hosts::HostStatus;
    use rho_dealer::Card;

    use super::*;

    fn away(hosts: &[(&str, HostStatus)]) -> HostsAway {
        HostsAway::from_statuses(hosts.iter().map(|(name, status)| (*name, status)))
    }

    #[test]
    fn hosts_that_answer_leave_nothing_to_note() {
        for hosts in [
            vec![],
            vec![("lab", HostStatus::Online), ("reef", HostStatus::Online)],
        ] {
            let away = away(&hosts);
            assert_eq!(away.note(), None);
            assert_eq!(away.nothing_waits(None), "nothing waits");
        }
    }

    #[test]
    fn a_host_still_connecting_is_noted() {
        let one = away(&[("lab", HostStatus::Connecting)]);
        assert_eq!(one.note().as_deref(), Some("lab connecting"));
        assert_eq!(one.nothing_waits(None), "nothing waits · lab connecting");
        let both = away(&[
            ("lab", HostStatus::Connecting),
            ("reef", HostStatus::Connecting),
        ]);
        assert_eq!(
            both.nothing_waits(None),
            "nothing waits · 2 hosts connecting"
        );
    }

    /// An away host is an everyday state, whatever kept it away: a drop, a
    /// connection gone quiet, or a host that needs the user's hand. It is
    /// named calmly and never with how long.
    #[test]
    fn hosts_away_are_noted_calmly() {
        let lab_away = away(&[
            ("lab", HostStatus::Disconnected("refused".into())),
            ("reef", HostStatus::Online),
        ]);
        assert_eq!(lab_away.nothing_waits(None), "nothing waits · lab away");
        let all_away = away(&[
            (
                "lab",
                HostStatus::Recovering(std::time::Duration::from_secs(12)),
            ),
            ("reef", HostStatus::Unenrolled("rho enroll".into())),
        ]);
        assert_eq!(all_away.note().as_deref(), Some("2 hosts away"));
        let mixed = away(&[
            ("lab", HostStatus::Disconnected("refused".into())),
            ("reef", HostStatus::Connecting),
            ("sea", HostStatus::Online),
        ]);
        assert_eq!(
            mixed.nothing_waits(None),
            "nothing waits · lab away · reef connecting"
        );
        assert!(
            !all_away.note().unwrap().contains("12s"),
            "no ticking counter"
        );
    }

    /// An empty focused deal names the project it looked in.
    #[test]
    fn an_empty_focused_deal_names_its_project() {
        let lab_away = away(&[("lab", HostStatus::Disconnected("refused".into()))]);
        assert_eq!(
            lab_away.nothing_waits(Some("tide")),
            "nothing waits in tide · lab away"
        );
        assert_eq!(
            away(&[]).nothing_waits(Some("tide")),
            "nothing waits in tide"
        );
    }

    fn address(n: u32) -> Address {
        Address::new("thread", format!("t{n}"))
    }

    fn card(n: u32) -> Card {
        Card {
            address: address(n),
            title: format!("idea {n}"),
            context: "unformatted channel".into(),
            label: format!("waits · {n}m"),
            kind: CardKind::Waits,
            priority: f64::from(n),
            cursor: format!("{n}/100"),
            seen: Default::default(),
            skipped: false,
        }
    }

    fn hand(cards: impl Into<Vec<Card>>) -> Hand {
        Hand {
            cards: cards.into(),
            next_change: None,
        }
    }

    fn nowhere(_: &Address) -> bool {
        false
    }

    /// A row is the card's subject, its exception, its project and how
    /// long it waited, each cell from its own part of the card: the age is
    /// the end of the card's words, never the words before it.
    #[test]
    fn a_row_takes_each_cell_from_its_own_part_of_the_card() {
        let mut push = card(4);
        push.kind = CardKind::Approve("push");
        push.label = "approve push · 3.0h".into();
        let mut stop = card(5);
        stop.kind = CardKind::Stop("failed");
        stop.label = "failed · 12m".into();
        let mut other = card(6);
        other.kind = CardKind::Other;
        other.label = "something · 9m".into();
        let rows = desk_rows(
            &hand([card(7), push, stop, other]),
            |_| None,
            |_| None,
            |at| *at == address(7),
        );
        let cells = rows
            .waiting
            .iter()
            .map(|row| {
                (
                    row.subject.as_str(),
                    row.tag.as_deref(),
                    row.project.as_str(),
                    row.age.as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            cells,
            [
                // Cached from a host that is away.
                ("idea 7", Some("away"), "unformatted channel", "7m"),
                (
                    "idea 4",
                    Some("approve push"),
                    "unformatted channel",
                    "3.0h"
                ),
                ("idea 5", Some("failed"), "unformatted channel", "12m"),
                // A card that is no wait says no age.
                ("idea 6", None, "unformatted channel", ""),
            ]
        );
    }

    #[test]
    fn current_words_are_kept_for_the_phone_on_the_existing_card_only() {
        let hand = hand([7, 2].map(card));
        let with_flash = desk_rows(
            &hand,
            |_| None,
            |id| (*id == address(7)).then(|| "Running clock checks".into()),
            nowhere,
        );
        let without_flash = desk_rows(&hand, |_| None, |_| None, nowhere);
        assert_eq!(with_flash.wait_count, without_flash.wait_count);
        assert_eq!(with_flash.waiting[1], without_flash.waiting[1]);
        let row = &with_flash.waiting[0];
        assert_eq!(row.subject, without_flash.waiting[0].subject);
        assert_eq!(row.flash.as_deref(), Some("Running clock checks"));
        assert_eq!(row.tag, None, "current words are no exception");
    }

    #[test]
    fn waiting_comes_straight_from_the_hand_with_the_full_count() {
        // Ranking a second time by priority, subject, or project yields
        // different orders. Counting the displayed rows would say seven.
        let hand = hand([7, 94, 2, 71, 28, 16, 48, 44, 12].map(card));
        let rows = desk_rows(
            &hand,
            |address| {
                Some(Project {
                    address: address.clone(),
                    name: "shoal@lab".into(),
                })
            },
            |_| None,
            nowhere,
        );
        assert_eq!(rows.wait_count, 9);
        assert_eq!(
            rows.waiting
                .iter()
                .map(|row| row.subject.as_str())
                .collect::<Vec<_>>(),
            [
                "idea 7", "idea 94", "idea 2", "idea 71", "idea 28", "idea 16", "idea 48"
            ]
        );
        assert_eq!(rows.waiting[1].project, "shoal@lab");
        assert_eq!(rows.waiting[1].card, address(94));
    }

    #[test]
    fn folding_preserves_the_first_card_and_reports_hidden_cards_not_groups() {
        let mut cards = [7, 85, 2, 71, 28, 25, 48, 44, 12, 16, 9, 1, 3, 11].map(card);
        for index in [0, 2, 7, 9, 10, 12, 13] {
            cards[index].label = format!("failed · {index}m");
            cards[index].kind = CardKind::Stop("failed");
        }
        cards[10].label = "stuck · 1m".into();
        cards[10].kind = CardKind::Stop("stuck");
        cards[8].label = "waits · 6m".into();
        cards[8].kind = CardKind::Waits;
        let hand = hand(cards);
        let project = |address: &Address| {
            let id = match address.rest() {
                "t7" | "t2" | "t85" => 1,
                "t44" | "t16" | "t9" => 2,
                "t3" | "t11" => 3,
                _ => return None,
            };
            Some(Project {
                address: Address::new("project", id),
                name: if id == 3 { "clock@lab" } else { "reef@lab" }.into(),
            })
        };
        let rows = desk_rows(&hand, project, |_| None, nowhere);
        assert_eq!(rows.wait_count, 14);
        assert_eq!(rows.waiting[0].subject, "reef@lab");
        assert_eq!(rows.waiting[0].tag.as_deref(), Some("2 failed"));
        assert_eq!(rows.waiting[0].card, address(7));
        assert_eq!(rows.waiting[1].card, address(85));
        assert_eq!(rows.waiting[1].project, "reef@lab");
        assert_eq!(rows.waiting[6].card, address(44));
        assert_eq!(rows.waiting[6].tag.as_deref(), Some("3 stops"));
        let overflow = rows.overflow.unwrap();
        assert_eq!(overflow.first, address(12));
        assert_eq!(
            overflow.words,
            "and 4 more · unformatted channel · clock@lab"
        );
        let afterwards = Hand {
            cards: hand
                .cards
                .into_iter()
                .filter(|card| card.address != address(7))
                .collect(),
            next_change: None,
        };
        let rows = desk_rows(&afterwards, project, |_| None, nowhere);
        assert_eq!(rows.waiting[1].card, address(2));
        assert_eq!(rows.waiting[1].subject, "idea 2");
    }

    #[test]
    fn two_results_from_one_project_keep_their_subjects_and_name_hidden_cards() {
        let mut cards = [7, 85, 2, 71, 28, 25, 48, 44, 12, 16].map(card);
        for card in &mut cards[8..] {
            card.kind = CardKind::Waits;
            card.label = "waits · now".into();
        }
        let hand = hand(cards);
        let project = |_: &Address| {
            Some(Project {
                address: Address::new("project", 1),
                name: "clock@lab".into(),
            })
        };
        let rows = desk_rows(&hand, project, |_| None, nowhere);
        assert_eq!(rows.waiting[0].subject, "idea 7");
        assert_eq!(rows.waiting[1].subject, "idea 85");
        let overflow = rows.overflow.unwrap();
        assert_eq!(overflow.first, address(44));
        assert_eq!(overflow.words, "and 3 more · clock@lab");

        let hand = Hand {
            cards: hand.cards[8..].into(),
            next_change: None,
        };
        let rows = desk_rows(&hand, project, |_| None, nowhere);
        assert_eq!(
            rows.waiting
                .iter()
                .map(|row| row.subject.as_str())
                .collect::<Vec<_>>(),
            ["idea 12", "idea 16"]
        );
        assert!(rows.overflow.is_none());
    }

    fn row(n: u32, project: &str, age: &str) -> DeskRow {
        DeskRow {
            key: DeskKey::Waiting(address(n)),
            subject: format!("row {n}"),
            tag: None,
            project: project.into(),
            age: age.into(),
            flash: None,
            card: address(n),
        }
    }

    #[test]
    fn an_arrival_does_not_reorder_visible_rows() {
        let before = [row(5, "lab", ""), row(9, "lab", "")];
        let mut fresh = [
            row(3, "lab", ""),
            row(9, "lab", ""),
            row(5, "lab", ""),
            row(2, "lab", ""),
        ];
        DeskRows::keep_order(&mut fresh, &before);
        assert_eq!(
            fresh.map(|row| row.subject),
            ["row 5", "row 9", "row 3", "row 2"]
        );
    }
}

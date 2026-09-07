//! The transient buffer: a menu that opens under the point, takes one key,
//! and closes.
//!
//! Magit's transient, held to the ruling in `RHO-WINDOW-DESIGN.md`. A menu is
//! a title and rows of key and meaning; it is drawn at the bottom of the
//! window, in the editor's own text style, the way Magit's sits at the bottom
//! of the frame. The buffer above it is not reflowed and the point does not
//! move: the menu is a place on the screen the reader looks down to, not a
//! thing that happens to their text.
//!
//! Two things make it a window primitive rather than a screen's own widget.
//!
//! An item's action is a value of the caller's own type, not a closure over
//! the application: `Transient<A>` knows nothing about `A` except that it is
//! there to be handed back when the key is pressed. That is what lets this
//! crate name nothing above it, and what lets a source crate put an item in a
//! menu without naming the workspace.
//!
//! And a press is answered, not performed. [`Transient::press`] returns what
//! the key meant — run this item, take this digit as a count, dismiss, or
//! nothing is bound — and the caller does the doing. The menu holds no
//! handles, so it can be tested without a window and rendered by anything.
//!
//! The rule is one key and closed. An item may declare itself an infix, which
//! is Magit's distinction: a suffix exits, an infix (a toggle, a value the
//! next key needs) stays. Nothing else stays.

use gpui::prelude::*;
use gpui::{AnyElement, App, Keystroke, TextStyle, div};
use theme::ActiveTheme as _;

/// How tall a column of the grid is. Magit's number, and the reason the
/// grid exists: four rows read as one block, and a menu wraps sideways
/// instead of growing down the screen.
const COLUMN_ROWS: usize = 4;

/// What an item does to the menu when it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The rule: it runs and the menu closes.
    Suffix,
    /// A toggle or a value the next key needs. It runs and the menu stays,
    /// so several chain without reopening. An item has to say so.
    Infix,
}

/// One row: a key, what it means, and what to hand back when it is pressed.
pub struct Item<A> {
    /// Keystroke in binding notation: `d`, `shift-d`, `3`.
    key: String,
    description: String,
    /// An infix shows its current value apart from its description.
    value: Option<String>,
    action: A,
    kind: Kind,
}

impl<A> Item<A> {
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn action(&self) -> &A {
        &self.action
    }
}

/// A menu of keys over one subject, opened under the point.
pub struct Transient<A> {
    title: String,
    items: Vec<Item<A>>,
    counted: bool,
    count: Option<u32>,
}

/// What a key meant. The caller does the doing; the menu only says what.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    /// Run this item. `count` is the digits typed before it in a counted
    /// menu, and `closes` is false only for an infix.
    Run {
        item: usize,
        count: Option<u32>,
        closes: bool,
    },
    /// A digit in a counted menu: taken as a count for the next item, the
    /// menu stays and the caller redraws so the count shows.
    Count(u32),
    /// `escape` or `ctrl-g`. Nothing ran; close and leave the point alone.
    Dismiss,
    /// Nothing is bound to it. The menu stays: a mistyped key in a menu is
    /// not a reason to lose the menu.
    Unbound,
}

impl<A> Transient<A> {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            items: Vec::new(),
            counted: false,
            count: None,
        }
    }

    /// Digits in this menu are a count for the next item rather than keys of
    /// their own, vim style: `45 m` is forty-five minutes.
    pub fn counted(mut self) -> Self {
        self.counted = true;
        self
    }

    /// A suffix: it runs and the menu closes.
    pub fn item(self, key: impl Into<String>, description: impl Into<String>, action: A) -> Self {
        self.push(key, description, None, action, Kind::Suffix)
    }

    /// An infix: it runs, shows its value, and the menu stays.
    pub fn infix(
        self,
        key: impl Into<String>,
        description: impl Into<String>,
        value: impl Into<String>,
        action: A,
    ) -> Self {
        self.push(key, description, Some(value.into()), action, Kind::Infix)
    }

    /// Applicability at open, not at press: an item with nothing to act on —
    /// no agent selected, no thread under the point — is not in the menu at
    /// all, rather than in it and failing when pressed.
    pub fn when(
        self,
        applicable: bool,
        key: impl Into<String>,
        description: impl Into<String>,
        action: A,
    ) -> Self {
        if applicable {
            self.item(key, description, action)
        } else {
            self
        }
    }

    fn push(
        mut self,
        key: impl Into<String>,
        description: impl Into<String>,
        value: Option<String>,
        action: A,
        kind: Kind,
    ) -> Self {
        self.items.push(Item {
            key: key.into(),
            description: description.into(),
            value,
            action,
            kind,
        });
        self
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The rows, in order. One presentation path: whatever draws the menu —
    /// the block below, a touch list on the phone, a test — reads these.
    pub fn items(&self) -> &[Item<A>] {
        &self.items
    }

    /// The rows as the menu lays them out: columns of at most
    /// [`COLUMN_ROWS`], filled top to bottom and then left to right. The
    /// layout is answerable without a window, which is how it is tested.
    pub fn columns(&self) -> impl Iterator<Item = &[Item<A>]> {
        self.items.chunks(COLUMN_ROWS)
    }

    /// The count standing for the next item, when there is one.
    pub fn count(&self) -> Option<u32> {
        self.count
    }

    /// What this key means here.
    ///
    /// Cost: one pass over the rows of the menu, which is what is on the
    /// screen — per event O(touched), with nothing behind it that grows.
    pub fn press(&mut self, keystroke: &Keystroke) -> Press {
        if is_dismiss(keystroke) {
            return Press::Dismiss;
        }
        if self.counted
            && let Some(digit) = digit(keystroke)
        {
            let count = self
                .count
                .unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(digit);
            self.count = Some(count);
            return Press::Count(count);
        }
        let Some(item) = self
            .items
            .iter()
            .position(|item| matches(&item.key, keystroke))
        else {
            return Press::Unbound;
        };
        Press::Run {
            item,
            count: self.count.take(),
            closes: self.items[item].kind == Kind::Suffix,
        }
    }

    /// The menu as an element, drawn in the editor's own text style. Where it
    /// goes is the caller's: on the desk it is pinned to the bottom edge of
    /// the window, over the buffer rather than in it.
    ///
    /// Cost: per frame O(rows in the menu), and a menu is a screenful of keys
    /// at the most.
    pub fn render(&self, text_style: &TextStyle, cx: &App) -> AnyElement {
        let rows: Vec<(String, String, Option<String>)> = self
            .items
            .iter()
            .map(|item| {
                (
                    display_key(&item.key),
                    item.description.clone(),
                    item.value.clone(),
                )
            })
            .collect();
        render(&self.title, self.count, &rows, text_style, cx).into_any_element()
    }
}

fn render(
    title: &str,
    count: Option<u32>,
    rows: &[(String, String, Option<String>)],
    text_style: &TextStyle,
    cx: &App,
) -> impl IntoElement {
    let colors = cx.theme().colors();
    let accent = colors.text_accent;
    let muted = colors.text_muted;
    let value_color = colors.terminal_ansi_green;

    // Magit's grid, not a list: columns of four, filled top to bottom and
    // then left to right, wrapping across the width. A menu of
    // twenty-eight items down one column is the whole screen; the same
    // twenty-eight in sevens is one glance.
    let columns = rows.chunks(COLUMN_ROWS).map(|chunk| {
        div()
            .flex()
            .flex_col()
            .children(chunk.iter().map(|(key, description, value)| {
                let mut row = div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    // The keys line up down the column, so the eye runs
                    // down them rather than down ragged descriptions.
                    .child(
                        div()
                            .w_8()
                            .text_align(gpui::TextAlign::Right)
                            .pr_2()
                            .text_color(accent)
                            .child(key.clone()),
                    )
                    .child(div().child(description.clone()));
                if let Some(value) = value {
                    row = row
                        .child(div().pl_1().text_color(muted).child("("))
                        .child(
                            div()
                                .text_color(value_color)
                                .font_weight(gpui::FontWeight::BOLD)
                                .child(value.clone()),
                        )
                        .child(div().text_color(muted).child(")"));
                }
                row
            }))
    });

    let heading = match count {
        Some(count) => format!("{title} {count}"),
        None => title.to_owned(),
    };
    // A column, explicitly: a gpui div lays its children in a row.
    div()
        .block_mouse_except_scroll()
        .flex()
        .flex_col()
        .w_full()
        .font_family(text_style.font_family.clone())
        .text_size(text_style.font_size)
        .line_height(text_style.line_height)
        .child(
            div()
                .px_2()
                .flex()
                .gap_2()
                .child(div().font_weight(gpui::FontWeight::BOLD).child(heading)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_x_6()
                .px_2()
                .children(columns),
        )
}

/// `escape` and `ctrl-g` mean the same thing everywhere, so they mean it here
/// too: nothing ran, the menu is gone, the point has not moved.
fn is_dismiss(keystroke: &Keystroke) -> bool {
    (keystroke.key == "escape" && !keystroke.modifiers.control)
        || (keystroke.key == "g" && keystroke.modifiers.control)
}

fn digit(keystroke: &Keystroke) -> Option<u32> {
    if keystroke.modifiers.control || keystroke.modifiers.alt || keystroke.modifiers.platform {
        return None;
    }
    keystroke
        .key
        .chars()
        .next()
        .filter(|_| keystroke.key.chars().count() == 1)
        .and_then(|character| character.to_digit(10))
}

/// Binding notation against a real keystroke. A menu's keys are plain and
/// shifted letters; anything with a control or alt in it is not a menu key,
/// so a chord passes through unbound rather than matching by its letter.
fn matches(spec: &str, keystroke: &Keystroke) -> bool {
    let (shift, key) = match spec.strip_prefix("shift-") {
        Some(rest) => (true, rest),
        None => (false, spec),
    };
    keystroke.key == key
        && keystroke.modifiers.shift == shift
        && !keystroke.modifiers.control
        && !keystroke.modifiers.alt
        && !keystroke.modifiers.platform
}

/// What the reader sees for a key: `shift-d` is the `D` they have to type.
pub fn display_key(spec: &str) -> String {
    match spec.strip_prefix("shift-") {
        Some(rest) => rest.to_uppercase(),
        None => spec.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    enum Verdict {
        Done,
        Mute,
        Snooze,
        Room,
    }

    fn key(notation: &str) -> Keystroke {
        Keystroke::parse(notation).expect("a keystroke")
    }

    fn verdicts() -> Transient<Verdict> {
        Transient::new("verdict")
            .item("d", "done", Verdict::Done)
            .item("x", "mute", Verdict::Mute)
            .item("s", "snooze…", Verdict::Snooze)
            .item("shift-s", "snooze the room…", Verdict::Room)
    }

    /// The root menu is twenty-eight items. Down one column that is the
    /// whole screen; in Magit's grid it is seven columns of four, which is
    /// what the user asked for back.
    #[test]
    fn a_long_menu_lays_out_as_columns_of_four() {
        let mut menu = Transient::new("root");
        for index in 0..28 {
            menu = menu.item(format!("{index}"), format!("item {index}"), Verdict::Done);
        }
        let columns = menu.columns().collect::<Vec<_>>();
        assert_eq!(columns.len(), 7, "twenty-eight items in sevens");
        assert!(
            columns.iter().all(|column| column.len() == 4),
            "every column is four tall"
        );
        // Top to bottom, then left to right: the second column starts at
        // the fifth item, not at the second.
        assert_eq!(columns[1][0].description(), "item 4");
    }

    /// A short menu wraps at four all the same, so a five-item menu is a
    /// full column and a stub rather than one long list.
    #[test]
    fn a_short_menu_is_a_column_and_a_stub() {
        let mut menu = Transient::new("verdict");
        for index in 0..5 {
            menu = menu.item(format!("{index}"), format!("item {index}"), Verdict::Done);
        }
        let columns = menu.columns().collect::<Vec<_>>();
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].len(), 4);
        assert_eq!(columns[1].len(), 1);
    }

    #[test]
    fn one_key_runs_the_item_it_is_on_and_closes() {
        let mut menu = verdicts();
        let Press::Run { item, closes, .. } = menu.press(&key("x")) else {
            panic!("x is bound");
        };
        assert_eq!(*menu.items()[item].action(), Verdict::Mute);
        assert!(closes);
    }

    #[test]
    fn shift_is_its_own_key_not_the_letter() {
        let mut menu = verdicts();
        let Press::Run { item, .. } = menu.press(&key("shift-s")) else {
            panic!("shift-s is bound");
        };
        assert_eq!(*menu.items()[item].action(), Verdict::Room);

        let Press::Run { item, .. } = menu.press(&key("s")) else {
            panic!("s is bound");
        };
        assert_eq!(*menu.items()[item].action(), Verdict::Snooze);
    }

    #[test]
    fn a_chord_is_not_a_menu_key() {
        assert_eq!(verdicts().press(&key("ctrl-d")), Press::Unbound);
        assert_eq!(verdicts().press(&key("alt-d")), Press::Unbound);
    }

    #[test]
    fn an_unbound_key_keeps_the_menu() {
        let mut menu = verdicts();
        assert_eq!(menu.press(&key("q")), Press::Unbound);
        assert!(matches!(menu.press(&key("d")), Press::Run { .. }));
    }

    #[test]
    fn escape_and_ctrl_g_dismiss() {
        assert_eq!(verdicts().press(&key("escape")), Press::Dismiss);
        assert_eq!(verdicts().press(&key("ctrl-g")), Press::Dismiss);
    }

    #[test]
    fn an_infix_stays_and_a_suffix_does_not() {
        let mut menu = Transient::new("input")
            .infix("m", "model", "opus", Verdict::Done)
            .item("s", "send", Verdict::Mute);
        let Press::Run { closes, .. } = menu.press(&key("m")) else {
            panic!("m is bound");
        };
        assert!(!closes);
        let Press::Run { closes, .. } = menu.press(&key("s")) else {
            panic!("s is bound");
        };
        assert!(closes);
    }

    #[test]
    fn digits_are_keys_unless_the_menu_is_counted() {
        let mut plain = Transient::new("snooze").item("3", "three days", Verdict::Snooze);
        assert!(matches!(plain.press(&key("3")), Press::Run { .. }));

        let mut counted = Transient::new("snooze")
            .counted()
            .item("m", "minutes", Verdict::Snooze);
        assert_eq!(counted.press(&key("4")), Press::Count(4));
        assert_eq!(counted.press(&key("5")), Press::Count(45));
        assert_eq!(
            counted.press(&key("m")),
            Press::Run {
                item: 0,
                count: Some(45),
                closes: true,
            }
        );
        // The count belongs to the item it was typed for and to no other.
        assert_eq!(
            counted.press(&key("m")),
            Press::Run {
                item: 0,
                count: None,
                closes: true,
            }
        );
    }

    #[test]
    fn an_item_with_nothing_to_act_on_is_not_in_the_menu() {
        let menu = Transient::new("verdict")
            .item("d", "done", Verdict::Done)
            .when(false, "t", "go to its thread", Verdict::Snooze)
            .when(true, "x", "mute", Verdict::Mute);
        let keys: Vec<&str> = menu.items().iter().map(Item::key).collect();
        assert_eq!(keys, ["d", "x"]);
    }

    #[test]
    fn the_rows_are_what_is_drawn_and_what_a_key_reaches() {
        let menu = Transient::new("input").infix("m", "model", "opus", Verdict::Done);
        let item = &menu.items()[0];
        assert_eq!(display_key(item.key()), "m");
        assert_eq!(item.description(), "model");
        assert_eq!(item.value(), Some("opus"));
        assert_eq!(item.kind(), Kind::Infix);
        assert_eq!(display_key("shift-s"), "S");
    }
}

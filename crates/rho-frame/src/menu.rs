//! Menus as data: a row is a key, what it means, and what it does.
//!
//! A system hands the frame rows; the frame draws them under the point as
//! a transient block (`rho_window::transient`), reads the key, and runs
//! the row. A row that opens another menu carries its rows; the frame
//! keeps the menus a reader went down through, so escape returns one step
//! and `shift` closes them all. Menus are only the short keyed lists
//! (`window.Menu`): the `space` leader and the snooze times. Every other
//! choice is a [`crate::Command`] or a prompt.

use std::rc::Rc;

use gpui::{Context, Window};
use rho_window::transient::Transient;

use crate::frame::Frame;

/// What a row does when pressed. The count is the digits typed before it
/// in a counted menu.
pub type Run = Rc<dyn Fn(&mut Frame, Option<u32>, &mut Window, &mut Context<Frame>)>;

#[derive(Clone)]
pub enum Pick {
    /// Runs and closes the menu.
    Run { label: String, run: Run },
    /// Runs and leaves the menu open: a toggle, or a value the next key
    /// reads. The row shows the value.
    Infix {
        label: String,
        value: String,
        run: Run,
    },
    /// Opens these rows over this menu.
    Menu(Rows),
}

impl Pick {
    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Run { label, .. } | Self::Infix { label, .. } => label,
            Self::Menu(rows) => &rows.title,
        }
    }

    pub(crate) fn value(&self) -> Option<&str> {
        match self {
            Self::Infix { value, .. } => Some(value),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct Row {
    pub key: String,
    pub pick: Pick,
    pub(crate) snooze: Option<crate::when::SnoozeUnit>,
}

impl Row {
    pub fn run(
        key: impl Into<String>,
        description: impl Into<String>,
        run: impl Fn(&mut Frame, &mut Window, &mut Context<Frame>) + 'static,
    ) -> Self {
        Self {
            key: key.into(),
            pick: Pick::Run {
                label: description.into(),
                run: Rc::new(move |frame, _, window, cx| run(frame, window, cx)),
            },
            snooze: None,
        }
    }

    /// A row that reads the count typed before it: `45 m`.
    pub fn counted(
        key: impl Into<String>,
        description: impl Into<String>,
        run: impl Fn(&mut Frame, Option<u32>, &mut Window, &mut Context<Frame>) + 'static,
    ) -> Self {
        Self {
            key: key.into(),
            pick: Pick::Run {
                label: description.into(),
                run: Rc::new(run),
            },
            snooze: None,
        }
    }

    pub fn infix(
        key: impl Into<String>,
        description: impl Into<String>,
        value: impl Into<String>,
        run: impl Fn(&mut Frame, &mut Window, &mut Context<Frame>) + 'static,
    ) -> Self {
        Self {
            key: key.into(),
            pick: Pick::Infix {
                label: description.into(),
                value: value.into(),
                run: Rc::new(move |frame, _, window, cx| run(frame, window, cx)),
            },
            snooze: None,
        }
    }

    pub fn menu(key: impl Into<String>, rows: Rows) -> Self {
        Self {
            key: key.into(),
            pick: Pick::Menu(rows),
            snooze: None,
        }
    }
}

/// A menu: its title and rows. Counted when digits before a key mean a
/// count.
#[derive(Clone)]
pub struct Rows {
    pub title: String,
    pub rows: Vec<Row>,
    pub counted: bool,
}

impl Rows {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            rows: Vec::new(),
            counted: false,
        }
    }

    pub fn row(mut self, row: Row) -> Self {
        self.push(row);
        self
    }

    /// Adds a row unless its key is taken: the first to claim a key keeps
    /// it, and the loser is logged rather than silently drawn twice.
    pub fn push(&mut self, row: Row) {
        if let Some(taken) = self.rows.iter().find(|held| held.key == row.key) {
            tracing::warn!(
                menu = %self.title,
                key = %row.key,
                kept = %taken.pick.label(),
                dropped = %row.pick.label(),
                "two rows on one key"
            );
            return;
        }
        self.rows.push(row);
    }

    pub fn extend(&mut self, rows: impl IntoIterator<Item = Row>) {
        for row in rows {
            self.push(row);
        }
    }

    pub fn counted(mut self) -> Self {
        self.counted = true;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The block under the point: each item's action is its row's index.
    pub(crate) fn transient(&self) -> Transient<usize> {
        let mut transient = Transient::new(self.title.clone());
        if self.counted {
            transient = transient.counted();
        }
        for (index, row) in self.rows.iter().enumerate() {
            let label = row.pick.label().to_owned();
            transient = match row.pick.value() {
                Some(value) => transient.infix(row.key.clone(), label, value.to_owned(), index),
                None => transient.item(row.key.clone(), label, index),
            };
        }
        transient
    }
}

/// The menu that is open, and the menus under it.
pub(crate) struct OpenMenu {
    pub rows: Rows,
    pub transient: Transient<usize>,
    /// A count typed in this menu that belongs to the one it opened: `45 s
    /// m` is forty-five minutes, and the digits were typed before the `s`.
    pub carried_count: Option<u32>,
    /// The menus this one is standing on, oldest first.
    pub under: Vec<Rows>,
}

impl OpenMenu {
    pub fn resolve_times(&mut self) {
        let now = jiff::Zoned::now();
        let count = self
            .transient
            .count()
            .or(self.carried_count)
            .map_or(1, i64::from);
        for (index, row) in self.rows.rows.iter().enumerate() {
            if let Some(unit) = row.snooze {
                self.transient
                    .set_description(index, crate::when::snooze_target(unit, count, &now).1);
            }
        }
    }
}

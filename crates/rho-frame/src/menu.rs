//! Menus as data: a row is a key, what it means, and what it does.
//!
//! A system hands the frame rows; the frame draws them under the point as
//! a transient block (`rho_window::transient`), reads the key, and runs
//! the row. A row that opens another menu carries its rows; the frame
//! keeps the menus a reader went down through, so escape returns one step
//! and `shift` closes them all.

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
    Run(Run),
    /// Runs and leaves the menu open: a toggle, or a value the next key
    /// reads. The row shows the value.
    Infix(Run),
    /// Opens these rows over this menu.
    Menu(Rows),
}

#[derive(Clone)]
pub struct Row {
    pub key: String,
    pub description: String,
    pub value: Option<String>,
    pub pick: Pick,
}

impl Row {
    pub fn run(
        key: impl Into<String>,
        description: impl Into<String>,
        run: impl Fn(&mut Frame, &mut Window, &mut Context<Frame>) + 'static,
    ) -> Self {
        Self {
            key: key.into(),
            description: description.into(),
            value: None,
            pick: Pick::Run(Rc::new(move |frame, _, window, cx| run(frame, window, cx))),
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
            description: description.into(),
            value: None,
            pick: Pick::Run(Rc::new(run)),
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
            description: description.into(),
            value: Some(value.into()),
            pick: Pick::Infix(Rc::new(move |frame, _, window, cx| run(frame, window, cx))),
        }
    }

    pub fn menu(key: impl Into<String>, description: impl Into<String>, rows: Rows) -> Self {
        Self {
            key: key.into(),
            description: description.into(),
            value: None,
            pick: Pick::Menu(rows),
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
                kept = %taken.description,
                dropped = %row.description,
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
            transient = match (&row.pick, &row.value) {
                (Pick::Infix(_), value) => transient.infix(
                    row.key.clone(),
                    row.description.clone(),
                    value.clone().unwrap_or_default(),
                    index,
                ),
                _ => transient.item(row.key.clone(), row.description.clone(), index),
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

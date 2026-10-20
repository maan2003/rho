//! The platform with no terminal behind it, for tests: input is handed in
//! as the terminal would send it, and each frame lands in memory as the
//! cells the terminal would have shown.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseEvent, MouseEventKind,
};
use futures::channel::oneshot;

use crate::dispatcher::MainMsg;
use crate::grid::Grid;

/// What the platform and the test share.
#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) grid: Rc<RefCell<Option<Grid>>>,
    idle: RefCell<Vec<oneshot::Sender<()>>>,
}

impl Shared {
    /// The main loop has nothing left to do: wake whoever waits for that.
    pub(crate) fn idle(&self) {
        for waiter in self.idle.take() {
            let _ = waiter.send(());
        }
    }
}

/// A terminal for a test to type into and read back. It lives on the
/// main thread, beside the application it drives.
#[derive(Clone)]
pub struct Headless {
    pub(crate) sender: Sender<MainMsg>,
    pub(crate) shared: Rc<Shared>,
}

impl Headless {
    /// Hands the application an event as the terminal reports it.
    pub fn send(&self, event: Event) {
        let _ = self.sender.send(MainMsg::Input(event));
    }

    /// Types `text` one key at a time.
    pub fn type_text(&self, text: &str) {
        for ch in text.chars() {
            self.send(key_event(KeyCode::Char(ch), KeyModifiers::NONE));
        }
    }

    /// Presses one key, named as gpui's keymaps name it: `enter`,
    /// `escape`, `ctrl-c`, `shift-tab`, `alt-enter`, or a character.
    pub fn press(&self, key: &str) {
        let mut modifiers = KeyModifiers::NONE;
        let mut rest = key;
        while let Some((modifier, tail)) = rest.split_once('-').filter(|(_, tail)| !tail.is_empty())
        {
            modifiers |= match modifier {
                "ctrl" => KeyModifiers::CONTROL,
                "alt" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                _ => panic!("no modifier {modifier:?} in {key:?}"),
            };
            rest = tail;
        }
        let code = match rest {
            "enter" => KeyCode::Enter,
            "escape" => KeyCode::Esc,
            "tab" if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
            "tab" => KeyCode::Tab,
            "backspace" => KeyCode::Backspace,
            "space" => KeyCode::Char(' '),
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            single if single.chars().count() == 1 => KeyCode::Char(single.chars().next().unwrap()),
            _ => panic!("no key {rest:?} in {key:?}"),
        };
        self.send(key_event(code, modifiers));
    }

    /// Turns the mouse wheel a notch over a cell, back through what is
    /// shown when `up`.
    pub fn scroll(&self, col: u16, row: u16, up: bool) {
        self.send(Event::Mouse(MouseEvent {
            kind: if up {
                MouseEventKind::ScrollUp
            } else {
                MouseEventKind::ScrollDown
            },
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }));
    }

    /// The terminal changed size.
    pub fn resize(&self, cols: u16, rows: u16) {
        self.send(Event::Resize(cols, rows));
    }

    /// Resolves once the application has drawn everything it was handed
    /// and no work it started is left running. Pending timers do not
    /// count: a clock that ticks forever would never let it settle.
    pub async fn settled(&self) {
        let (sender, receiver) = oneshot::channel();
        self.shared.idle.borrow_mut().push(sender);
        let _ = self.sender.send(MainMsg::Wake);
        let _ = receiver.await;
    }

    /// The screen as text, a line per row with trailing blanks trimmed.
    pub fn text(&self) -> String {
        let grid = self.shared.grid.borrow();
        let Some(grid) = grid.as_ref() else {
            return String::new();
        };
        (0..grid.rows)
            .map(|row| grid.row_text(row).trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Where the terminal's cursor stands, if it is shown.
    pub fn cursor(&self) -> Option<(u16, u16)> {
        let grid = self.shared.grid.borrow();
        grid.as_ref()?.caret.map(|caret| (caret.col, caret.row))
    }

    /// One cell's background colour.
    pub fn background(&self, col: u16, row: u16) -> [u8; 3] {
        let grid = self.shared.grid.borrow();
        let grid = grid.as_ref().expect("a frame was drawn");
        grid.cells[row as usize * grid.cols as usize + col as usize].bg
    }
}

fn key_event(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

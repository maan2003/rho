//! Terminal input as gpui input. Keys follow `keystroke_from_xkb`'s
//! conventions so the stock keymaps (vim's included) match: a letter's key
//! is lower case with `shift` for capitals, a symbol's key is the symbol
//! itself with no `shift`, and control characters type nothing.

use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton as TermButton, MouseEvent,
    MouseEventKind,
};
use gpui::{
    KeyDownEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PlatformInput, ScrollDelta, ScrollWheelEvent, TouchPhase, point,
};

use crate::window::{WindowInner, cell_center};

fn modifiers(m: KeyModifiers) -> Modifiers {
    Modifiers {
        control: m.contains(KeyModifiers::CONTROL),
        alt: m.contains(KeyModifiers::ALT),
        shift: m.contains(KeyModifiers::SHIFT),
        platform: m.contains(KeyModifiers::SUPER),
        function: false,
    }
}

pub(crate) fn keystroke(event: &KeyEvent) -> Option<Keystroke> {
    let mut modifiers = modifiers(event.modifiers);
    let named = |name: &str| Some((name.to_owned(), None));
    let (key, key_char) = match event.code {
        KeyCode::Char(' ') => ("space".to_owned(), Some(" ".to_owned())),
        KeyCode::Char(c) => {
            let lower: String = c.to_lowercase().collect();
            if lower != c.to_string() {
                modifiers.shift = true;
            } else if lower.to_uppercase() == lower {
                // A symbol: its key already says what shift made it.
                modifiers.shift = false;
            }
            (lower, Some(c.to_string()))
        }
        KeyCode::Enter => named("enter")?,
        KeyCode::Tab => named("tab")?,
        KeyCode::BackTab => {
            modifiers.shift = true;
            named("tab")?
        }
        KeyCode::Backspace => named("backspace")?,
        KeyCode::Esc => named("escape")?,
        KeyCode::Delete => named("delete")?,
        KeyCode::Insert => named("insert")?,
        KeyCode::Left => named("left")?,
        KeyCode::Right => named("right")?,
        KeyCode::Up => named("up")?,
        KeyCode::Down => named("down")?,
        KeyCode::Home => named("home")?,
        KeyCode::End => named("end")?,
        KeyCode::PageUp => named("pageup")?,
        KeyCode::PageDown => named("pagedown")?,
        KeyCode::F(n) => (format!("f{n}"), None),
        _ => return None,
    };
    // Control and the platform key type nothing; alt still types, as xkb does.
    let key_char = key_char.filter(|_| !modifiers.control && !modifiers.platform);
    Some(Keystroke {
        modifiers,
        key,
        key_char,
    })
}

/// Turns repeated presses on one cell into gpui's click counts.
#[derive(Default)]
pub(crate) struct Clicks {
    last: Option<(Instant, u16, u16, usize)>,
}

const DOUBLE_CLICK: Duration = Duration::from_millis(400);

fn button(b: TermButton) -> MouseButton {
    match b {
        TermButton::Left => MouseButton::Left,
        TermButton::Right => MouseButton::Right,
        TermButton::Middle => MouseButton::Middle,
    }
}

pub(crate) fn dispatch(window: &WindowInner, clicks: &mut Clicks, event: Event) {
    match event {
        Event::Key(key) => {
            if key.kind == KeyEventKind::Release {
                return;
            }
            let Some(keystroke) = keystroke(&key) else {
                return;
            };
            window.state.borrow_mut().modifiers = keystroke.modifiers;
            window.handle_input(PlatformInput::KeyDown(KeyDownEvent {
                keystroke,
                is_held: key.kind == KeyEventKind::Repeat,
                prefer_character_input: false,
            }));
        }
        Event::Paste(text) => {
            let handler = window.state.borrow_mut().input_handler.take();
            if let Some(mut handler) = handler {
                handler.replace_text_in_range(None, &text);
                window.state.borrow_mut().input_handler = Some(handler);
            }
            window.frame_wanted.set(true);
        }
        Event::Mouse(mouse) => self::mouse(window, clicks, mouse),
        Event::Resize(cols, rows) => window.resized((cols, rows)),
        Event::FocusGained => window.set_active(true),
        Event::FocusLost => window.set_active(false),
    }
}

fn mouse(window: &WindowInner, clicks: &mut Clicks, event: MouseEvent) {
    let position = cell_center(event.column, event.row);
    let modifiers = modifiers(event.modifiers);
    {
        let mut state = window.state.borrow_mut();
        state.mouse = position;
        state.modifiers = modifiers;
    }
    let input = match event.kind {
        MouseEventKind::Down(b) => {
            let now = Instant::now();
            let count = match clicks.last {
                Some((at, col, row, count))
                    if now - at < DOUBLE_CLICK && (col, row) == (event.column, event.row) =>
                {
                    count + 1
                }
                _ => 1,
            };
            clicks.last = Some((now, event.column, event.row, count));
            window.state.borrow_mut().pressed = Some(button(b));
            PlatformInput::MouseDown(MouseDownEvent {
                button: button(b),
                position,
                modifiers,
                click_count: count,
                first_mouse: false,
            })
        }
        MouseEventKind::Up(b) => {
            window.state.borrow_mut().pressed = None;
            PlatformInput::MouseUp(MouseUpEvent {
                button: button(b),
                position,
                modifiers,
                click_count: clicks.last.map_or(1, |(.., count)| count),
            })
        }
        MouseEventKind::Drag(b) => PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: Some(button(b)),
            modifiers,
        }),
        MouseEventKind::Moved => PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers,
        }),
        kind @ (MouseEventKind::ScrollUp
        | MouseEventKind::ScrollDown
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight) => {
            let (x, y) = match kind {
                MouseEventKind::ScrollUp => (0.0, 3.0),
                MouseEventKind::ScrollDown => (0.0, -3.0),
                MouseEventKind::ScrollLeft => (3.0, 0.0),
                _ => (-3.0, 0.0),
            };
            PlatformInput::ScrollWheel(ScrollWheelEvent {
                position,
                delta: ScrollDelta::Lines(point(x, y)),
                modifiers,
                touch_phase: TouchPhase::Moved,
            })
        }
    };
    window.handle_input(input);
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEventKind, KeyEventState};

    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Keystroke {
        keystroke(&KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
        .expect("a key gpui knows")
    }

    /// The stock keymaps are written in xkb's terms; a terminal reports
    /// the same keys differently, and each difference is a binding that
    /// would silently never match.
    #[test]
    fn keys_read_as_xkb_reads_them() {
        let capital = key(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(
            (
                capital.key.as_str(),
                capital.modifiers.shift,
                capital.key_char.as_deref()
            ),
            ("g", true, Some("G"))
        );
        // Some terminals report a capital without shift.
        let capital = key(KeyCode::Char('G'), KeyModifiers::NONE);
        assert!(capital.modifiers.shift);
        let dollar = key(KeyCode::Char('$'), KeyModifiers::SHIFT);
        assert_eq!((dollar.key.as_str(), dollar.modifiers.shift), ("$", false));
        let control = key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            (
                control.key.as_str(),
                control.modifiers.control,
                control.key_char
            ),
            ("c", true, None)
        );
        let alt = key(KeyCode::Char('x'), KeyModifiers::ALT);
        assert_eq!(
            alt.key_char.as_deref(),
            Some("x"),
            "alt still types, as xkb has it"
        );
        assert_eq!(key(KeyCode::Char(' '), KeyModifiers::NONE).key, "space");
        let back = key(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!((back.key.as_str(), back.modifiers.shift), ("tab", true));
        let escape = key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!((escape.key.as_str(), escape.key_char), ("escape", None));
    }
}

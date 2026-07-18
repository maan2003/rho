//! Keystroke → PTY byte encoding against live terminal modes.
//!
//! Ported from zed's `terminal/src/mappings/keys.rs` (itself derived from
//! alacritty's default bindings), reshaped onto the wire's [`TermKeystroke`]
//! and alacritty's [`TermMode`] directly. Alt is always meta (the daemon has
//! no macOS "option sends option" convention to honor).

use std::borrow::Cow;

use alacritty_terminal::term::TermMode;
use rho_ui_proto::term::TermKeystroke;

#[derive(Debug, PartialEq, Eq)]
enum Modifiers {
    None,
    Alt,
    Ctrl,
    Shift,
    CtrlShift,
    Other,
}

impl Modifiers {
    fn new(ks: &TermKeystroke) -> Self {
        match (ks.alt, ks.ctrl, ks.shift) {
            (false, false, false) => Modifiers::None,
            (true, false, false) => Modifiers::Alt,
            (false, true, false) => Modifiers::Ctrl,
            (false, false, true) => Modifiers::Shift,
            (false, true, true) => Modifiers::CtrlShift,
            _ => Modifiers::Other,
        }
    }

    fn any(&self) -> bool {
        !matches!(self, Modifiers::None)
    }
}

/// The escape (or control) bytes for a keystroke, `None` when the key has no
/// terminal encoding beyond its plain text (the caller then falls back to
/// [`TermKeystroke::key_char`]).
pub(super) fn to_esc_str(keystroke: &TermKeystroke, mode: &TermMode) -> Option<Cow<'static, str>> {
    let modifiers = Modifiers::new(keystroke);
    let app_cursor = mode.contains(TermMode::APP_CURSOR);

    // Manual bindings including modifiers.
    let manual_esc_str: Option<&'static str> = match (keystroke.key.as_ref(), &modifiers) {
        // Basic special keys.
        ("tab", Modifiers::None) => Some("\x09"),
        ("escape", Modifiers::None) => Some("\x1b"),
        ("enter", Modifiers::None) => Some("\x0d"),
        ("enter", Modifiers::Shift) => Some("\x0a"),
        ("enter", Modifiers::Alt) => Some("\x1b\x0d"),
        ("backspace", Modifiers::None) => Some("\x7f"),
        // Interesting escape codes.
        ("tab", Modifiers::Shift) => Some("\x1b[Z"),
        ("backspace", Modifiers::Ctrl) => Some("\x08"),
        ("backspace", Modifiers::Alt) => Some("\x1b\x7f"),
        ("backspace", Modifiers::Shift) => Some("\x7f"),
        ("space", Modifiers::Ctrl) => Some("\x00"),
        ("home", Modifiers::None) if app_cursor => Some("\x1bOH"),
        ("home", Modifiers::None) => Some("\x1b[H"),
        ("end", Modifiers::None) if app_cursor => Some("\x1bOF"),
        ("end", Modifiers::None) => Some("\x1b[F"),
        ("up", Modifiers::None) if app_cursor => Some("\x1bOA"),
        ("up", Modifiers::None) => Some("\x1b[A"),
        ("down", Modifiers::None) if app_cursor => Some("\x1bOB"),
        ("down", Modifiers::None) => Some("\x1b[B"),
        ("right", Modifiers::None) if app_cursor => Some("\x1bOC"),
        ("right", Modifiers::None) => Some("\x1b[C"),
        ("left", Modifiers::None) if app_cursor => Some("\x1bOD"),
        ("left", Modifiers::None) => Some("\x1b[D"),
        ("back", Modifiers::None) => Some("\x7f"),
        ("insert", Modifiers::None) => Some("\x1b[2~"),
        ("delete", Modifiers::None) => Some("\x1b[3~"),
        ("pageup", Modifiers::None) => Some("\x1b[5~"),
        ("pagedown", Modifiers::None) => Some("\x1b[6~"),
        ("f1", Modifiers::None) => Some("\x1bOP"),
        ("f2", Modifiers::None) => Some("\x1bOQ"),
        ("f3", Modifiers::None) => Some("\x1bOR"),
        ("f4", Modifiers::None) => Some("\x1bOS"),
        ("f5", Modifiers::None) => Some("\x1b[15~"),
        ("f6", Modifiers::None) => Some("\x1b[17~"),
        ("f7", Modifiers::None) => Some("\x1b[18~"),
        ("f8", Modifiers::None) => Some("\x1b[19~"),
        ("f9", Modifiers::None) => Some("\x1b[20~"),
        ("f10", Modifiers::None) => Some("\x1b[21~"),
        ("f11", Modifiers::None) => Some("\x1b[23~"),
        ("f12", Modifiers::None) => Some("\x1b[24~"),
        ("f13", Modifiers::None) => Some("\x1b[25~"),
        ("f14", Modifiers::None) => Some("\x1b[26~"),
        ("f15", Modifiers::None) => Some("\x1b[28~"),
        ("f16", Modifiers::None) => Some("\x1b[29~"),
        ("f17", Modifiers::None) => Some("\x1b[31~"),
        ("f18", Modifiers::None) => Some("\x1b[32~"),
        ("f19", Modifiers::None) => Some("\x1b[33~"),
        ("f20", Modifiers::None) => Some("\x1b[34~"),
        // Caret notation keys.
        ("a", Modifiers::Ctrl) => Some("\x01"),
        ("a", Modifiers::CtrlShift) => Some("\x01"),
        ("b", Modifiers::Ctrl) => Some("\x02"),
        ("b", Modifiers::CtrlShift) => Some("\x02"),
        ("c", Modifiers::Ctrl) => Some("\x03"),
        ("c", Modifiers::CtrlShift) => Some("\x03"),
        ("d", Modifiers::Ctrl) => Some("\x04"),
        ("d", Modifiers::CtrlShift) => Some("\x04"),
        ("e", Modifiers::Ctrl) => Some("\x05"),
        ("e", Modifiers::CtrlShift) => Some("\x05"),
        ("f", Modifiers::Ctrl) => Some("\x06"),
        ("f", Modifiers::CtrlShift) => Some("\x06"),
        ("g", Modifiers::Ctrl) => Some("\x07"),
        ("g", Modifiers::CtrlShift) => Some("\x07"),
        ("h", Modifiers::Ctrl) => Some("\x08"),
        ("h", Modifiers::CtrlShift) => Some("\x08"),
        ("i", Modifiers::Ctrl) => Some("\x09"),
        ("i", Modifiers::CtrlShift) => Some("\x09"),
        ("j", Modifiers::Ctrl) => Some("\x0a"),
        ("j", Modifiers::CtrlShift) => Some("\x0a"),
        ("k", Modifiers::Ctrl) => Some("\x0b"),
        ("k", Modifiers::CtrlShift) => Some("\x0b"),
        ("l", Modifiers::Ctrl) => Some("\x0c"),
        ("l", Modifiers::CtrlShift) => Some("\x0c"),
        ("m", Modifiers::Ctrl) => Some("\x0d"),
        ("m", Modifiers::CtrlShift) => Some("\x0d"),
        ("n", Modifiers::Ctrl) => Some("\x0e"),
        ("n", Modifiers::CtrlShift) => Some("\x0e"),
        ("o", Modifiers::Ctrl) => Some("\x0f"),
        ("o", Modifiers::CtrlShift) => Some("\x0f"),
        ("p", Modifiers::Ctrl) => Some("\x10"),
        ("p", Modifiers::CtrlShift) => Some("\x10"),
        ("q", Modifiers::Ctrl) => Some("\x11"),
        ("q", Modifiers::CtrlShift) => Some("\x11"),
        ("r", Modifiers::Ctrl) => Some("\x12"),
        ("r", Modifiers::CtrlShift) => Some("\x12"),
        ("s", Modifiers::Ctrl) => Some("\x13"),
        ("s", Modifiers::CtrlShift) => Some("\x13"),
        ("t", Modifiers::Ctrl) => Some("\x14"),
        ("t", Modifiers::CtrlShift) => Some("\x14"),
        ("u", Modifiers::Ctrl) => Some("\x15"),
        ("u", Modifiers::CtrlShift) => Some("\x15"),
        ("v", Modifiers::Ctrl) => Some("\x16"),
        ("v", Modifiers::CtrlShift) => Some("\x16"),
        ("w", Modifiers::Ctrl) => Some("\x17"),
        ("w", Modifiers::CtrlShift) => Some("\x17"),
        ("x", Modifiers::Ctrl) => Some("\x18"),
        ("x", Modifiers::CtrlShift) => Some("\x18"),
        ("y", Modifiers::Ctrl) => Some("\x19"),
        ("y", Modifiers::CtrlShift) => Some("\x19"),
        ("z", Modifiers::Ctrl) => Some("\x1a"),
        ("z", Modifiers::CtrlShift) => Some("\x1a"),
        ("@", Modifiers::Ctrl) => Some("\x00"),
        ("[", Modifiers::Ctrl) => Some("\x1b"),
        ("\\", Modifiers::Ctrl) => Some("\x1c"),
        ("]", Modifiers::Ctrl) => Some("\x1d"),
        ("^", Modifiers::Ctrl) => Some("\x1e"),
        ("_", Modifiers::Ctrl) => Some("\x1f"),
        ("?", Modifiers::Ctrl) => Some("\x7f"),
        _ => None,
    };
    if let Some(esc_str) = manual_esc_str {
        return Some(Cow::Borrowed(esc_str));
    }

    // Automated bindings applying modifiers.
    if modifiers.any() {
        let code = modifier_code(keystroke);
        let modified_esc_str = match keystroke.key.as_ref() {
            "up" => Some(format!("\x1b[1;{code}A")),
            "down" => Some(format!("\x1b[1;{code}B")),
            "right" => Some(format!("\x1b[1;{code}C")),
            "left" => Some(format!("\x1b[1;{code}D")),
            "f1" => Some(format!("\x1b[1;{code}P")),
            "f2" => Some(format!("\x1b[1;{code}Q")),
            "f3" => Some(format!("\x1b[1;{code}R")),
            "f4" => Some(format!("\x1b[1;{code}S")),
            "f5" => Some(format!("\x1b[15;{code}~")),
            "f6" => Some(format!("\x1b[17;{code}~")),
            "f7" => Some(format!("\x1b[18;{code}~")),
            "f8" => Some(format!("\x1b[19;{code}~")),
            "f9" => Some(format!("\x1b[20;{code}~")),
            "f10" => Some(format!("\x1b[21;{code}~")),
            "f11" => Some(format!("\x1b[23;{code}~")),
            "f12" => Some(format!("\x1b[24;{code}~")),
            "f13" => Some(format!("\x1b[25;{code}~")),
            "f14" => Some(format!("\x1b[26;{code}~")),
            "f15" => Some(format!("\x1b[28;{code}~")),
            "f16" => Some(format!("\x1b[29;{code}~")),
            "f17" => Some(format!("\x1b[31;{code}~")),
            "f18" => Some(format!("\x1b[32;{code}~")),
            "f19" => Some(format!("\x1b[33;{code}~")),
            "f20" => Some(format!("\x1b[34;{code}~")),
            "insert" => Some(format!("\x1b[2;{code}~")),
            "delete" => Some(format!("\x1b[3;{code}~")),
            "pageup" => Some(format!("\x1b[5;{code}~")),
            "pagedown" => Some(format!("\x1b[6;{code}~")),
            "end" => Some(format!("\x1b[1;{code}F")),
            "home" => Some(format!("\x1b[1;{code}H")),
            _ => None,
        };
        if let Some(esc_str) = modified_esc_str {
            return Some(Cow::Owned(esc_str));
        }
    }

    // Alt is meta: prefix the (possibly shifted) ascii key with ESC.
    let is_alt_lowercase_ascii = modifiers == Modifiers::Alt && keystroke.key.is_ascii();
    let is_alt_uppercase_ascii = keystroke.alt && keystroke.shift && keystroke.key.is_ascii();
    if is_alt_lowercase_ascii || is_alt_uppercase_ascii {
        let key = if is_alt_uppercase_ascii {
            &keystroke.key.to_ascii_uppercase()
        } else {
            &keystroke.key
        };
        return Some(Cow::Owned(format!("\x1b{key}")));
    }

    None
}

/// The PTY bytes for a paste, honoring bracketed-paste mode (which also
/// filters ESC so pasted text cannot end the bracket early).
pub(super) fn encode_paste(text: &str, mode: &TermMode) -> Vec<u8> {
    if mode.contains(TermMode::BRACKETED_PASTE) {
        format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")).into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// xterm PC-style modifier codes: 1 + (shift | alt<<1 | ctrl<<2).
fn modifier_code(keystroke: &TermKeystroke) -> u32 {
    let mut code = 0;
    if keystroke.shift {
        code |= 1;
    }
    if keystroke.alt {
        code |= 1 << 1;
    }
    if keystroke.ctrl {
        code |= 1 << 2;
    }
    code + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ks(spec: &str) -> TermKeystroke {
        let mut keystroke = TermKeystroke::default();
        let mut parts = spec.split('-').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                keystroke.key = part.to_owned();
            } else {
                match part {
                    "ctrl" => keystroke.ctrl = true,
                    "alt" => keystroke.alt = true,
                    "shift" => keystroke.shift = true,
                    other => panic!("unknown modifier {other}"),
                }
            }
        }
        keystroke
    }

    #[test]
    fn application_cursor_mode() {
        let none = TermMode::empty();
        let app = TermMode::APP_CURSOR;
        assert_eq!(to_esc_str(&ks("up"), &none), Some("\x1b[A".into()));
        assert_eq!(to_esc_str(&ks("up"), &app), Some("\x1bOA".into()));
        assert_eq!(to_esc_str(&ks("home"), &none), Some("\x1b[H".into()));
        assert_eq!(to_esc_str(&ks("home"), &app), Some("\x1bOH".into()));
        assert_eq!(to_esc_str(&ks("shift-up"), &none), Some("\x1b[1;2A".into()));
        assert_eq!(to_esc_str(&ks("shift-up"), &app), Some("\x1b[1;2A".into()));
    }

    #[test]
    fn ctrl_codes_and_meta() {
        let mode = TermMode::empty();
        assert_eq!(to_esc_str(&ks("ctrl-a"), &mode), Some("\x01".into()));
        assert_eq!(
            to_esc_str(&ks("ctrl-shift-a"), &mode),
            to_esc_str(&ks("ctrl-a"), &mode)
        );
        assert_eq!(to_esc_str(&ks("alt-a"), &mode), Some("\x1ba".into()));
        assert_eq!(to_esc_str(&ks("alt-shift-a"), &mode), Some("\x1bA".into()));
        // Plain keys have no escape encoding; callers fall back to key_char.
        assert_eq!(to_esc_str(&ks("a"), &mode), None);
    }

    #[test]
    fn modifier_codes() {
        assert_eq!(modifier_code(&ks("shift-a")), 2);
        assert_eq!(modifier_code(&ks("alt-a")), 3);
        assert_eq!(modifier_code(&ks("shift-alt-a")), 4);
        assert_eq!(modifier_code(&ks("ctrl-a")), 5);
        assert_eq!(modifier_code(&ks("shift-ctrl-a")), 6);
        assert_eq!(modifier_code(&ks("alt-ctrl-a")), 7);
        assert_eq!(modifier_code(&ks("shift-ctrl-alt-a")), 8);
    }

    #[test]
    fn paste_modes() {
        let plain = TermMode::empty();
        let bracketed = TermMode::BRACKETED_PASTE;
        assert_eq!(encode_paste("a\r\nb\nc", &plain), b"a\rb\rc");
        assert_eq!(
            encode_paste("a\x1b[201~b", &bracketed),
            b"\x1b[200~a[201~b\x1b[201~"
        );
    }
}

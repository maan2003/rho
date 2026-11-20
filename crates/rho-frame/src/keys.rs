//! The frame's own keys. A system binds its own under the same contexts.
//!
//! rho's modal editing is Helix on top of vim (`init_vim_mode`), so a key
//! that means something in normal mode is bound for `vim_mode == normal`
//! and `vim_mode == helix_normal` both: [`normal_contexts`] says them.

use gpui::{App, KeyBinding, actions};

actions!(
    rho_frame,
    [
        /// Up through history.
        Back,
        /// Down through history, and from the newest entry the next card.
        Forward,
        /// Closes the surface in view.
        Close,
        /// Closes the surface in view and deals the next card.
        CloseAndDeal,
        /// `:`, and `ctrl-shift-;` where `:` would be typed: every command
        /// by name.
        Palette,
        /// `space`: the verdicts over the card in view; `space` again is
        /// Home.
        VerdictMenu,
        /// `tab`: Home, and from Home the way back.
        Home,
        OpenRow,
        FindAddress,
        UndoVerdict,
        MinibufferConfirm,
        MinibufferCancel,
        MinibufferNext,
        MinibufferPrevious,
        MinibufferComplete,
    ]
);

/// The contexts a normal-mode key of `surface` (a key context such as
/// `RhoFrame` or `RhoHome`) is bound under, with `more` appended to each.
pub fn normal_contexts(surface: &str, more: &str) -> [String; 2] {
    ["normal", "helix_normal"].map(|mode| format!("{surface} > Editor && vim_mode == {mode}{more}"))
}

/// Binds the frame's keys. Bound at both `RhoFrame` and `RhoFrame > Editor`:
/// the bundled keymaps bind some of these keys under plain `Editor`, and
/// gpui prefers the deeper match, so a root-level binding would lose
/// while typing.
pub fn bind(cx: &mut App) {
    cx.bind_keys(bindings());
    crate::thread::bind(cx);
}

fn bindings() -> Vec<KeyBinding> {
    let mut bindings = Vec::new();
    for context in ["RhoFrame", "RhoFrame > Editor"] {
        bindings.extend([
            KeyBinding::new("ctrl-k", Back, Some(context)),
            KeyBinding::new("f21", Back, Some(context)),
            KeyBinding::new("ctrl-j", Forward, Some(context)),
            KeyBinding::new("f20", Forward, Some(context)),
            KeyBinding::new("ctrl-shift-backspace", Close, Some(context)),
            KeyBinding::new("ctrl-shift-j", CloseAndDeal, Some(context)),
            KeyBinding::new("f16", CloseAndDeal, Some(context)),
            KeyBinding::new("ctrl-shift-f", FindAddress, Some(context)),
            // `ctrl-shift-;`, the palette where `:` would be typed: in a
            // terminal, on a desktop or a page. Spelled as gpui reads it on
            // Linux and macOS: shift turns `;` into `:` and is dropped, so
            // `ctrl-shift-;` would never match.
            KeyBinding::new("ctrl-:", Palette, Some(context)),
        ]);
    }
    for context in normal_contexts("RhoFrame", " && !showing_completions") {
        bindings.extend([
            KeyBinding::new(":", Palette, Some(&context)),
            KeyBinding::new("space", VerdictMenu, Some(&context)),
            KeyBinding::new("tab", Home, Some(&context)),
            KeyBinding::new("q", Close, Some(&context)),
        ]);
    }
    for context in normal_contexts("RhoFrame", " && vim_operator == none") {
        bindings.push(KeyBinding::new("shift-u", UndoVerdict, Some(&context)));
    }
    for context in normal_contexts("RhoHome", "") {
        bindings.push(KeyBinding::new("enter", OpenRow, Some(&context)));
    }
    bindings.extend([
        KeyBinding::new("enter", MinibufferConfirm, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("escape", MinibufferCancel, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("ctrl-g", MinibufferCancel, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("ctrl-n", MinibufferNext, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("down", MinibufferNext, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("ctrl-p", MinibufferPrevious, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("up", MinibufferPrevious, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("tab", MinibufferComplete, Some("RhoMinibuffer > Editor")),
    ]);
    bindings
}

#[cfg(test)]
mod tests {
    use gpui::{KeyContext, Keymap, Keystroke, Modifiers};

    use super::*;

    /// What the window hears when `ctrl-shift-;` is pressed in `surface`.
    fn chord_in(surface: &str) -> Vec<KeyBinding> {
        let mut keymap = Keymap::default();
        keymap.add_bindings(bindings());
        // As gpui on Linux delivers it: shift made `;` into `:`, and is
        // dropped because `:` is already the shifted key.
        let typed = Keystroke {
            modifiers: Modifiers {
                control: true,
                ..Modifiers::default()
            },
            key: ":".into(),
            key_char: Some(":".into()),
        };
        let stack = [
            KeyContext::parse("RhoFrame").unwrap(),
            KeyContext::parse(surface).unwrap(),
        ];
        keymap.bindings_for_input(&[typed], &stack).0.into_vec()
    }

    #[test]
    fn ctrl_shift_semicolon_opens_the_palette_from_a_place_that_takes_every_key() {
        // A raw terminal, a live desktop and a page bind none of it
        // themselves: the frame's chord reaches through each.
        for surface in ["RhoTerminal", "RhoDesktop", "RhoBrowser"] {
            let bindings = chord_in(surface);
            assert!(
                bindings
                    .iter()
                    .any(|binding| binding.action().as_any().is::<Palette>()),
                "ctrl-shift-; in {surface} matched {:?}",
                bindings
                    .iter()
                    .map(|b| b.action().name())
                    .collect::<Vec<_>>(),
            );
        }
    }
}

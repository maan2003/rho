//! The frame's own keys. A system binds its own under the same contexts.
//!
//! rho's modal editing is Helix on top of vim (`init_vim_mode`), so a key
//! that means something in normal mode is bound for `vim_mode == normal`
//! and `vim_mode == helix_normal` both: [`normal_contexts`] says them.

use gpui::{App, DummyKeyboardMapper, KeyBinding, KeyBindingContextPredicate, actions};

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
        /// Desk.
        VerdictMenu,
        /// `tab`: Desk, and from Desk the way back.
        Desk,
        OpenRow,
        PromptIdea,
        RedoIdeaOrUndoVerdict,
        SendIdea,
        CancelIdea,
        /// `/`: search forward in what is in view.
        SearchForward,
        /// `?`: search back.
        SearchBackward,
        /// `n`: the last search again, the same way.
        SearchNext,
        /// `N`: the last search again, the other way.
        SearchPrevious,
        UndoVerdict,
        MinibufferConfirm,
        MinibufferCancel,
        MinibufferNext,
        MinibufferPrevious,
        MinibufferComplete,
        MinibufferClear,
    ]
);

/// The contexts a normal-mode key of `surface` (a key context such as
/// `RhoFrame` or `RhoDesk`) is bound under, with `more` appended to each.
pub fn normal_contexts(surface: &str, more: &str) -> [String; 2] {
    ["normal", "helix_normal"].map(|mode| format!("{surface} > Editor && vim_mode == {mode}{more}"))
}

/// Binds the frame's keys. Bound at both `RhoFrame` and `RhoFrame > Editor`:
/// the bundled keymaps bind some of these keys under plain `Editor`, and
/// gpui prefers the deeper match, so a root-level binding would lose
/// while typing.
pub fn bind(cx: &mut App) {
    cx.bind_keys(bindings());
    cx.bind_keys([end_of_document(cx)]);
    crate::thread::bind(cx);
}

/// `G` as the end of the document, as vim has it. Helix keeps `G` for
/// goto-line, which does nothing without a count; a reader pressing it
/// in a transcript wants the newest message. `g e` stays as Helix has it.
/// vim's motions are not public types, so the action is built by name.
fn end_of_document(cx: &App) -> KeyBinding {
    let action = cx
        .build_action("vim::EndOfDocument", None)
        .expect("vim is initialised before the frame binds its keys");
    let context = KeyBindingContextPredicate::parse(
        "RhoFrame > Editor && vim_mode == helix_normal && vim_operator == none",
    )
    .expect("the context parses");
    KeyBinding::load(
        "shift-g",
        action,
        Some(context.into()),
        false,
        None,
        &DummyKeyboardMapper,
    )
    .expect("the keystroke parses")
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
            KeyBinding::new("ctrl-alt-m", PromptIdea, Some(context)),
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
            KeyBinding::new("tab", Desk, Some(&context)),
            KeyBinding::new("q", Close, Some(&context)),
        ]);
    }
    for context in normal_contexts("RhoFrame", " && vim_operator == none") {
        bindings.extend([
            KeyBinding::new("shift-u", UndoVerdict, Some(&context)),
            // vim's search keys reach for a Zed pane's search bar, which no
            // surface has; the frame searches the surface's editor instead.
            KeyBinding::new("/", SearchForward, Some(&context)),
            KeyBinding::new("?", SearchBackward, Some(&context)),
            KeyBinding::new("n", SearchNext, Some(&context)),
            KeyBinding::new("shift-n", SearchPrevious, Some(&context)),
        ]);
    }

    for context in normal_contexts("RhoDesk", " && vim_operator == none") {
        bindings.extend([
            KeyBinding::new("enter", OpenRow, Some(&context)),
            KeyBinding::new("shift-u", RedoIdeaOrUndoVerdict, Some(&context)),
        ]);
    }
    for context in normal_contexts("RhoIdeaUnaddressed", " && vim_operator == none") {
        for key in ["i", "shift-i", "a", "shift-a", "o", "shift-o"] {
            bindings.push(KeyBinding::new(key, PromptIdea, Some(&context)));
        }
    }
    bindings.extend([
        KeyBinding::new("ctrl-enter", SendIdea, Some("RhoIdea > Editor")),
        KeyBinding::new(
            "enter",
            editor::actions::Newline,
            Some("RhoIdeaOverlay > Editor"),
        ),
        KeyBinding::new("escape", CancelIdea, Some("RhoIdeaOverlay > Editor")),
        KeyBinding::new("ctrl-g", CancelIdea, Some("RhoIdeaOverlay > Editor")),
        KeyBinding::new("enter", MinibufferConfirm, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("escape", MinibufferCancel, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("ctrl-g", MinibufferCancel, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("ctrl-n", MinibufferNext, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("down", MinibufferNext, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("ctrl-p", MinibufferPrevious, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("up", MinibufferPrevious, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("tab", MinibufferComplete, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("ctrl-u", MinibufferClear, Some("RhoMinibuffer > Editor")),
    ]);
    bindings
}

#[cfg(test)]
mod tests {
    use gpui::{KeyContext, Keymap, Keystroke, Modifiers};

    use super::*;

    /// What the window hears when `ctrl-shift-;` is pressed in `surface`.
    fn chord_in(surface: &str) -> Vec<KeyBinding> {
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
        bindings_for(surface, typed)
    }

    fn bindings_for(surface: &str, typed: Keystroke) -> Vec<KeyBinding> {
        let mut keymap = Keymap::default();
        keymap.add_bindings(bindings());
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

    #[test]
    fn the_idea_chord_reaches_raw_places_and_typing_editors() {
        for surface in [
            "RhoTerminal",
            "RhoDesktop",
            "RhoBrowser",
            "Editor vim_mode=insert",
            "Editor vim_mode=helix_normal",
        ] {
            let matches = bindings_for(
                surface,
                Keystroke {
                    key: "m".into(),
                    key_char: None,
                    modifiers: Modifiers {
                        control: true,
                        alt: true,
                        ..Modifiers::default()
                    },
                },
            );
            assert!(
                matches
                    .first()
                    .is_some_and(|binding| binding.action().as_any().is::<PromptIdea>()),
                "the idea action must win in {surface}"
            );
        }
    }
}

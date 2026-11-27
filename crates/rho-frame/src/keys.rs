//! The frame's own keys. A system binds its own under the same contexts.
//!
//! rho's modal editing is Helix on top of vim (`init_vim_mode`), so a key
//! that means something in normal mode is bound for `vim_mode == normal`
//! and `vim_mode == helix_normal` both: [`normal_contexts`] says them.
//!
//! Navigation is by recency, never by a back-and-forward stack
//! (`window.Screen`): `space f` lists the places last visited ([`Places`],
//! from the [`Leader`]), `tab` is Desk, [`Deal`] opens the next card, and
//! `q` closes to the place visited before.
//!
//! A raw place (a page, a terminal, a desktop) is in one of two modes,
//! and the keymap here is the whole of what each means
//! (`window.raw places share one dual-escape model`). Its live input is
//! drawn in [`LIVE`]: every key goes to the guest except `escape`
//! ([`Normal`]), the [`os`] gestures, the commands chord and the idea
//! chord. Escaped, a place that is no buffer is drawn in [`PLACE`], where
//! the window's keys work as on any screen and `i` ([`Live`]) types
//! again. A place answers the three actions; it binds no escape of its own.

use gpui::{App, KeyBinding, KeyBindingContextPredicate, NoAction, actions};

/// The key context of a raw place's live input, beside the place's own.
pub const LIVE: &str = "RhoLive";

/// The key context of a screen that is no buffer while the window has
/// the keys: an escaped raw place, or one still connecting.
pub const PLACE: &str = "RhoPlace";

/// The key context of the Desk, a glance that is no buffer and takes no
/// cursor: only the window's keys and `i` work on it.
pub const DESK: &str = "RhoDesk";

/// The gestures the OS keyboard layer (`keyd`) turns into function keys
/// nothing types, so they reach the window even while a raw place takes
/// every key. The one table that says which key each gesture arrives as;
/// every binding below names the gesture. `f21` and `f24` are spare and
/// bound to nothing.
pub mod os {
    /// `space-j`: `space` held while `j` is pressed. [`super::Deal`].
    pub const SPACE_J: &str = "f20";
    /// `space-shift-j`: `space` held while `shift-j` is pressed.
    /// [`super::CloseAndDeal`].
    pub const SPACE_SHIFT_J: &str = "f16";
    /// `physical-escape`: the physical `escape` key, since a tapped
    /// `capslock` is the `escape` rho sees. [`super::GuestEscape`], a
    /// literal `escape` to the guest of a raw place.
    pub const PHYSICAL_ESCAPE: &str = "f9";
}

actions!(
    rho_frame,
    [
        /// `space-j`, or `ctrl-j` on an ordinary keyboard: the next card.
        Deal,
        /// `q`: closes the screen in view, back to the place visited
        /// before it.
        Close,
        /// `space-shift-j`, or `ctrl-shift-j`: closes the screen in view
        /// and deals the next card.
        CloseAndDeal,
        /// `:`, and `ctrl-shift-;` where `:` would be typed: every command
        /// by name (`window.Commands`).
        Commands,
        /// `space f`: every place by name, the most recent first
        /// (`window.Places`).
        Places,
        /// `space`: the silent prefix ([`crate::prefix`]): `f` places, `u`
        /// undo, and the card's verdicts on a waiting card.
        Leader,
        /// `tab`: Desk, and from Desk the place visited before it.
        Desk,
        /// `escape` in a raw place's live input: the window's keys (`NOR`).
        Normal,
        /// `i` in an escaped raw place: the guest's keys again (`INS`).
        Live,
        /// [`os::PHYSICAL_ESCAPE`] in a raw place's live input: a literal
        /// `escape` to the guest, which `escape` itself never reaches.
        GuestEscape,
        /// `ctrl-alt-m` anywhere, `i` on Desk: write an idea
        /// (`chat.Write an idea`). The window offers it; the system that
        /// keeps ideas answers it.
        PromptIdea,
        RedoEdit,
        /// `/`: search forward in what is in view.
        SearchForward,
        /// `?`: search back.
        SearchBackward,
        /// `n`: the last search again, the same way.
        SearchNext,
        /// `N`: the last search again, the other way.
        SearchPrevious,
        MinibufferConfirm,
        MinibufferCancel,
        MinibufferNext,
        MinibufferPrevious,
        /// `ctrl-w` on a prompt line ([`prompt_line`]).
        MinibufferDeleteWord,
        MinibufferComplete,
        /// `ctrl-u` on a prompt line ([`prompt_line`]).
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
    KeyBinding::load("shift-g", action, Some(context.into()), false, None, cx)
        .expect("the keystroke parses")
}

fn bindings() -> Vec<KeyBinding> {
    let mut bindings = Vec::new();
    for context in ["RhoFrame", "RhoFrame > Editor"] {
        // The ordinary-keyboard chords belong to the guest while a raw
        // place takes live input, and do nothing in a prompt. The negation
        // checks the whole focus stack, not just the context at which the
        // binding would otherwise match.
        let window = format!("({context}) && !{LIVE} && !RhoMinibuffer");
        bindings.extend([
            KeyBinding::new("ctrl-j", Deal, Some(&window)),
            KeyBinding::new("ctrl-shift-j", CloseAndDeal, Some(&window)),
            KeyBinding::new("ctrl-shift-backspace", Close, Some(&window)),
            // The OS gestures reach every place, live input included.
            KeyBinding::new(os::SPACE_J, Deal, Some(context)),
            KeyBinding::new(os::SPACE_SHIFT_J, CloseAndDeal, Some(context)),
            KeyBinding::new("ctrl-alt-m", PromptIdea, Some(context)),
            // `ctrl-shift-;`, the commands where `:` would be typed: in a
            // terminal, on a desktop or a page. Spelled as gpui reads it on
            // Linux and macOS: shift turns `;` into `:` and is dropped, so
            // `ctrl-shift-;` would never match.
            KeyBinding::new("ctrl-:", Commands, Some(context)),
        ]);
    }
    bindings.extend([
        KeyBinding::new("escape", Normal, Some(LIVE)),
        KeyBinding::new(os::PHYSICAL_ESCAPE, GuestEscape, Some(LIVE)),
        // The bundled keymap binds `ctrl-k ctrl-o` under no context, which
        // would hold every `ctrl-k` for the guest until the next key.
        KeyBinding::new("ctrl-k ctrl-o", NoAction, Some(LIVE)),
        KeyBinding::new(":", Commands, Some(PLACE)),
        KeyBinding::new("space", Leader, Some(PLACE)),
        KeyBinding::new("tab", Desk, Some(PLACE)),
        KeyBinding::new("q", Close, Some(PLACE)),
        KeyBinding::new("i", Live, Some(PLACE)),
    ]);
    for context in normal_contexts("RhoFrame", " && !showing_completions") {
        bindings.extend([
            KeyBinding::new(":", Commands, Some(&context)),
            KeyBinding::new("space", Leader, Some(&context)),
            KeyBinding::new("tab", Desk, Some(&context)),
            KeyBinding::new("q", Close, Some(&context)),
        ]);
    }
    for context in normal_contexts("RhoFrame", " && vim_operator == none") {
        bindings.extend([
            KeyBinding::new("shift-u", RedoEdit, Some(&context)),
            // vim's search keys reach for a Zed pane's search bar, which no
            // surface has; the frame searches the surface's editor instead.
            KeyBinding::new("/", SearchForward, Some(&context)),
            KeyBinding::new("?", SearchBackward, Some(&context)),
            KeyBinding::new("n", SearchNext, Some(&context)),
            KeyBinding::new("shift-n", SearchPrevious, Some(&context)),
        ]);
    }

    // The Desk is no buffer: its few keys are bound on the view itself.
    bindings.extend([
        KeyBinding::new(":", Commands, Some(DESK)),
        KeyBinding::new("space", Leader, Some(DESK)),
        KeyBinding::new("tab", Desk, Some(DESK)),
        KeyBinding::new("i", PromptIdea, Some(DESK)),
    ]);
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
    bindings.extend(prompt_line("RhoMinibuffer"));
    bindings
}

/// The editing keys of every one-line prompt (`window.Prompt`): the
/// minibuffer's, and a screen's own find line under `context`, which
/// answers [`MinibufferDeleteWord`] and [`MinibufferClear`] for its own
/// editor ([`crate::minibuffer::delete_word`]).
pub fn prompt_line(context: &str) -> [KeyBinding; 3] {
    let editor = format!("{context} > Editor");
    [
        KeyBinding::new("ctrl-w", MinibufferDeleteWord, Some(&editor)),
        KeyBinding::new("ctrl-u", MinibufferClear, Some(&editor)),
        KeyBinding::new(
            "ctrl-a",
            editor::actions::MoveToBeginningOfLine {
                stop_at_soft_wraps: false,
                stop_at_indent: false,
            },
            Some(&editor),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use gpui::{Action as _, KeyContext, Keymap, Keystroke, Modifiers};

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
    fn ctrl_shift_semicolon_opens_commands_from_a_place_that_takes_every_key() {
        // A raw terminal, a live desktop and a page bind none of it
        // themselves: the frame's chord reaches through each.
        for surface in LIVE_PLACES {
            let bindings = chord_in(surface);
            assert!(
                bindings
                    .iter()
                    .any(|binding| binding.action().as_any().is::<Commands>()),
                "ctrl-shift-; in {surface} matched {:?}",
                bindings
                    .iter()
                    .map(|b| b.action().name())
                    .collect::<Vec<_>>(),
            );
        }
    }

    /// The focus stacks of the three raw places, live and escaped, as the
    /// places draw them.
    const LIVE_PLACES: [&str; 3] = [
        "RhoBrowser RhoLive",
        "RhoTerminal RhoLive",
        "RhoDesktop RhoLive",
    ];
    const ESCAPED_PLACES: [&str; 2] = ["RhoBrowser RhoPlace", "RhoDesktop RhoPlace"];

    fn action_at(surface: &str, key: &'static str) -> Option<&'static str> {
        bindings_for(surface, Keystroke::parse(key).unwrap())
            .first()
            .map(|binding| binding.action().name())
    }

    #[test]
    fn live_input_leaves_every_window_chord_to_the_guest_but_the_escapes() {
        for place in LIVE_PLACES {
            for key in [
                "ctrl-k",
                "ctrl-j",
                "ctrl-shift-backspace",
                "ctrl-shift-j",
                "space",
                "q",
                "tab",
                "i",
                "shift-;",
                // The per-place chords this model replaced.
                "ctrl-shift-f9",
                "ctrl-shift-f10",
                "ctrl-shift-f11",
                "ctrl-shift-n",
            ] {
                assert_eq!(action_at(place, key), None, "{key} in {place}");
            }
            assert_eq!(action_at(place, "escape"), Some(Normal.name()), "{place}");
            assert_eq!(
                action_at(place, os::PHYSICAL_ESCAPE),
                Some(GuestEscape.name()),
                "{place}"
            );
        }
    }

    /// The Desk is a glance with no cursor: the window's keys and `i` work
    /// on it, and the motion keys of a buffer do nothing.
    #[test]
    fn the_desk_takes_the_window_keys_and_no_motion() {
        assert_eq!(action_at(DESK, ":"), Some(Commands.name()));
        assert_eq!(action_at(DESK, "space"), Some(Leader.name()));
        assert_eq!(action_at(DESK, "tab"), Some(Desk.name()));
        assert_eq!(action_at(DESK, "i"), Some(PromptIdea.name()));
        assert_eq!(action_at(DESK, "ctrl-j"), Some(Deal.name()));
        for motion in ["j", "k", "enter", "shift-g", "q"] {
            assert_eq!(action_at(DESK, motion), None, "{motion}");
        }
    }

    /// The OS gestures deal from everywhere, live input included, and the
    /// spare function keys mean nothing anywhere.
    #[test]
    fn the_os_gestures_deal_everywhere_and_the_spare_keys_stay_unbound() {
        assert_eq!(
            (os::SPACE_J, os::SPACE_SHIFT_J, os::PHYSICAL_ESCAPE),
            ("f20", "f16", "f9"),
            "keyd sends these; change both sides together"
        );
        let everywhere = LIVE_PLACES.into_iter().chain(ESCAPED_PLACES).chain([
            "Editor vim_mode=helix_normal",
            "Editor vim_mode=insert",
            "RhoDesk",
        ]);
        for surface in everywhere {
            assert_eq!(
                action_at(surface, os::SPACE_J),
                Some(Deal.name()),
                "{surface}"
            );
            assert_eq!(
                action_at(surface, os::SPACE_SHIFT_J),
                Some(CloseAndDeal.name()),
                "{surface}"
            );
            for spare in ["f21", "f24"] {
                assert_eq!(action_at(surface, spare), None, "{spare} in {surface}");
            }
        }
    }

    #[test]
    fn an_escaped_place_has_the_same_screen_keys_as_any_buffer() {
        let buffer = "Editor vim_mode=helix_normal";
        for (key, action) in [
            ("ctrl-j", Deal.name()),
            ("ctrl-shift-j", CloseAndDeal.name()),
            ("tab", Desk.name()),
            ("q", Close.name()),
            ("space", Leader.name()),
            (":", Commands.name()),
        ] {
            assert_eq!(action_at(buffer, key), Some(action), "{key} in a buffer");
            for place in ESCAPED_PLACES {
                assert_eq!(action_at(place, key), Some(action), "{key} in {place}");
            }
        }
        for place in ESCAPED_PLACES.into_iter().chain([buffer]) {
            // No back and forward: the chords that once walked a history
            // are nothing of the window's.
            assert_eq!(action_at(place, "ctrl-k"), None, "{place}");
            assert_eq!(action_at(place, "f21"), None, "{place}");
        }
        for place in ESCAPED_PLACES {
            assert_eq!(action_at(place, "i"), Some(Live.name()), "{place}");
            // Escaped, `escape` and the physical escape are nothing of the
            // window's.
            assert_eq!(action_at(place, "escape"), None, "{place}");
            assert_eq!(action_at(place, os::PHYSICAL_ESCAPE), None, "{place}");
        }
    }

    #[test]
    fn a_context_free_prefix_does_not_hold_ctrl_k_from_the_guest() {
        // As the bundled keymap has it: bound under no context, before the
        // frame's own.
        let mut keymap = Keymap::default();
        keymap.add_bindings([KeyBinding::new("ctrl-k ctrl-o", Commands, None)]);
        keymap.add_bindings(bindings());
        let typed = [Keystroke::parse("ctrl-k").unwrap()];
        let stack = |place: &str| {
            [
                KeyContext::parse("RhoFrame").unwrap(),
                KeyContext::parse(place).unwrap(),
            ]
        };
        for place in LIVE_PLACES {
            let (matched, pending) = keymap.bindings_for_input(&typed, &stack(place));
            assert!(matched.is_empty() && !pending, "ctrl-k waits in {place}");
        }
        // Without the live unbinding, the prefix would hold it.
        let mut bundled = Keymap::default();
        bundled.add_bindings([KeyBinding::new("ctrl-k ctrl-o", Commands, None)]);
        bundled.add_bindings(
            bindings()
                .into_iter()
                .filter(|binding| !binding.action().as_any().is::<NoAction>()),
        );
        let (_, pending) = bundled.bindings_for_input(&typed, &stack(LIVE_PLACES[0]));
        assert!(pending);
    }

    #[test]
    fn prompt_arrows_bind_to_the_same_row_motion_as_ctrl_p_n() {
        let mut keymap = Keymap::default();
        keymap.add_bindings(bindings());
        let stack = [
            KeyContext::parse("RhoMinibuffer").unwrap(),
            KeyContext::parse("Editor").unwrap(),
        ];
        for (key, expected) in [
            ("up", MinibufferPrevious.name()),
            ("ctrl-p", MinibufferPrevious.name()),
            ("down", MinibufferNext.name()),
            ("ctrl-n", MinibufferNext.name()),
        ] {
            let (matches, _) = keymap.bindings_for_input(&[Keystroke::parse(key).unwrap()], &stack);
            assert_eq!(matches[0].action().name(), expected, "{key}");
        }
    }

    #[test]
    fn prompt_ctrl_a_wins_over_the_editors_select_all() {
        let mut keymap = Keymap::default();
        keymap.add_bindings([KeyBinding::new(
            "ctrl-a",
            editor::actions::SelectAll,
            Some("Editor"),
        )]);
        keymap.add_bindings(bindings());
        keymap.add_bindings(prompt_line("RhoNotesQuery"));
        for prompt in ["RhoMinibuffer", "RhoNotesQuery"] {
            let stack = [
                KeyContext::parse(prompt).unwrap(),
                KeyContext::parse("Editor").unwrap(),
            ];
            let typed = Keystroke::parse("ctrl-a").unwrap();
            let (matches, _) = keymap.bindings_for_input(&[typed], &stack);
            let action = matches[0].action().as_any();
            assert!(
                action.downcast_ref::<editor::actions::MoveToBeginningOfLine>()
                    == Some(&editor::actions::MoveToBeginningOfLine {
                        stop_at_soft_wraps: false,
                        stop_at_indent: false,
                    }),
                "{prompt}"
            );
        }
    }

    #[test]
    fn the_idea_chord_reaches_raw_places_and_typing_editors() {
        for surface in [
            "RhoTerminal RhoLive",
            "RhoDesktop RhoLive",
            "RhoBrowser RhoLive",
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

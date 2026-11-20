//! The keys of a thread screen, the same on every thread the window
//! shows, whether a channel or a thread on a host.
//!
//! The mails and draft share one editor under [`SCREEN`].

use gpui::{App, FocusHandle, KeyBinding, Window, actions};
pub use vim::enter_normal_mode;

actions!(
    rho_thread,
    [
        /// Posts the draft; in a channel on a host, as an idea an
        /// agent starts from.
        SendMessage,
        /// Posts the draft and starts nothing, in a channel too.
        SendOnly,
        /// Opens what the cursor is on: a thread under a message, the
        /// thread above, a link, or a quote.
        OpenRow,
        /// Quotes the selected lines of a mail in the draft.
        QuoteSelection,
        /// Helix redo in the draft, or undo verdict on the history.
        RedoDraftOrUndoVerdict,
    ]
);

/// The key context of the whole screen.
pub const SCREEN: &str = "RhoThread";
/// The project list has a second way to send an idea.
pub const PROJECT_SCREEN: &str = "RhoThread RhoProjectThread";
/// Starts editing in the thread's already focused editor, as `i` would.
pub fn enter_insert_mode(focus: &FocusHandle, window: &mut Window, cx: &mut App) {
    let action = cx
        .build_action("vim::InsertBefore", None)
        .expect("vim is initialised before a thread opens");
    focus.dispatch_action(action.as_ref(), window, cx);
}

/// A message's time on the user's clock, as every thread says it: the hour
/// when it is today, the day in front of it when it is not.
pub fn when(at_ms: i64) -> String {
    let at = jiff::Timestamp::from_millisecond(at_ms)
        .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
        .to_zoned(jiff::tz::TimeZone::system());
    let now = jiff::Zoned::now();
    match at.date() == now.date() {
        true => at.strftime("%H:%M").to_string(),
        false => at.strftime("%a %-d %b %H:%M").to_string(),
    }
}

pub(crate) fn bind(cx: &mut App) {
    let screen = format!("{SCREEN} > Editor");
    let mut bindings = vec![
        KeyBinding::new("ctrl-enter", SendMessage, Some(&screen)),
        KeyBinding::new(
            "ctrl-shift-enter",
            SendOnly,
            Some("RhoProjectThread > Editor"),
        ),
    ];
    for context in
        crate::normal_contexts(SCREEN, " && vim_operator == none && !showing_completions")
    {
        bindings.push(KeyBinding::new("enter", OpenRow, Some(&context)));
    }
    bindings.push(KeyBinding::new(
        "shift-u",
        RedoDraftOrUndoVerdict,
        Some(&format!(
            "{screen} && vim_mode == helix_normal && vim_operator == none"
        )),
    ));
    for mode in ["visual", "helix_select"] {
        let context = format!("{screen} && vim_mode == {mode}");
        bindings.push(KeyBinding::new("enter", QuoteSelection, Some(&context)));
    }
    cx.bind_keys(bindings);
}

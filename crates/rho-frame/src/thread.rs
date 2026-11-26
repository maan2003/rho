//! The keys of a thread screen, the same on every thread the window
//! shows.
//!
//! The mails and draft share one editor under [`SCREEN`].

use gpui::{App, KeyBinding, actions};
pub use vim::enter_normal_mode;

actions!(
    rho_thread,
    [
        /// Posts the draft as the thread's next message.
        SendMessage,
        /// Opens what the cursor is on, whatever the selection's width: a
        /// private reply, a tangent, a link, or a message's work.
        OpenRow,
        /// `>`: quotes the selected lines of a read-only message in the
        /// draft. In the draft itself it lets the editor indent.
        QuoteSelection,
    ]
);

/// The key context of the whole screen.
pub const SCREEN: &str = "RhoThread";
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
    let mut bindings = vec![KeyBinding::new("ctrl-enter", SendMessage, Some(&screen))];
    // A selection from `w`, `x`, `/` or `v` leaves `enter` what it is:
    // the key that opens what is under the cursor. Quoting is `>`'s.
    for mode in ["normal", "helix_normal", "visual", "helix_select"] {
        let context = format!("{screen} && vim_mode == {mode} && vim_operator == none");
        bindings.push(KeyBinding::new(
            "enter",
            OpenRow,
            Some(&format!("{context} && !showing_completions")),
        ));
        bindings.push(KeyBinding::new(">", QuoteSelection, Some(&context)));
    }
    cx.bind_keys(bindings);
}

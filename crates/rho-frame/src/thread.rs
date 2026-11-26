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
        /// Opens what the cursor is on: a thread under a message, the
        /// thread above, a link, or a quote.
        OpenRow,
        /// Quotes the selected lines of a mail in the draft.
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
    for context in
        crate::normal_contexts(SCREEN, " && vim_operator == none && !showing_completions")
    {
        bindings.push(KeyBinding::new("enter", OpenRow, Some(&context)));
    }
    for mode in ["visual", "helix_select"] {
        let context = format!("{screen} && vim_mode == {mode}");
        bindings.push(KeyBinding::new("enter", QuoteSelection, Some(&context)));
    }
    cx.bind_keys(bindings);
}

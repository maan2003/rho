//! The keys of a thread screen, the same on every thread the window
//! shows: a channel or a thread on a host, and a Slack conversation or a
//! Slack thread (`chat.Thread` in the product model).
//!
//! A thread screen is the messages, an editor drawn under [`LOG`], above a
//! box to write in, an editor drawn under [`COMPOSER`], both inside
//! [`SCREEN`]. A system that draws one in these contexts and handles these
//! actions has the thread's keys, and binds none of its own for them.

use gpui::{App, KeyBinding, actions};

actions!(
    rho_thread,
    [
        /// Posts what is in the box; in a channel on a host, as an idea an
        /// agent starts from.
        SendMessage,
        /// Posts what is in the box and starts nothing, in a channel too.
        SendOnly,
        /// Puts the keyboard in the box.
        FocusComposer,
        /// Puts the keyboard in the messages.
        FocusLog,
        /// Opens what the cursor is on: a thread under a message, the
        /// thread above, a link, or a quote.
        OpenRow,
    ]
);

/// The key context of the whole screen.
pub const SCREEN: &str = "RhoThread";
/// The key context the messages are drawn in.
pub const LOG: &str = "RhoThreadLog";
/// The key context the box to write in is drawn in.
pub const COMPOSER: &str = "RhoThreadComposer";

/// What the empty box says.
pub const PLACEHOLDER: &str = "reply · ctrl-enter sends";
/// What the empty box of a channel on a host says, where a message is an
/// idea an agent starts from.
pub const CHANNEL_PLACEHOLDER: &str =
    "idea · ctrl-enter starts an agent · ctrl-shift-enter only sends";

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
    let composer = format!("{COMPOSER} > Editor && !showing_completions");
    let mut bindings = vec![
        KeyBinding::new("ctrl-enter", SendMessage, Some(&screen)),
        KeyBinding::new("ctrl-shift-enter", SendOnly, Some(&screen)),
        KeyBinding::new("ctrl-down", FocusComposer, Some(&screen)),
        KeyBinding::new("ctrl-up", FocusLog, Some(&screen)),
        // The box is an input, always in insert: escape is the way back to
        // the messages, where the vim keys are. With a completion menu open
        // escape and enter are the menu's.
        KeyBinding::new("escape", FocusLog, Some(&composer)),
        // Only `ctrl-enter` sends, so `enter` is free to break the line, the
        // way it does in every other box that takes paragraphs.
        KeyBinding::new("enter", editor::actions::Newline, Some(&composer)),
    ];
    for context in crate::normal_contexts(LOG, "") {
        bindings.extend([
            KeyBinding::new("i", FocusComposer, Some(&context)),
            KeyBinding::new("enter", OpenRow, Some(&context)),
        ]);
    }
    cx.bind_keys(bindings);
}

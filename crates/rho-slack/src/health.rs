//! Whether rho can still be trusted to know what Slack knows.
//!
//! A dropped socket is ordinary: it reconnects in a second and the feed
//! covers the gap. What is not ordinary is being *out* for minutes, or a feed
//! that keeps refusing, because then a mention can be sitting unread with
//! nothing on screen saying so. That is the only case worth a notice and a
//! lamp, and it clears only once a catch-up poll has actually succeeded.

use std::time::Duration;

/// How long an outage may last before it is worth telling the user about.
/// Shorter than this is a reconnect they should never have to think about.
pub const OUTAGE_GRACE: Duration = Duration::from_secs(180);
/// Consecutive failed polls before the feed counts as broken. One is a
/// hiccup; three in a row is Slack refusing us.
pub const FEED_FAILURE_LIMIT: u32 = 3;

/// What the GUI should do about a change in health.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    /// Say so, and light the lamp.
    Degraded(String),
    /// Say so, and put the lamp out.
    Recovered,
}

#[derive(Debug, Default)]
pub struct Health {
    connected: bool,
    disconnected_since_ms: Option<i64>,
    feed_failures: u32,
    reason: Option<String>,
    /// Set when a socket comes back while degraded: the lamp stays lit until
    /// the catch-up poll that follows has landed, because until then rho
    /// still does not know what it missed.
    awaiting_catch_up: bool,
}

impl Health {
    pub fn is_degraded(&self) -> bool {
        self.reason.is_some()
    }

    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    pub fn connected(&mut self, _now_ms: i64) -> Option<Signal> {
        self.connected = true;
        self.disconnected_since_ms = None;
        if self.is_degraded() {
            self.awaiting_catch_up = true;
        }
        None
    }

    pub fn disconnected(&mut self, now_ms: i64) -> Option<Signal> {
        self.connected = false;
        self.disconnected_since_ms.get_or_insert(now_ms);
        None
    }

    /// Called on every poll result and on the clock, so an outage that never
    /// produces another event still lights the lamp.
    pub fn tick(&mut self, now_ms: i64) -> Option<Signal> {
        let out_for = self.disconnected_since_ms.map(|since| now_ms - since);
        match out_for {
            Some(elapsed) if elapsed >= OUTAGE_GRACE.as_millis() as i64 => {
                self.degrade("slack: connection lost")
            }
            _ => None,
        }
    }

    pub fn feed_failed(&mut self, error: &str) -> Option<Signal> {
        self.feed_failures = self.feed_failures.saturating_add(1);
        tracing::warn!(
            error,
            failures = self.feed_failures,
            "slack feed poll failed"
        );
        match self.feed_failures >= FEED_FAILURE_LIMIT {
            true => self.degrade("slack: cannot reach the activity feed"),
            false => None,
        }
    }

    /// A poll landed. This is the only thing that clears a degraded session:
    /// the feed is what fills the gap, so until one succeeds the catch-up has
    /// not happened.
    pub fn feed_ok(&mut self) -> Option<Signal> {
        self.feed_failures = 0;
        self.awaiting_catch_up = false;
        match (self.connected, self.reason.take()) {
            (true, Some(_)) => Some(Signal::Recovered),
            // Still no socket: the poll proves the network, not the session.
            (false, reason) => {
                self.reason = reason;
                None
            }
            (true, None) => None,
        }
    }

    fn degrade(&mut self, reason: &str) -> Option<Signal> {
        match self.reason.as_deref() == Some(reason) {
            true => None,
            false => {
                self.reason = Some(reason.to_owned());
                Some(Signal::Degraded(reason.to_owned()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: i64 = 60_000;

    #[test]
    fn a_short_outage_says_nothing() {
        let mut health = Health::default();
        health.connected(0);
        health.disconnected(0);
        assert_eq!(health.tick(MINUTE), None, "a reconnect is not news");
        assert!(!health.is_degraded());
    }

    #[test]
    fn a_long_outage_lights_the_lamp_once_and_clears_only_after_a_catch_up() {
        let mut health = Health::default();
        health.connected(0);
        health.disconnected(0);
        assert_eq!(
            health.tick(4 * MINUTE),
            Some(Signal::Degraded("slack: connection lost".to_owned()))
        );
        assert_eq!(health.tick(5 * MINUTE), None, "one notice, not one a tick");

        health.connected(6 * MINUTE);
        assert!(
            health.is_degraded(),
            "a socket is not knowledge: the gap is still unread"
        );
        assert_eq!(health.feed_ok(), Some(Signal::Recovered));
        assert!(!health.is_degraded());
    }

    #[test]
    fn repeated_poll_failures_degrade_and_a_single_one_does_not() {
        let mut health = Health::default();
        health.connected(0);
        assert_eq!(health.feed_failed("500"), None);
        assert_eq!(health.feed_failed("500"), None);
        assert_eq!(
            health.feed_failed("500"),
            Some(Signal::Degraded(
                "slack: cannot reach the activity feed".to_owned()
            ))
        );
        assert_eq!(health.feed_ok(), Some(Signal::Recovered));
    }

    #[test]
    fn a_poll_that_lands_while_the_socket_is_down_does_not_clear_the_lamp() {
        let mut health = Health::default();
        health.connected(0);
        health.disconnected(0);
        health.tick(4 * MINUTE);
        assert_eq!(health.feed_ok(), None);
        assert!(health.is_degraded(), "the session is still not live");
    }
}

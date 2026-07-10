//! In-process Slack integration for rho.
//!
//! [`SlackManager`] is the whole surface the daemon needs: it owns the
//! secrets ([`SecretStore`]: a sealed memfd stashed in the systemd fd store,
//! never on disk), the Socket Mode reconnect loop, the Slack-thread → agent
//! session mapping, and posting each turn's final answer back to the thread.
//!
//! Underneath sit the protocol pieces, exposed for tests: [`SlackApi`]
//! (the Web API subset rho calls) and [`run_connection`] (one Socket Mode
//! WebSocket connection yielding normalized [`MessageEvent`]s).

mod api;
mod manager;
mod mrkdwn;
mod secrets;
mod socket_mode;

pub use api::{BotIdentity, SlackApi, ThreadMessage};
pub use manager::SlackManager;
pub use mrkdwn::to_mrkdwn;
pub use secrets::SecretStore;
pub use socket_mode::{MessageEvent, run_connection};

pub const DEFAULT_API_BASE: &str = "https://slack.com/api";

pub struct SlackConfig {
    /// Bot User OAuth Token (`xoxb-…`), for Web API calls.
    pub bot_token: String,
    /// App-Level Token (`xapp-…`), for opening Socket Mode connections.
    pub app_token: String,
    /// Web API base URL; overridable for tests.
    pub api_base: String,
}

impl SlackConfig {
    pub fn new(bot_token: String, app_token: String) -> Self {
        Self {
            bot_token,
            app_token,
            api_base: DEFAULT_API_BASE.to_owned(),
        }
    }
}

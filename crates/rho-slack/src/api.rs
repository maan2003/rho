//! Slack Web API subset used by rho.
//!
//! Slack signals failure with `{"ok": false, "error": …}` inside an HTTP 200,
//! so every call checks `ok` and surfaces the `error` field. Rate limits
//! (HTTP 429 + Retry-After) are retried a few times. Token values ride only
//! in the Authorization header and never appear in errors or logs.

use std::time::Duration;

use anyhow::{Context as _, bail};

/// Slack rejects `chat.postMessage` text over 40,000 characters with
/// `msg_too_long`; chunk with margin like other Slack clients do.
const MAX_MESSAGE_LEN: usize = 39_000;
/// Retries after HTTP 429 before giving up.
const MAX_RATE_LIMIT_RETRIES: u32 = 3;

#[derive(Clone)]
pub struct SlackApi {
    http: reqwest::Client,
    api_base: String,
}

#[derive(Debug, Clone)]
pub struct BotIdentity {
    /// The bot's own user id (`U…`); used to detect mentions and drop the
    /// bot's own messages.
    pub user_id: String,
}

/// One message out of a thread, for seeding context on mid-thread mentions.
#[derive(Debug, Clone)]
pub struct ThreadMessage {
    /// Author user id; None for bot/app posts.
    pub user: Option<String>,
    pub text: String,
    pub ts: String,
}

impl SlackApi {
    pub fn new(api_base: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_base: api_base.trim_end_matches('/').to_owned(),
        }
    }

    async fn call(
        &self,
        method: &str,
        token: &str,
        body: Option<serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value> {
        let mut request = self
            .http
            .post(format!("{}/{method}", self.api_base))
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        self.send_checked(method, request).await
    }

    /// Read-style methods (`users.info`, `conversations.replies`) take their
    /// arguments as query parameters, not a JSON body.
    async fn call_get(
        &self,
        method: &str,
        token: &str,
        params: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        let request = self
            .http
            .get(format!("{}/{method}", self.api_base))
            .bearer_auth(token)
            .query(params);
        self.send_checked(method, request).await
    }

    async fn send_checked(
        &self,
        method: &str,
        request: reqwest::RequestBuilder,
    ) -> anyhow::Result<serde_json::Value> {
        let mut attempts = 0;
        loop {
            let attempt = request
                .try_clone()
                .with_context(|| format!("slack {method}: request not retryable"))?;
            let response = attempt
                .send()
                .await
                .with_context(|| format!("slack {method}: request failed"))?;
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                && attempts < MAX_RATE_LIMIT_RETRIES
            {
                attempts += 1;
                let wait = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(1);
                tracing::warn!(method, wait, "slack rate limited; retrying");
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            let response = response
                .error_for_status()
                .with_context(|| format!("slack {method}: http error"))?;
            let value: serde_json::Value = response
                .json()
                .await
                .with_context(|| format!("slack {method}: bad response body"))?;
            if !value["ok"].as_bool().unwrap_or(false) {
                let error = value["error"].as_str().unwrap_or("unknown error");
                bail!("slack {method}: {error}");
            }
            return Ok(value);
        }
    }

    /// Open a Socket Mode connection; returns the single-use wss URL.
    pub async fn connections_open(&self, app_token: &str) -> anyhow::Result<String> {
        let value = self.call("apps.connections.open", app_token, None).await?;
        value["url"]
            .as_str()
            .map(str::to_owned)
            .context("apps.connections.open: missing url")
    }

    pub async fn auth_test(&self, bot_token: &str) -> anyhow::Result<BotIdentity> {
        let value = self.call("auth.test", bot_token, None).await?;
        Ok(BotIdentity {
            user_id: value["user_id"]
                .as_str()
                .context("auth.test: missing user_id")?
                .to_owned(),
        })
    }

    /// Post `text`, threaded under `thread_ts` when given; over-long text is
    /// sent as several messages. Returns the first message's ts.
    pub async fn post_message(
        &self,
        bot_token: &str,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
    ) -> anyhow::Result<String> {
        let mut first_ts = String::new();
        for chunk in split_chunks(text, MAX_MESSAGE_LEN) {
            let mut body = serde_json::json!({ "channel": channel, "text": chunk });
            if let Some(thread_ts) = thread_ts {
                body["thread_ts"] = thread_ts.into();
            }
            let value = self.call("chat.postMessage", bot_token, Some(body)).await?;
            if first_ts.is_empty() {
                first_ts = value["ts"].as_str().unwrap_or_default().to_owned();
            }
        }
        Ok(first_ts)
    }

    /// A user's display name (falling back to real name, then handle).
    pub async fn users_info(&self, bot_token: &str, user_id: &str) -> anyhow::Result<String> {
        let value = self
            .call_get("users.info", bot_token, &[("user", user_id)])
            .await?;
        let nonempty =
            |value: &serde_json::Value| value.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        nonempty(&value["user"]["profile"]["display_name"])
            .or_else(|| nonempty(&value["user"]["real_name"]))
            .or_else(|| nonempty(&value["user"]["name"]))
            .context("users.info: missing name")
    }

    /// The messages of a thread, oldest first (includes the root).
    pub async fn conversations_replies(
        &self,
        bot_token: &str,
        channel: &str,
        thread_ts: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<ThreadMessage>> {
        let limit = limit.to_string();
        let value = self
            .call_get(
                "conversations.replies",
                bot_token,
                &[("channel", channel), ("ts", thread_ts), ("limit", &limit)],
            )
            .await?;
        let messages = value["messages"]
            .as_array()
            .context("conversations.replies: missing messages")?;
        Ok(messages
            .iter()
            .filter_map(|message| {
                Some(ThreadMessage {
                    user: message["user"].as_str().map(str::to_owned),
                    text: message["text"].as_str().unwrap_or("").to_owned(),
                    ts: message["ts"].as_str()?.to_owned(),
                })
            })
            .collect())
    }
}

/// Split at the last newline (or char boundary) within `max` bytes.
fn split_chunks(text: &str, max: usize) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut rest = text;
    while rest.len() > max {
        let mut end = max;
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let cut = match rest[..end].rfind('\n') {
            Some(0) | None => end,
            Some(i) => i + 1,
        };
        chunks.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    chunks.push(rest);
    chunks
}

#[cfg(test)]
mod tests {
    use super::split_chunks;

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(split_chunks("hello", 10), vec!["hello"]);
    }

    #[test]
    fn splits_at_newline_and_respects_char_boundaries() {
        let text = "aaaa\nbbbb\ncccc";
        assert_eq!(split_chunks(text, 6), vec!["aaaa\n", "bbbb\n", "cccc"]);
        // No newline: must still cut on a char boundary inside multi-byte text.
        let emoji = "ééééé";
        let chunks = split_chunks(emoji, 4);
        assert_eq!(chunks.concat(), emoji);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 4));
    }
}

use std::time::Duration;

use anyhow::Context as _;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::Credentials;

const EVENT_POLL_TIMEOUT: Duration = Duration::from_secs(11 * 60);

pub struct Client {
    http: reqwest::Client,
    credentials: Credentials,
}

pub enum Anchor {
    Newest,
    Oldest,
    FirstUnread,
    Id(u64),
}

pub struct EventBatch {
    pub events: Vec<crate::types::Event>,
    pub last_event_id: i64,
    pub queue_expired: bool,
}

pub struct MessagePage {
    pub messages: Vec<crate::types::Message>,
    pub found_oldest: bool,
    pub anchor: Option<u64>,
}

impl Client {
    pub fn new(credentials: Credentials) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .build()
            .context("building Zulip HTTP client")?;
        Ok(Self { http, credentials })
    }

    pub fn site(&self) -> &str {
        &self.credentials.site
    }

    pub async fn register(&self) -> anyhow::Result<crate::types::RegisterResponse> {
        let response = self
            .request(self.http.post(self.endpoint("register")))
            .form(&[
                ("apply_markdown", "false"),
                ("client_gravatar", "true"),
                (
                    "event_types",
                    r#"["message","update_message","update_message_flags","reaction","subscription","realm_user"]"#,
                ),
            ])
            .send()
            .await
            .context("registering Zulip event queue")?;
        decode(response).await
    }

    pub async fn get_events(
        &self,
        queue_id: &str,
        last_event_id: i64,
    ) -> anyhow::Result<EventBatch> {
        let response = self
            .request(self.http.get(self.endpoint("events")))
            .timeout(EVENT_POLL_TIMEOUT)
            .query(&[
                ("queue_id", queue_id),
                ("last_event_id", &last_event_id.to_string()),
            ])
            .send()
            .await
            .context("long-polling Zulip events")?;
        let body = response
            .json::<Value>()
            .await
            .context("decoding Zulip events")?;
        if body.get("result").and_then(Value::as_str) == Some("error") {
            if body.get("code").and_then(Value::as_str) == Some("BAD_EVENT_QUEUE_ID") {
                return Ok(EventBatch {
                    events: Vec::new(),
                    last_event_id,
                    queue_expired: true,
                });
            }
            return Err(api_error(&body));
        }
        ensure_success(&body)?;
        let raw_events = body
            .get("events")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let mut newest_id = last_event_id;
        if let Some(events) = raw_events.as_array() {
            for event in events {
                if let Some(id) = event.get("id").and_then(Value::as_i64) {
                    newest_id = newest_id.max(id);
                }
            }
        }
        let events = serde_json::from_value(raw_events).context("decoding Zulip event batch")?;
        Ok(EventBatch {
            events,
            last_event_id: newest_id,
            queue_expired: false,
        })
    }

    pub async fn messages(
        &self,
        narrow: &crate::Narrow,
        anchor: Anchor,
        num_before: u32,
        num_after: u32,
    ) -> anyhow::Result<MessagePage> {
        let anchor = match anchor {
            Anchor::Newest => "newest".to_owned(),
            Anchor::Oldest => "oldest".to_owned(),
            Anchor::FirstUnread => "first_unread".to_owned(),
            Anchor::Id(id) => id.to_string(),
        };
        let response = self
            .request(self.http.get(self.endpoint("messages")))
            .query(&[
                ("anchor", anchor),
                ("num_before", num_before.to_string()),
                ("num_after", num_after.to_string()),
                ("apply_markdown", "false".to_owned()),
                ("narrow", narrow.to_json().to_string()),
            ])
            .send()
            .await
            .context("fetching Zulip messages")?;
        #[derive(Deserialize)]
        struct Response {
            messages: Vec<crate::types::Message>,
            #[serde(default)]
            found_oldest: bool,
            #[serde(default)]
            anchor: Option<u64>,
        }
        let response: Response = decode(response).await?;
        Ok(MessagePage {
            messages: response.messages,
            found_oldest: response.found_oldest,
            anchor: response.anchor,
        })
    }

    pub async fn send(
        &self,
        destination: &crate::Destination,
        content: &str,
    ) -> anyhow::Result<u64> {
        let mut form = vec![("content".to_owned(), content.to_owned())];
        match destination {
            crate::Destination::Topic { stream_id, topic } => {
                form.extend([
                    ("type".to_owned(), "stream".to_owned()),
                    ("to".to_owned(), stream_id.to_string()),
                    ("topic".to_owned(), topic.clone()),
                ]);
            }
            crate::Destination::Dm { user_ids } => {
                form.extend([
                    ("type".to_owned(), "direct".to_owned()),
                    ("to".to_owned(), json!(user_ids).to_string()),
                ]);
            }
        }
        #[derive(Deserialize)]
        struct Response {
            id: u64,
        }
        let response = self
            .request(self.http.post(self.endpoint("messages")))
            .form(&form)
            .send()
            .await
            .context("sending Zulip message")?;
        Ok(decode::<Response>(response).await?.id)
    }

    pub async fn mark_read(&self, message_ids: &[u64]) -> anyhow::Result<()> {
        let response = self
            .request(self.http.post(self.endpoint("messages/flags")))
            .form(&[
                ("messages", json!(message_ids).to_string()),
                ("op", "add".to_owned()),
                ("flag", "read".to_owned()),
            ])
            .send()
            .await
            .context("marking Zulip messages read")?;
        decode::<Empty>(response).await?;
        Ok(())
    }

    pub async fn add_reaction(&self, message_id: u64, emoji_name: &str) -> anyhow::Result<()> {
        let response = self
            .request(
                self.http
                    .post(self.endpoint(&format!("messages/{message_id}/reactions"))),
            )
            .form(&[("emoji_name", emoji_name)])
            .send()
            .await
            .context("adding Zulip reaction")?;
        decode::<Empty>(response).await?;
        Ok(())
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/api/v1/{path}", self.credentials.site)
    }

    fn request(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.basic_auth(&self.credentials.email, Some(&self.credentials.key))
    }
}

#[derive(Deserialize)]
struct Empty {}

async fn decode<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> anyhow::Result<T> {
    let body = response
        .json::<Value>()
        .await
        .context("decoding Zulip response")?;
    ensure_success(&body)?;
    serde_json::from_value(body).context("decoding Zulip response body")
}

fn ensure_success(body: &Value) -> anyhow::Result<()> {
    if body.get("result").and_then(Value::as_str) == Some("success") {
        Ok(())
    } else {
        Err(api_error(body))
    }
}

fn api_error(body: &Value) -> anyhow::Error {
    let message = body
        .get("msg")
        .and_then(Value::as_str)
        .unwrap_or("Zulip API request failed");
    anyhow::anyhow!("Zulip API error: {message}")
}

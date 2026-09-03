//! The Slack web API, called the way the desktop client calls it.
//!
//! Every request carries the `xoxc` token as a bearer and the `d` cookie that
//! authenticates it; without the cookie the token is refused. Slack answers
//! `200 OK` with `{"ok": false, "error": …}` for failures, so the status code
//! is never the verdict — `ok` is.

use std::time::Duration;

use anyhow::Context as _;
use serde_json::{Value, json};

use crate::config::Credentials;
use crate::types::{
    Attachment, ChannelId, Conversation, ConversationKind, FileSummary, Message, Ts, User, UserId,
};

const DEFAULT_BASE: &str = "https://slack.com/api";
/// One page of a conversation. Deep enough that entering a normal channel is
/// one request, shallow enough that the first screen paints quickly.
pub const PAGE: usize = 50;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Slack refuses a websocket handshake that arrives without a user agent,
/// and an `xoxc` session belongs to the desktop client, so rho presents
/// itself as one. The same string goes on every HTTP call, so what Slack
/// accepts from one it accepts from the other.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Slack/4.36.140 Chrome/120.0.6099.199 Electron/28.1.0 Safari/537.36";
/// The origin Slack's own client sends on the RTM upgrade.
const SOCKET_ORIGIN: &str = "https://api.slack.com";

/// The activity types the feed is asked for: everything that can put the user
/// under an obligation. Reactions and channel chatter are deliberately absent
/// — they would become cards for things nobody is waiting on.
const ACTIVITY_TYPES: &str = "at_user,at_user_group,at_channel,at_everyone,keyword,\
     unjoined_channel_mention,thread_v2,dm";

pub struct Client {
    http: reqwest::Client,
    credentials: Credentials,
    base: String,
}

/// What `rtm.connect` hands back: the socket to open, and who we are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtmConnection {
    pub url: String,
    pub self_id: UserId,
    pub self_name: String,
    pub team_name: String,
}

/// One entry of the activity feed, reduced to what decides an item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityItem {
    pub channel: ChannelId,
    pub ts: Ts,
    pub thread_ts: Option<Ts>,
    pub kind: ActivityKind,
    pub unread: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityKind {
    Mention,
    ThreadReply,
    DirectMessage,
    /// Reactions and everything else the feed offers that rho does not raise.
    Other,
}

impl ActivityKind {
    fn parse(raw: &str) -> Self {
        match raw {
            "at_user"
            | "at_user_group"
            | "at_channel"
            | "at_everyone"
            | "keyword"
            | "unjoined_channel_mention" => Self::Mention,
            "thread_v2" | "thread_reply" => Self::ThreadReply,
            "dm" | "bot_dm_bundle" => Self::DirectMessage,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ActivityPage {
    pub items: Vec<ActivityItem>,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct MessagePage {
    /// Oldest first, the order the surface renders.
    pub messages: Vec<Message>,
    /// The cursor for the page *before* this one, when more history exists.
    pub older_cursor: Option<String>,
}

/// Slack's own unread bookkeeping, from `client.counts`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub conversations: Vec<ConversationCount>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationCount {
    pub channel: ChannelId,
    pub has_unreads: bool,
    pub mention_count: u32,
    /// The newest message Slack knows about, which orders the list.
    pub latest: Option<Ts>,
}

impl Client {
    pub fn new(credentials: Credentials) -> anyhow::Result<Self> {
        Self::with_base(credentials, Self::default_base())
    }

    /// The real API, or wherever `RHO_SLACK_API_BASE` points. The override is
    /// what lets the transport tests and the isolated QA run drive a fake
    /// Slack without a network or a real session.
    pub fn default_base() -> String {
        std::env::var("RHO_SLACK_API_BASE")
            .ok()
            .filter(|base| !base.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE.to_owned())
    }

    pub fn with_base(credentials: Credentials, base: impl Into<String>) -> anyhow::Result<Self> {
        // reqwest refuses to build without one, and the host installs the
        // same provider at startup; whichever runs first wins.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building the Slack HTTP client")?;
        Ok(Self {
            http,
            credentials,
            base: base.into().trim_end_matches('/').to_owned(),
        })
    }

    pub fn workspace(&self) -> &crate::config::WorkspaceName {
        &self.credentials.workspace
    }

    fn endpoint(&self, method: &str) -> String {
        format!("{}/{method}", self.base)
    }

    /// The websocket upgrade, carrying the same session the API calls do.
    /// Slack has required this of `xoxc` sessions since 2023: an upgrade
    /// without the token, the cookie, a user agent, and an origin is answered
    /// with `invalid_auth`, and then rate-limited. A socket that never asks
    /// for them is simply silent, which is worse than an error.
    pub fn socket_request(
        &self,
        url: &str,
    ) -> anyhow::Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

        let mut request = url
            .into_client_request()
            .context("building the Slack websocket handshake")?;
        let headers = request.headers_mut();
        headers.insert(
            "Authorization",
            format!("Bearer {}", self.credentials.token).parse()?,
        );
        headers.insert("Cookie", format!("d={};", self.credentials.cookie).parse()?);
        headers.insert("User-Agent", USER_AGENT.parse()?);
        headers.insert("Origin", SOCKET_ORIGIN.parse()?);
        Ok(request)
    }

    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
            .header(
                "Authorization",
                format!("Bearer {}", self.credentials.token),
            )
            .header("Cookie", format!("d={}", self.credentials.cookie))
    }

    async fn post_form(&self, method: &str, fields: &[(&str, String)]) -> anyhow::Result<Value> {
        let response = self
            .authorize(self.http.post(self.endpoint(method)))
            .form(fields)
            .send()
            .await
            .with_context(|| format!("calling {method}"))?;
        decode(method, response).await
    }

    async fn post_json(&self, method: &str, body: Value) -> anyhow::Result<Value> {
        let response = self
            .authorize(self.http.post(self.endpoint(method)))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("calling {method}"))?;
        decode(method, response).await
    }

    pub async fn rtm_connect(&self) -> anyhow::Result<RtmConnection> {
        let body = self.post_form("rtm.connect", &[]).await?;
        let url = string(&body["url"])
            .filter(|url| !url.is_empty())
            .context("rtm.connect returned no websocket url")?;
        Ok(RtmConnection {
            url,
            self_id: UserId(string(&body["self"]["id"]).unwrap_or_default()),
            self_name: string(&body["self"]["name"]).unwrap_or_default(),
            team_name: string(&body["team"]["name"]).unwrap_or_default(),
        })
    }

    /// One page of `activity.feed`, the endpoint behind the web client's
    /// Activity view. Slack accepts it only as multipart, which is built by
    /// hand here rather than pulling in a multipart encoder for one call.
    pub async fn activity_feed(&self, cursor: Option<&str>) -> anyhow::Result<ActivityPage> {
        const BOUNDARY: &str = "----rhoSlackActivityFeed";
        let mut fields = vec![
            ("token", self.credentials.token.clone()),
            ("limit", "50".to_owned()),
            ("types", ACTIVITY_TYPES.to_owned()),
            ("mode", "chrono_v1".to_owned()),
            ("archive_only", "false".to_owned()),
            ("unread_only", "false".to_owned()),
            ("priority_only", "false".to_owned()),
            ("is_activity_inbox", "true".to_owned()),
        ];
        if let Some(cursor) = cursor {
            fields.push(("cursor", cursor.to_owned()));
        }
        let mut body = String::new();
        for (name, value) in &fields {
            body.push_str(&format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            ));
        }
        body.push_str(&format!("--{BOUNDARY}--\r\n"));
        let response = self
            .authorize(self.http.post(self.endpoint("activity.feed")))
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(body)
            .send()
            .await
            .context("calling activity.feed")?;
        let body = decode("activity.feed", response).await?;
        let items = body["items"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(parse_activity_item)
            .collect();
        Ok(ActivityPage {
            items,
            cursor: string(&body["response_metadata"]["next_cursor"])
                .filter(|cursor| !cursor.is_empty()),
        })
    }

    /// A thread, oldest first. The parent is the first message, exactly as
    /// the surface wants to render it.
    pub async fn conversations_replies(
        &self,
        channel: &ChannelId,
        thread_ts: &Ts,
        cursor: Option<&str>,
    ) -> anyhow::Result<MessagePage> {
        let mut fields = vec![
            ("channel", channel.0.clone()),
            ("ts", thread_ts.0.clone()),
            ("limit", PAGE.to_string()),
        ];
        if let Some(cursor) = cursor {
            fields.push(("cursor", cursor.to_owned()));
        }
        let body = self.post_form("conversations.replies", &fields).await?;
        Ok(parse_message_page(&body, channel))
    }

    /// A channel or DM's recent messages. Slack returns newest first here,
    /// unlike replies, so the page is reversed before it is handed on.
    pub async fn conversations_history(
        &self,
        channel: &ChannelId,
        cursor: Option<&str>,
    ) -> anyhow::Result<MessagePage> {
        let mut fields = vec![("channel", channel.0.clone()), ("limit", PAGE.to_string())];
        if let Some(cursor) = cursor {
            fields.push(("cursor", cursor.to_owned()));
        }
        let body = self.post_form("conversations.history", &fields).await?;
        let mut page = parse_message_page(&body, channel);
        page.messages
            .sort_by(|left, right| left.ts.epoch_seconds().total_cmp(&right.ts.epoch_seconds()));
        Ok(page)
    }

    /// The tail of a conversation: only what is newer than `oldest`. This is
    /// what a mirrored conversation asks for when it is reopened, so nothing
    /// already on disk is fetched twice.
    pub async fn conversations_history_since(
        &self,
        channel: &ChannelId,
        oldest: &Ts,
    ) -> anyhow::Result<MessagePage> {
        let fields = vec![
            ("channel", channel.0.clone()),
            ("limit", PAGE.to_string()),
            ("oldest", oldest.0.clone()),
            ("inclusive", "false".to_owned()),
        ];
        let body = self.post_form("conversations.history", &fields).await?;
        let mut page = parse_message_page(&body, channel);
        page.messages
            .sort_by(|left, right| left.ts.epoch_seconds().total_cmp(&right.ts.epoch_seconds()));
        Ok(page)
    }

    /// The window of a conversation around `ts`: what a person sees when they
    /// click a notification and land on the message, with context on both
    /// sides of it. Two bounded calls, never paged — it is context for a
    /// ping, not a history walk.
    pub async fn conversations_history_around(
        &self,
        channel: &ChannelId,
        ts: &Ts,
        span: usize,
    ) -> anyhow::Result<Vec<Message>> {
        let before = self
            .history_window(
                channel,
                span,
                &[("latest", ts.0.clone()), ("inclusive", "true".to_owned())],
            )
            .await?;
        let after = self
            .history_window(
                channel,
                span,
                &[("oldest", ts.0.clone()), ("inclusive", "false".to_owned())],
            )
            .await?;
        let mut messages = before;
        messages.extend(after);
        messages
            .sort_by(|left, right| left.ts.epoch_seconds().total_cmp(&right.ts.epoch_seconds()));
        messages.dedup_by(|left, right| left.ts == right.ts);
        Ok(messages)
    }

    async fn history_window(
        &self,
        channel: &ChannelId,
        limit: usize,
        bounds: &[(&str, String)],
    ) -> anyhow::Result<Vec<Message>> {
        let mut fields = vec![("channel", channel.0.clone()), ("limit", limit.to_string())];
        fields.extend(bounds.iter().cloned());
        let body = self.post_form("conversations.history", &fields).await?;
        Ok(parse_message_page(&body, channel).messages)
    }

    /// Every conversation the user is in, across all four kinds.
    pub async fn conversations(&self) -> anyhow::Result<Vec<Conversation>> {
        let mut conversations = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut fields = vec![
                ("types", "public_channel,private_channel,mpim,im".to_owned()),
                ("exclude_archived", "true".to_owned()),
                ("limit", "200".to_owned()),
            ];
            if let Some(cursor) = &cursor {
                fields.push(("cursor", cursor.clone()));
            }
            let body = self.post_form("users.conversations", &fields).await?;
            conversations.extend(
                body["channels"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(parse_conversation),
            );
            cursor = string(&body["response_metadata"]["next_cursor"])
                .filter(|cursor| !cursor.is_empty());
            if cursor.is_none() {
                return Ok(conversations);
            }
        }
    }

    pub async fn conversation_info(&self, channel: &ChannelId) -> anyhow::Result<Conversation> {
        let body = self
            .post_form("conversations.info", &[("channel", channel.0.clone())])
            .await?;
        parse_conversation(&body["channel"]).context("conversations.info returned no channel")
    }

    /// The workspace's custom emoji, by name. They have no glyph anywhere
    /// but Slack, so knowing which shortcodes are real is what keeps a
    /// custom one from reading as a stray word.
    pub async fn custom_emoji(&self) -> anyhow::Result<Vec<String>> {
        let body = self.post_form("emoji.list", &[]).await?;
        Ok(body["emoji"]
            .as_object()
            .map(|emoji| emoji.keys().cloned().collect())
            .unwrap_or_default())
    }

    /// The bytes behind a file. Slack serves them from the same session as
    /// the API, so an unauthenticated fetch gets a login page rather than a
    /// file.
    pub async fn download(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let response = self
            .authorize(self.http.get(url))
            .send()
            .await
            .with_context(|| format!("fetching {url}"))?
            .error_for_status()
            .with_context(|| format!("fetching {url}"))?;
        Ok(response.bytes().await?.to_vec())
    }

    pub async fn counts(&self) -> anyhow::Result<Counts> {
        let body = self
            .post_form(
                "client.counts",
                &[("thread_counts_by_channel", "true".to_owned())],
            )
            .await?;
        let mut conversations = Vec::new();
        for group in ["channels", "mpims", "ims"] {
            conversations.extend(
                body[group]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|count| {
                        Some(ConversationCount {
                            channel: ChannelId(string(&count["id"])?),
                            has_unreads: count["has_unreads"].as_bool().unwrap_or(false),
                            mention_count: count["mention_count"].as_u64().unwrap_or(0) as u32,
                            latest: string(&count["latest"])
                                .filter(|latest| !latest.is_empty())
                                .map(Ts),
                        })
                    }),
            );
        }
        Ok(Counts { conversations })
    }

    /// Marks a conversation read up to `ts`, so the phone and the web client
    /// agree with what rho has shown.
    pub async fn mark_read(&self, channel: &ChannelId, ts: &Ts) -> anyhow::Result<()> {
        self.post_form(
            "conversations.mark",
            &[("channel", channel.0.clone()), ("ts", ts.0.clone())],
        )
        .await?;
        Ok(())
    }

    /// Sends `text`. With `thread_ts` it is a reply inside that thread;
    /// without, a new message in the channel or DM.
    pub async fn post_message(
        &self,
        channel: &ChannelId,
        thread_ts: Option<&Ts>,
        text: &str,
    ) -> anyhow::Result<Ts> {
        let mut body = json!({"channel": channel.0, "text": text});
        if let Some(thread_ts) = thread_ts {
            body["thread_ts"] = json!(thread_ts.0);
        }
        let response = self.post_json("chat.postMessage", body).await?;
        Ok(Ts(string(&response["ts"]).unwrap_or_default()))
    }

    pub async fn user_info(&self, user: &UserId) -> anyhow::Result<User> {
        let body = self
            .post_form("users.info", &[("user", user.0.clone())])
            .await?;
        Ok(parse_user(&body["user"]).unwrap_or_else(|| User {
            id: user.clone(),
            name: "someone".to_owned(),
            handle: String::new(),
        }))
    }

    /// The whole roster in one call, which is how mentions get names without
    /// a request per author.
    pub async fn users(&self) -> anyhow::Result<Vec<User>> {
        let mut users = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut fields = vec![("limit", "500".to_owned())];
            if let Some(cursor) = &cursor {
                fields.push(("cursor", cursor.clone()));
            }
            let body = self.post_form("users.list", &fields).await?;
            users.extend(
                body["members"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(parse_user),
            );
            cursor = string(&body["response_metadata"]["next_cursor"])
                .filter(|cursor| !cursor.is_empty());
            if cursor.is_none() {
                return Ok(users);
            }
        }
    }
}

async fn decode(method: &str, response: reqwest::Response) -> anyhow::Result<Value> {
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .with_context(|| format!("decoding {method} ({status})"))?;
    if body["ok"].as_bool() == Some(true) {
        return Ok(body);
    }
    let error = string(&body["error"]).unwrap_or_else(|| "unknown error".to_owned());
    anyhow::bail!("{method} failed: {error}")
}

fn string(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

fn parse_message_page(body: &Value, channel: &ChannelId) -> MessagePage {
    let messages = body["messages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|message| parse_message(message, channel))
        .collect();
    let older_cursor = body["has_more"]
        .as_bool()
        .unwrap_or(false)
        .then(|| string(&body["response_metadata"]["next_cursor"]))
        .flatten()
        .filter(|cursor| !cursor.is_empty());
    MessagePage {
        messages,
        older_cursor,
    }
}

/// Parses one message payload. The same shape arrives over the websocket, so
/// this is the single place that decides what a message is.
pub fn parse_message(value: &Value, fallback_channel: &ChannelId) -> Option<Message> {
    let ts = Ts(string(&value["ts"])?);
    Some(Message {
        ts,
        thread_ts: string(&value["thread_ts"]).map(Ts),
        channel: string(&value["channel"])
            .map(ChannelId)
            .unwrap_or_else(|| fallback_channel.clone()),
        user: string(&value["user"])
            .filter(|id| !id.is_empty())
            .map(UserId),
        bot_name: string(&value["username"])
            .or_else(|| string(&value["bot_profile"]["name"]))
            .filter(|name| !name.is_empty()),
        blocks: value["blocks"].as_array().cloned().unwrap_or_default(),
        text: string(&value["text"]).unwrap_or_default(),
        attachments: value["attachments"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|attachment| Attachment {
                title: string(&attachment["title"]).filter(|title| !title.is_empty()),
                text: string(&attachment["text"]).filter(|text| !text.is_empty()),
                fallback: string(&attachment["fallback"]).filter(|text| !text.is_empty()),
                pretext: string(&attachment["pretext"]).filter(|text| !text.is_empty()),
                fields: attachment["fields"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|field| Some((string(&field["title"])?, string(&field["value"])?)))
                    .collect(),
                is_unfurl: attachment["is_msg_unfurl"].as_bool().unwrap_or(false)
                    || attachment["is_app_unfurl"].as_bool().unwrap_or(false),
            })
            .collect(),
        subtype: string(&value["subtype"]).filter(|subtype| !subtype.is_empty()),
        reply_count: value["reply_count"].as_u64().unwrap_or(0) as u32,
        latest_reply: string(&value["latest_reply"]).map(Ts),
        edited: value["edited"].is_object(),
        reactions: value["reactions"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|reaction| {
                Some(crate::types::Reaction {
                    name: string(&reaction["name"])?,
                    count: reaction["count"].as_u64().unwrap_or(0) as u32,
                    users: reaction["users"]
                        .as_array()
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|user| string(user).map(UserId))
                        .collect(),
                })
            })
            .collect(),
        files: value["files"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|file| {
                Some(FileSummary {
                    id: string(&file["id"]).unwrap_or_default(),
                    title: string(&file["name"])
                        .or_else(|| string(&file["title"]))
                        .filter(|title| !title.is_empty())?,
                    filetype: string(&file["filetype"]).unwrap_or_default(),
                    size: file["size"].as_u64().unwrap_or(0),
                    url: string(&file["url_private"]).unwrap_or_default(),
                })
            })
            .collect(),
    })
}

fn parse_conversation(value: &Value) -> Option<Conversation> {
    let id = ChannelId(string(&value["id"])?);
    let is_im = value["is_im"].as_bool().unwrap_or(false);
    let is_mpim = value["is_mpim"].as_bool().unwrap_or(false);
    let kind = match (is_im, is_mpim) {
        (true, _) => ConversationKind::DirectMessage,
        (_, true) => ConversationKind::Group,
        _ => ConversationKind::Channel,
    };
    let user = string(&value["user"])
        .filter(|id| !id.is_empty())
        .map(UserId);
    Some(Conversation {
        id,
        kind,
        // A DM has no name of its own; the roster supplies it later, and
        // until then it reads as an unnamed person rather than as an id.
        name: string(&value["name"])
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "someone".to_owned()),
        user,
        members: value["members"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|member| string(member).map(UserId))
            .collect(),
    })
}

fn parse_user(value: &Value) -> Option<User> {
    let id = UserId(string(&value["id"])?);
    let handle = string(&value["name"]).unwrap_or_default();
    let name = string(&value["profile"]["display_name"])
        .filter(|name| !name.is_empty())
        .or_else(|| string(&value["profile"]["real_name"]).filter(|name| !name.is_empty()))
        .or_else(|| string(&value["name"]))
        .unwrap_or_else(|| "someone".to_owned());
    Some(User { id, name, handle })
}

/// The activity feed nests its payload differently per type; this pulls the
/// channel and the two timestamps out of whichever shape arrived.
fn parse_activity_item(value: &Value) -> Option<ActivityItem> {
    let item = &value["item"];
    let kind = ActivityKind::parse(item["type"].as_str().unwrap_or_default());
    let message = &item["message"];
    let bundle = &item["bundle_info"]["payload"];
    let thread_entry = &bundle["thread_entry"];
    let dm_entry = &bundle["dm_entry"]["latest_message"];
    let bundle_message = &bundle["message"];

    let ts = string(&message["ts"])
        .or_else(|| string(&bundle_message["ts"]))
        .or_else(|| string(&thread_entry["latest_ts"]))
        .or_else(|| string(&dm_entry["ts"]))?;
    let channel = string(&message["channel"])
        .or_else(|| string(&bundle_message["channel"]))
        .or_else(|| string(&thread_entry["channel_id"]))
        .or_else(|| string(&dm_entry["channel"]))?;
    let thread_ts = string(&message["thread_ts"]).or_else(|| string(&thread_entry["thread_ts"]));
    Some(ActivityItem {
        channel: ChannelId(channel),
        ts: Ts(ts),
        thread_ts: thread_ts.map(Ts),
        kind,
        unread: value["is_unread"].as_bool().unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn activity_items_parse_from_all_three_payload_shapes() {
        let mention = parse_activity_item(&json!({
            "is_unread": true,
            "item": {
                "type": "at_user",
                "message": {"ts": "100.1", "channel": "C1", "author_user_id": "U2"},
            },
        }))
        .unwrap();
        assert_eq!(
            mention,
            ActivityItem {
                channel: ChannelId("C1".into()),
                ts: Ts("100.1".into()),
                thread_ts: None,
                kind: ActivityKind::Mention,
                unread: true,
            }
        );

        let thread = parse_activity_item(&json!({
            "is_unread": false,
            "item": {
                "type": "thread_v2",
                "bundle_info": {"payload": {"thread_entry": {
                    "channel_id": "C2", "thread_ts": "90.0", "latest_ts": "99.5",
                }}},
            },
        }))
        .unwrap();
        assert_eq!(thread.channel, ChannelId("C2".into()));
        assert_eq!(thread.ts, Ts("99.5".into()));
        assert_eq!(thread.thread_ts, Some(Ts("90.0".into())));
        assert_eq!(thread.kind, ActivityKind::ThreadReply);

        let dm = parse_activity_item(&json!({
            "item": {
                "type": "dm",
                "bundle_info": {"payload": {"dm_entry": {"latest_message": {
                    "ts": "12.0", "channel": "D9",
                }}}},
            },
        }))
        .unwrap();
        assert_eq!(dm.kind, ActivityKind::DirectMessage);
        assert_eq!(dm.channel, ChannelId("D9".into()));

        // A reaction is in the feed but is nobody's obligation.
        let reaction = parse_activity_item(&json!({
            "item": {"type": "message_reaction", "message": {"ts": "1.0", "channel": "C1"}},
        }))
        .unwrap();
        assert_eq!(reaction.kind, ActivityKind::Other);
    }

    #[test]
    fn a_message_keeps_its_blocks_files_and_thread_parent() {
        let message = parse_message(
            &json!({
                "ts": "101.2",
                "thread_ts": "100.0",
                "user": "U1",
                "text": "hello",
                "blocks": [{"type": "rich_text"}],
                "files": [{"name": "log.txt"}],
                "attachments": [{"title": "Build", "fallback": "b"}],
            }),
            &ChannelId("C1".into()),
        )
        .unwrap();
        assert_eq!(message.channel, ChannelId("C1".into()));
        assert_eq!(message.thread_root(), Ts("100.0".into()));
        assert_eq!(message.user, Some(UserId("U1".into())));
        assert_eq!(message.blocks.len(), 1);
        assert_eq!(message.files[0].title, "log.txt");
        assert_eq!(message.attachments[0].title.as_deref(), Some("Build"));

        // A message that is not in a thread is its own thread root.
        let root = parse_message(&json!({"ts": "5.0"}), &ChannelId("C1".into())).unwrap();
        assert_eq!(root.thread_root(), Ts("5.0".into()));
    }

    #[test]
    fn conversation_kinds_and_members_come_from_the_flags() {
        let channel = parse_conversation(&json!({"id": "C1", "name": "design"})).unwrap();
        assert_eq!(channel.kind, ConversationKind::Channel);
        assert_eq!(channel.name, "design");

        let group = parse_conversation(
            &json!({"id": "G1", "name": "trio", "is_mpim": true, "members": ["ME", "U7"]}),
        )
        .unwrap();
        assert_eq!(group.kind, ConversationKind::Group);
        assert_eq!(
            group.members,
            vec![UserId("ME".into()), UserId("U7".into())]
        );

        let dm = parse_conversation(&json!({"id": "D1", "is_im": true, "user": "U7"})).unwrap();
        assert_eq!(dm.kind, ConversationKind::DirectMessage);
        assert_eq!(dm.user, Some(UserId("U7".into())));
        assert!(dm.members.is_empty());
    }
}

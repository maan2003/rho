//! The web API: the methods rho calls, the errors Slack answers with, and
//! the one typed action list that both the in-process handle and the binary's
//! control endpoint take.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::store::Store;
use crate::types::{ChannelId, Ts, UserId};
use crate::wire;

/// Every method this server answers. A typed name rather than a string, so a
/// caller cannot ask for a method that does not exist and a refusal cannot be
/// aimed at one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Method {
    UsersConversations,
    UsersInfo,
    UsersList,
    UsersPrefsGet,
    ConversationsHistory,
    ConversationsReplies,
    ConversationsInfo,
    ClientCounts,
    EmojiList,
    SubscriptionsThreadGetView,
    ActivityFeed,
}

impl Method {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "users.conversations" => Self::UsersConversations,
            "users.info" => Self::UsersInfo,
            "users.list" => Self::UsersList,
            "users.prefs.get" => Self::UsersPrefsGet,
            "conversations.history" => Self::ConversationsHistory,
            "conversations.replies" => Self::ConversationsReplies,
            "conversations.info" => Self::ConversationsInfo,
            "client.counts" => Self::ClientCounts,
            "emoji.list" => Self::EmojiList,
            "subscriptions.thread.getView" => Self::SubscriptionsThreadGetView,
            "activity.feed" => Self::ActivityFeed,
            _ => return None,
        })
    }
}

/// Why a call was refused, in Slack's own vocabulary. The client is written
/// against these strings, so they are the thing to be literal about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    InvalidAuth,
    ChannelNotFound,
    UserNotFound,
    MessageNotFound,
    NotInChannel,
    UnknownMethod,
    RateLimited { retry_after_seconds: u32 },
}

impl Refusal {
    fn code(self) -> &'static str {
        match self {
            Self::InvalidAuth => "invalid_auth",
            Self::ChannelNotFound => "channel_not_found",
            Self::UserNotFound => "user_not_found",
            Self::MessageNotFound => "message_not_found",
            Self::NotInChannel => "not_in_channel",
            Self::UnknownMethod => "unknown_method",
            Self::RateLimited { .. } => "ratelimited",
        }
    }

    fn response(self) -> Response {
        let body = axum::Json(json!({"ok": false, "error": self.code()}));
        match self {
            // Slack rate-limits with a status and a header, not with a 200,
            // and a client that only reads the body never backs off.
            Self::RateLimited {
                retry_after_seconds,
            } => (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                [("retry-after", retry_after_seconds.to_string())],
                body,
            )
                .into_response(),
            _ => body.into_response(),
        }
    }
}

/// Something done to the server from outside a request. One type for both
/// transports: the in-process handle takes it directly and the binary's
/// control endpoint decodes it, so there is no second vocabulary to keep in
/// step.
#[derive(Clone, Copy, Debug)]
pub enum Action {
    /// Refuse the next `times` calls of a method.
    Refuse {
        method: Method,
        refusal: Refusal,
        times: usize,
    },
}

/// What the server is doing to callers on purpose, and what it has served.
#[derive(Default)]
pub struct Control {
    refusals: HashMap<Method, (Refusal, usize)>,
    /// How many requests have been answered, per method, which is where the
    /// "what a full sync costs" number comes from.
    served: HashMap<Method, u64>,
}

impl Control {
    pub fn take(&mut self, action: Action) {
        match action {
            Action::Refuse {
                method,
                refusal,
                times,
            } => {
                self.refusals.insert(method, (refusal, times));
            }
        }
    }

    pub fn served(&self, method: Method) -> u64 {
        self.served.get(&method).copied().unwrap_or_default()
    }

    pub fn served_total(&self) -> u64 {
        self.served.values().sum()
    }

    fn refusal(&mut self, method: Method) -> Option<Refusal> {
        let (refusal, left) = self.refusals.get_mut(&method)?;
        let refusal = *refusal;
        *left -= 1;
        if *left == 0 {
            self.refusals.remove(&method);
        }
        Some(refusal)
    }
}

#[derive(Clone)]
pub struct Server {
    pub store: Arc<Store>,
    pub control: Arc<Mutex<Control>>,
}

/// How many rows a paginated call returns when the caller does not say.
const PAGE: usize = 100;

pub async fn call(
    State(server): State<Server>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    // The body is read rather than extracted, because Slack accepts a call
    // with no body at all and a `Form` extractor answers that with a 415 the
    // client has never seen.
    let form = parse_form(&body);
    let Some(method) = Method::parse(name.trim_end_matches(".json")) else {
        return Refusal::UnknownMethod.response();
    };
    let authorised = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("Bearer ") && value.len() > "Bearer ".len())
        || form.get("token").is_some_and(|token| !token.is_empty());
    if !authorised {
        return Refusal::InvalidAuth.response();
    }
    {
        let mut control = server.control.lock().expect("control");
        if let Some(refusal) = control.refusal(method) {
            return refusal.response();
        }
        *control.served.entry(method).or_default() += 1;
    }
    match answer(&server.store, method, &form) {
        Ok(body) => axum::Json(body).into_response(),
        Err(refusal) => refusal.response(),
    }
}

fn answer(store: &Store, method: Method, form: &HashMap<String, String>) -> Result<Value, Refusal> {
    match method {
        Method::UsersConversations => {
            let (from, limit) = window(form);
            let all: Vec<Value> = store
                .conversations()
                .skip(from)
                .take(limit)
                .map(|conversation| wire::conversation(store, conversation))
                .collect();
            let next = cursor(from + all.len(), store.conversation_count());
            Ok(wire::page(json!({ "channels": all }), next))
        }
        Method::UsersList => {
            let (from, limit) = window(form);
            let members: Vec<Value> = store
                .users()
                .skip(from)
                .take(limit)
                .map(wire::user)
                .collect();
            let next = cursor(from + members.len(), store.user_count());
            Ok(wire::page(json!({ "members": members }), next))
        }
        Method::UsersInfo => {
            let id = UserId(field(form, "user")?);
            let user = store.user(&id).ok_or(Refusal::UserNotFound)?;
            Ok(json!({"ok": true, "user": wire::user(user)}))
        }
        Method::UsersPrefsGet => Ok(json!({
            "ok": true,
            "prefs": {
                "muted_channels": store
                    .conversations()
                    .filter(|conversation| conversation.muted)
                    .map(|conversation| conversation.id.0.clone())
                    .collect::<Vec<_>>()
                    .join(","),
            },
        })),
        Method::ConversationsInfo => {
            let id = ChannelId(field(form, "channel")?);
            let conversation = store.conversation(&id).ok_or(Refusal::ChannelNotFound)?;
            Ok(json!({"ok": true, "channel": wire::conversation(store, conversation)}))
        }
        Method::ConversationsHistory => {
            let id = ChannelId(field(form, "channel")?);
            let limit = form
                .get("limit")
                .and_then(|limit| limit.parse().ok())
                .unwrap_or(PAGE);
            // Slack's history cursor is opaque to the client; here it is the
            // oldest timestamp already seen, so asking again with it hands
            // back the page before that one. `latest` does the same job for a
            // client that walks backwards by time instead.
            let cursor = form.get("cursor").and_then(|cursor| Ts::parse(cursor));
            let latest = cursor.or_else(|| form.get("latest").and_then(|ts| Ts::parse(ts)));
            let window = store
                .history(
                    &id,
                    form.get("oldest").and_then(|ts| Ts::parse(ts)),
                    latest,
                    cursor.is_none()
                        && form
                            .get("inclusive")
                            .is_some_and(|value| value == "1" || value == "true"),
                    limit,
                )
                .ok_or(Refusal::ChannelNotFound)?;
            // Newest first, which is the order the client backfills in.
            let messages: Vec<Value> = window
                .messages
                .iter()
                .rev()
                .map(|message| wire::message(&id.0, message))
                .collect();
            let next = window
                .has_more
                .then(|| {
                    window
                        .messages
                        .first()
                        .map(|message| message.ts.to_string())
                })
                .flatten();
            Ok(wire::page(
                json!({"messages": messages, "has_more": window.has_more}),
                next,
            ))
        }
        Method::ConversationsReplies => {
            let id = ChannelId(field(form, "channel")?);
            let ts = Ts::parse(&field(form, "ts")?).ok_or(Refusal::MessageNotFound)?;
            let (parent, replies) = store.replies(&id, ts).ok_or(Refusal::ChannelNotFound)?;
            let mut messages = Vec::with_capacity(replies.len() + 1);
            messages.push(wire::message(&id.0, parent));
            messages.extend(replies.iter().map(|reply| wire::message(&id.0, reply)));
            Ok(json!({"ok": true, "messages": messages, "has_more": false}))
        }
        Method::ClientCounts => {
            let mut channels = Vec::new();
            let mut ims = Vec::new();
            let mut mpims = Vec::new();
            for conversation in store.conversations() {
                let unread = store.unread(&conversation.id, &store.self_id);
                let row = wire::count(
                    &conversation.id.0,
                    unread,
                    store.latest(&conversation.id).map(|ts| ts.to_string()),
                    store
                        .read_cursor(&conversation.id, &store.self_id)
                        .map(|ts| ts.to_string()),
                    store.counts_messages(&conversation.id),
                );
                match conversation.kind {
                    crate::types::Kind::Dm => ims.push(row),
                    crate::types::Kind::Group => mpims.push(row),
                    _ => channels.push(row),
                }
            }
            Ok(json!({"ok": true, "channels": channels, "ims": ims, "mpims": mpims}))
        }
        Method::SubscriptionsThreadGetView => {
            let threads: Vec<Value> = store
                .followed()
                .iter()
                .map(|(channel, parent)| {
                    wire::followed(
                        &channel.0,
                        &parent.to_string(),
                        store
                            .thread_read_cursor(channel, *parent, &store.self_id)
                            .map(|ts| ts.to_string()),
                    )
                })
                .collect();
            Ok(json!({"ok": true, "threads": threads}))
        }
        Method::ActivityFeed => {
            // The feed is what someone else did that names the reader:
            // newest first, across the workspace. The mention prefix in the
            // store is what makes finding them a lookup rather than a walk,
            // and the items themselves are the messages, not a summary of
            // them.
            let items: Vec<Value> = store
                .mentions(50)
                .into_iter()
                .map(|(channel, message)| {
                    json!({
                        "type": "at_channel",
                        "ts": message.ts.to_string(),
                        "channel": {"id": channel.0},
                        "message": wire::message(&channel.0, message),
                    })
                })
                .collect();
            Ok(wire::page(json!({ "items": items }), None))
        }
        Method::EmojiList => {
            let emoji: serde_json::Map<String, Value> = store
                .emoji()
                .map(|(name, url)| (name.clone(), json!(url)))
                .collect();
            Ok(json!({"ok": true, "emoji": emoji}))
        }
    }
}

/// `a=1&b=two%20words`, the way a client sends it.
fn parse_form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            Some((decode(name), decode(value)))
        })
        .collect()
}

/// Percent-decoding that puts the bytes back together before it reads them
/// as text, so a message with anything but ASCII in it survives the round
/// trip.
fn decode(text: &str) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(text.len());
    let mut bytes = text.bytes();
    while let Some(byte) = bytes.next() {
        match byte {
            b'+' => out.push(b' '),
            b'%' => {
                let digits: String = bytes.by_ref().take(2).map(char::from).collect();
                match u8::from_str_radix(&digits, 16) {
                    Ok(byte) => out.push(byte),
                    Err(_) => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn field(form: &HashMap<String, String>, name: &str) -> Result<String, Refusal> {
    form.get(name).cloned().ok_or(match name {
        "channel" => Refusal::ChannelNotFound,
        "user" => Refusal::UserNotFound,
        _ => Refusal::MessageNotFound,
    })
}

/// Slack's cursor is opaque to the client; here it is the row to start at,
/// which is all a cursor into a stable list has to be.
fn window(form: &HashMap<String, String>) -> (usize, usize) {
    let from = form
        .get("cursor")
        .and_then(|cursor| cursor.parse().ok())
        .unwrap_or(0);
    let limit = form
        .get("limit")
        .and_then(|limit| limit.parse().ok())
        .unwrap_or(PAGE);
    (from, limit)
}

fn cursor(reached: usize, total: usize) -> Option<String> {
    match reached < total {
        true => Some(reached.to_string()),
        false => None,
    }
}

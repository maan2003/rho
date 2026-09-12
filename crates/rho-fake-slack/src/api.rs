//! The web API: the methods rho calls, the errors Slack answers with, and
//! the one typed action list that both the in-process handle and the binary's
//! control endpoint take.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::socket::Wire;
use crate::store::Store;
use crate::types::{ChannelId, Message, Ts, UserId};
use crate::{socket, wire};

/// Every method this server answers. A typed name rather than a string, so a
/// caller cannot ask for a method that does not exist and a refusal cannot be
/// aimed at one.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum Method {
    #[serde(rename = "users.conversations")]
    UsersConversations,
    #[serde(rename = "users.info")]
    UsersInfo,
    #[serde(rename = "users.list")]
    UsersList,
    #[serde(rename = "users.prefs.get")]
    UsersPrefsGet,
    #[serde(rename = "conversations.history")]
    ConversationsHistory,
    #[serde(rename = "conversations.replies")]
    ConversationsReplies,
    #[serde(rename = "conversations.info")]
    ConversationsInfo,
    #[serde(rename = "client.counts")]
    ClientCounts,
    #[serde(rename = "emoji.list")]
    EmojiList,
    #[serde(rename = "subscriptions.thread.getView")]
    SubscriptionsThreadGetView,
    #[serde(rename = "activity.feed")]
    ActivityFeed,
    #[serde(rename = "rtm.connect")]
    RtmConnect,
    #[serde(rename = "chat.postMessage")]
    ChatPostMessage,
    #[serde(rename = "chat.update")]
    ChatUpdate,
    #[serde(rename = "reactions.add")]
    ReactionsAdd,
    #[serde(rename = "reactions.remove")]
    ReactionsRemove,
    #[serde(rename = "conversations.mark")]
    ConversationsMark,
    #[serde(rename = "subscriptions.thread.add")]
    SubscriptionsThreadAdd,
    #[serde(rename = "subscriptions.thread.remove")]
    SubscriptionsThreadRemove,
    #[serde(rename = "subscriptions.thread.mark")]
    SubscriptionsThreadMark,
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
            "rtm.connect" => Self::RtmConnect,
            "chat.postMessage" => Self::ChatPostMessage,
            "chat.update" => Self::ChatUpdate,
            "reactions.add" => Self::ReactionsAdd,
            "reactions.remove" => Self::ReactionsRemove,
            "conversations.mark" => Self::ConversationsMark,
            "subscriptions.thread.add" => Self::SubscriptionsThreadAdd,
            "subscriptions.thread.remove" => Self::SubscriptionsThreadRemove,
            "subscriptions.thread.mark" => Self::SubscriptionsThreadMark,
            _ => return None,
        })
    }
}

impl Method {
    /// Whether the method changes the workspace. Writes take the store
    /// exclusively and end in a frame; reads do neither.
    pub fn writes(self) -> bool {
        matches!(
            self,
            Self::ChatPostMessage
                | Self::ChatUpdate
                | Self::ReactionsAdd
                | Self::ReactionsRemove
                | Self::ConversationsMark
                | Self::SubscriptionsThreadAdd
                | Self::SubscriptionsThreadRemove
                | Self::SubscriptionsThreadMark
        )
    }
}

/// Why a call was refused, in Slack's own vocabulary. The client is written
/// against these strings, so they are the thing to be literal about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    InvalidAuth,
    ChannelNotFound,
    UserNotFound,
    MessageNotFound,
    NotInChannel,
    AlreadyReacted,
    NoReaction,
    UnknownMethod,
    #[serde(rename = "ratelimited")]
    RateLimited {
        retry_after_seconds: u32,
    },
}

impl Refusal {
    fn code(self) -> &'static str {
        match self {
            Self::InvalidAuth => "invalid_auth",
            Self::ChannelNotFound => "channel_not_found",
            Self::UserNotFound => "user_not_found",
            Self::MessageNotFound => "message_not_found",
            Self::NotInChannel => "not_in_channel",
            Self::AlreadyReacted => "already_reacted",
            Self::NoReaction => "no_reaction",
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
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Refuse the next `times` calls of a method.
    Refuse {
        method: Method,
        refusal: Refusal,
        times: usize,
    },
    /// Run the next `happenings` of the schedule right now.
    Advance { happenings: usize },
    /// Run the schedule on a clock, at this many happenings a second.
    Live { per_second: f64 },
    /// Stop the clock without stopping the server.
    Still,
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
        if let Action::Refuse {
            method,
            refusal,
            times,
        } = action
        {
            self.refusals.insert(method, (refusal, times));
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
    /// Behind a lock because the world is alive: the schedule and the
    /// clients both write to it. Reads take it shared and never hold it
    /// across an await, so a history page and a scheduled message do not
    /// wait on each other for longer than the page takes to build.
    pub store: Arc<RwLock<Store>>,
    pub control: Arc<Mutex<Control>>,
    /// Where a write becomes a frame every connected client sees.
    pub live: Wire,
    /// Time, so the control endpoint can drive the same schedule the
    /// in-process handle drives.
    pub living: crate::live::Living,
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
    let json = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    // Slack takes some calls as a form and some as JSON, and the client
    // sends each the way Slack wants it. Both arrive here as the same flat
    // map of fields, so no method has to know which it was.
    let form = match json {
        true => parse_json(&body),
        false => parse_form(&body),
    };
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
    let host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("127.0.0.1")
        .to_owned();
    // Reads take the store shared; writes take it exclusively and put a
    // frame on the wire, which is how one client's action reaches the
    // others.
    let answered = match method.writes() {
        true => apply(&server, method, &form),
        false => answer(&server.store.read().expect("store"), method, &form, &host),
    };
    match answered {
        Ok(body) => axum::Json(body).into_response(),
        Err(refusal) => refusal.response(),
    }
}

/// The control surface, for the binary form. One typed action in, over the
/// same enum the in-process handle takes; whatever it made happen back out.
/// There is no second vocabulary to keep in step, and nothing here is
/// reachable from the Slack API path a client uses.
pub async fn control(
    State(server): State<Server>,
    axum::Json(action): axum::Json<Action>,
) -> Response {
    server.control.lock().expect("control").take(action);
    let happened = server.living.take(action);
    axum::Json(json!({"ok": true, "happenings": happened.len()})).into_response()
}

/// What the server saw every client be told, and everywhere they do not
/// agree. The rig reads this rather than trying to compare clients from
/// outside, because the server is the only place that knows what it sent.
pub async fn watched(State(server): State<Server>) -> Response {
    let observations = server
        .live
        .observations(&server.store.read().expect("store"));
    axum::Json(observations).into_response()
}

/// The write side. Every one of these ends in a frame, because that is how
/// Slack tells the client — including the client that made the call — that
/// it happened.
fn apply(
    server: &Server,
    method: Method,
    form: &HashMap<String, String>,
) -> Result<Value, Refusal> {
    let mut store = server.store.write().expect("store");
    match method {
        Method::ChatPostMessage => {
            let channel = ChannelId(field(form, "channel")?);
            let thread_ts = form.get("thread_ts").and_then(|ts| Ts::parse(ts));
            let text = form.get("text").cloned().unwrap_or_default();
            let ts = store.tick();
            let self_id = store.self_id.clone();
            let message = Message {
                ts,
                thread_ts,
                user: self_id,
                text,
                edited: false,
                reply_count: 0,
                latest_reply: None,
                reactions: Vec::new(),
                deleted: false,
                mentions_self: false,
            };
            if !store.post(&channel, message.clone()) {
                return Err(Refusal::ChannelNotFound);
            }
            server.live.publish(socket::message(&channel, &message));
            Ok(json!({"ok": true, "channel": channel.0, "ts": ts.to_string()}))
        }
        Method::ChatUpdate => {
            let channel = ChannelId(field(form, "channel")?);
            let ts = Ts::parse(&field(form, "ts")?).ok_or(Refusal::MessageNotFound)?;
            let text = form.get("text").cloned().unwrap_or_default();
            let names_self = text.contains(&format!("<@{}>", store.self_id.0));
            if !store.edit(&channel, ts, text, names_self) {
                return Err(Refusal::MessageNotFound);
            }
            let at = store.tick();
            let (_, edited) = store
                .message(&channel, ts)
                .ok_or(Refusal::MessageNotFound)?;
            server.live.publish(socket::edited(&channel, at, edited));
            Ok(json!({"ok": true, "channel": channel.0, "ts": ts.to_string()}))
        }
        Method::ReactionsAdd | Method::ReactionsRemove => {
            let added = method == Method::ReactionsAdd;
            let channel = ChannelId(field(form, "channel")?);
            let ts = Ts::parse(&field(form, "timestamp")?).ok_or(Refusal::MessageNotFound)?;
            let name = field(form, "name")?;
            let user = store.self_id.clone();
            if store.message(&channel, ts).is_none() {
                return Err(Refusal::MessageNotFound);
            }
            // Slack says so when nothing changed, and the client is written
            // to read those two as "already in the state you asked for".
            if !store.react(&channel, ts, &user, &name, added) {
                return Err(match added {
                    true => Refusal::AlreadyReacted,
                    false => Refusal::NoReaction,
                });
            }
            server
                .live
                .publish(socket::reaction(added, &channel, ts, &user, &name));
            Ok(json!({"ok": true}))
        }
        Method::ConversationsMark => {
            let channel = ChannelId(field(form, "channel")?);
            let ts = Ts::parse(&field(form, "ts")?).ok_or(Refusal::MessageNotFound)?;
            let kind = store
                .conversation(&channel)
                .ok_or(Refusal::ChannelNotFound)?
                .kind;
            let user = store.self_id.clone();
            store.set_read(&channel, user, ts);
            server.live.publish(socket::marked(kind, &channel, ts));
            Ok(json!({"ok": true}))
        }
        Method::SubscriptionsThreadAdd | Method::SubscriptionsThreadRemove => {
            let channel = ChannelId(field(form, "channel")?);
            let parent = Ts::parse(&field(form, "thread_ts")?).ok_or(Refusal::MessageNotFound)?;
            if store.message(&channel, parent).is_none() {
                return Err(Refusal::MessageNotFound);
            }
            let name = match method {
                Method::SubscriptionsThreadAdd => {
                    store.follow_thread(channel.clone(), parent);
                    "thread_subscribed"
                }
                _ => {
                    store.unfollow_thread(&channel, parent);
                    "thread_unsubscribed"
                }
            };
            server
                .live
                .publish(socket::thread(name, &channel, parent, None));
            Ok(json!({"ok": true}))
        }
        Method::SubscriptionsThreadMark => {
            let channel = ChannelId(field(form, "channel")?);
            let parent = Ts::parse(&field(form, "thread_ts")?).ok_or(Refusal::MessageNotFound)?;
            let ts = Ts::parse(&field(form, "ts")?).ok_or(Refusal::MessageNotFound)?;
            let user = store.self_id.clone();
            store.set_thread_read(&channel, parent, user, ts);
            server
                .live
                .publish(socket::thread("thread_marked", &channel, parent, Some(ts)));
            Ok(json!({"ok": true}))
        }
        _ => Err(Refusal::UnknownMethod),
    }
}

fn answer(
    store: &Store,
    method: Method,
    form: &HashMap<String, String>,
    host: &str,
) -> Result<Value, Refusal> {
    match method {
        Method::RtmConnect => {
            let name = store
                .user(&store.self_id)
                .map(|user| user.handle.clone())
                .unwrap_or_default();
            Ok(socket::rtm(
                &format!("ws://{host}/socket"),
                &store.self_id,
                &name,
            ))
        }
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
        // Every writing method went to `apply` before this was called.
        Method::ChatPostMessage
        | Method::ChatUpdate
        | Method::ReactionsAdd
        | Method::ReactionsRemove
        | Method::ConversationsMark
        | Method::SubscriptionsThreadAdd
        | Method::SubscriptionsThreadRemove
        | Method::SubscriptionsThreadMark => Err(Refusal::UnknownMethod),
    }
}

/// A JSON body flattened to the same field map a form gives, so that
/// `chat.postMessage` and `conversations.mark` are the same shape of handler
/// even though Slack takes them differently.
fn parse_json(body: &str) -> HashMap<String, String> {
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(body) else {
        return HashMap::new();
    };
    fields
        .into_iter()
        .map(|(name, value)| {
            let value = match value {
                Value::String(text) => text,
                other => other.to_string(),
            };
            (name, value)
        })
        .collect()
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

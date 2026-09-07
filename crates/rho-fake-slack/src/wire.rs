//! The typed store turned into the shapes Slack puts on the wire.
//!
//! Everything odd about Slack's JSON is odd in this one file: the three
//! booleans that say what kind of conversation it is, `ts` as a string with
//! six decimal places, a reaction carrying both its users and their count,
//! and a paginated call answering with `response_metadata.next_cursor` that
//! is empty rather than absent when there is no more.

use serde_json::{Value, json};

use crate::store::{Store, Unread};
use crate::types::{Conversation, Kind, Message, User};

pub fn user(user: &User) -> Value {
    json!({
        "id": user.id.0,
        "name": user.handle,
        "real_name": user.display,
        "is_bot": user.bot,
        "deleted": false,
        "profile": {
            "display_name": user.display,
            "real_name": user.display,
            "image_72": user.avatar,
        },
    })
}

pub fn conversation(store: &Store, conversation: &Conversation) -> Value {
    let latest = store.latest(&conversation.id);
    json!({
        "id": conversation.id.0,
        "name": conversation.name,
        "is_channel": matches!(conversation.kind, Kind::Channel),
        "is_private": matches!(conversation.kind, Kind::Private | Kind::Group | Kind::Dm),
        "is_group": matches!(conversation.kind, Kind::Group),
        "is_im": matches!(conversation.kind, Kind::Dm),
        "is_mpim": matches!(conversation.kind, Kind::Group),
        "user": conversation.user.as_ref().map(|user| user.0.clone()),
        "members": conversation.members.iter().map(|member| member.0.clone()).collect::<Vec<_>>(),
        "latest": latest.map(|ts| ts.to_string()),
    })
}

pub fn message(channel: &str, message: &Message) -> Value {
    let mut value = json!({
        "type": "message",
        "ts": message.ts.to_string(),
        "channel": channel,
        "user": message.user.0,
        "text": message.text,
    });
    let object = value.as_object_mut().expect("a message is an object");
    if let Some(thread_ts) = message.thread_ts {
        object.insert("thread_ts".to_owned(), json!(thread_ts.to_string()));
    }
    if message.reply_count > 0 {
        object.insert("thread_ts".to_owned(), json!(message.ts.to_string()));
        object.insert("reply_count".to_owned(), json!(message.reply_count));
        if let Some(latest) = message.latest_reply {
            object.insert("latest_reply".to_owned(), json!(latest.to_string()));
        }
    }
    if message.edited {
        object.insert(
            "edited".to_owned(),
            json!({"user": message.user.0, "ts": message.ts.to_string()}),
        );
    }
    if !message.reactions.is_empty() {
        let reactions: Vec<Value> = message
            .reactions
            .iter()
            .map(|reaction| {
                json!({
                    "name": reaction.name,
                    "count": reaction.users.len(),
                    "users": reaction.users.iter().map(|user| user.0.clone()).collect::<Vec<_>>(),
                })
            })
            .collect();
        object.insert("reactions".to_owned(), json!(reactions));
    }
    value
}

/// One row of `client.counts`. Slack counts messages for DMs and group DMs
/// and reports only mentions for channels, and the client reads the message
/// count out of `dm_count` rather than `unread_count`, so that is the field
/// that has to be right. `last_read` is the person's cursor, not the newest
/// message: a server that answers the latter says everything is read.
pub fn count(
    id: &str,
    unread: Unread,
    latest: Option<String>,
    last_read: Option<String>,
    counts_messages: bool,
) -> Value {
    json!({
        "id": id,
        "last_read": last_read.unwrap_or_default(),
        "latest": latest,
        "has_unreads": unread.messages > 0,
        "mention_count": unread.mentions,
        "dm_count": match counts_messages {
            true => unread.messages,
            false => 0,
        },
        "unread_count": unread.messages,
    })
}

/// One followed thread, as `subscriptions.thread.getView` lists them: the
/// root message carries the channel and the thread's timestamp, and an unread
/// thread spells its cursor as a zero rather than leaving it out.
pub fn followed(channel: &str, thread_ts: &str, last_read: Option<String>) -> Value {
    json!({
        "root_msg": {
            "channel": channel,
            "ts": thread_ts,
            "thread_ts": thread_ts,
        },
        "last_read": last_read.unwrap_or_else(|| "0000000000.000000".to_owned()),
    })
}

/// An `ok: true` body with `response_metadata.next_cursor`, which Slack sends
/// empty rather than leaving out when a page is the last one.
pub fn page(mut body: Value, next: Option<String>) -> Value {
    let object = body.as_object_mut().expect("a response is an object");
    object.insert("ok".to_owned(), json!(true));
    object.insert(
        "response_metadata".to_owned(),
        json!({ "next_cursor": next.unwrap_or_default() }),
    );
    body
}

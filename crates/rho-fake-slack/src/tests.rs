//! What the server promises: the same seed twice is the same workspace, the
//! reads are Slack's shapes, the errors are Slack's codes, and the counts a
//! client badges from cannot disagree with the history it reads.

use crate::api::{Action, Method, Refusal};
use crate::types::{ChannelId, Ts, UserId};
use crate::world::{SELF_ID, Seed};
use crate::{FakeSlack, world};

/// A client that talks to the fake. The crypto provider is installed here
/// because reqwest refuses to build one without it, and a test binary may
/// have several of these in it.
fn client() -> reqwest::Client {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    reqwest::Client::new()
}

/// A world small enough to check by hand, built the same way the big one is.
fn small() -> Seed {
    Seed {
        seed: 7,
        conversations: 8,
        messages: 400,
        people: 6,
        now: 1_760_000_000,
    }
}

#[test]
fn the_same_seed_builds_the_same_workspace() {
    let one = world::build(small());
    let two = world::build(small());
    assert_eq!(one.conversation_count(), two.conversation_count());
    assert_eq!(one.message_count(), two.message_count());
    let ids: Vec<_> = one.conversations().map(|c| c.id.clone()).collect();
    for id in ids {
        let left = one.history(&id, None, None, false, 50).expect("history");
        let right = two.history(&id, None, None, false, 50).expect("history");
        let left: Vec<_> = left
            .messages
            .iter()
            .map(|m| (m.ts, m.text.clone()))
            .collect();
        let right: Vec<_> = right
            .messages
            .iter()
            .map(|m| (m.ts, m.text.clone()))
            .collect();
        assert_eq!(left, right, "the same seed is the same conversation");
    }
}

#[test]
fn the_messages_asked_for_are_the_messages_made() {
    let store = world::build(small());
    assert_eq!(
        store.message_count(),
        400,
        "the seed's message count is a budget the whole world fits in, replies included"
    );
}

#[test]
fn unread_is_what_is_after_the_cursor_and_nothing_else() {
    let mut store = world::build(small());
    let id = store
        .conversations()
        .next()
        .expect("a conversation")
        .id
        .clone();
    let whole = store
        .history(&id, None, None, false, usize::MAX)
        .expect("history");
    let all: Vec<Ts> = whole.messages.iter().map(|m| m.ts).collect();
    let me = UserId(SELF_ID.to_owned());
    store.set_read(&id, me.clone(), all[all.len() - 4]);
    assert_eq!(
        store.unread(&id, &me).messages,
        3,
        "three left after the cursor"
    );
    store.set_read(&id, me.clone(), *all.last().expect("a message"));
    assert_eq!(
        store.unread(&id, &me).messages,
        0,
        "read to the end is nothing left"
    );
}

#[tokio::test]
async fn the_read_side_answers_in_slack_s_shapes() {
    let slack = FakeSlack::start(small()).await.expect("start");
    let client = client();

    let list: serde_json::Value = client
        .post(format!("{}/users.conversations", slack.api_base()))
        .bearer_auth("xoxc-fake")
        .form(&[("limit", "4")])
        .send()
        .await
        .expect("call")
        .json()
        .await
        .expect("json");
    assert_eq!(list["ok"], true);
    assert_eq!(list["channels"].as_array().expect("channels").len(), 4);
    let next = list["response_metadata"]["next_cursor"]
        .as_str()
        .expect("cursor");
    assert!(
        !next.is_empty(),
        "a page that is not the last one says where to go on"
    );

    let id = list["channels"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let history: serde_json::Value = client
        .post(format!("{}/conversations.history", slack.api_base()))
        .bearer_auth("xoxc-fake")
        .form(&[("channel", id.as_str()), ("limit", "5")])
        .send()
        .await
        .expect("call")
        .json()
        .await
        .expect("json");
    let messages = history["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 5);
    let first = messages[0]["ts"].as_str().expect("ts");
    let last = messages[4]["ts"].as_str().expect("ts");
    assert!(first > last, "history comes back newest first");
    assert_eq!(history["has_more"], true);

    let counts: serde_json::Value = client
        .post(format!("{}/client.counts", slack.api_base()))
        .bearer_auth("xoxc-fake")
        .form(&[("thread_counts_by_channel", "true")])
        .send()
        .await
        .expect("call")
        .json()
        .await
        .expect("json");
    assert_eq!(counts["ok"], true);
    let rows = counts["channels"].as_array().expect("channels").len()
        + counts["ims"].as_array().expect("ims").len()
        + counts["mpims"].as_array().expect("mpims").len();
    assert_eq!(rows, slack.store().conversation_count());
}

#[tokio::test]
async fn a_wrong_call_is_refused_the_way_slack_refuses_it() {
    let slack = FakeSlack::start(small()).await.expect("start");
    let client = client();

    let unauthorised: serde_json::Value = client
        .post(format!("{}/users.conversations", slack.api_base()))
        .form(&[("limit", "1")])
        .send()
        .await
        .expect("call")
        .json()
        .await
        .expect("json");
    assert_eq!(unauthorised["ok"], false);
    assert_eq!(unauthorised["error"], "invalid_auth");

    let missing: serde_json::Value = client
        .post(format!("{}/conversations.history", slack.api_base()))
        .bearer_auth("xoxc-fake")
        .form(&[("channel", "C-nope")])
        .send()
        .await
        .expect("call")
        .json()
        .await
        .expect("json");
    assert_eq!(missing["error"], "channel_not_found");

    slack.take(Action::Refuse {
        method: Method::ClientCounts,
        refusal: Refusal::RateLimited {
            retry_after_seconds: 7,
        },
        times: 1,
    });
    let limited = client
        .post(format!("{}/client.counts", slack.api_base()))
        .bearer_auth("xoxc-fake")
        .send()
        .await
        .expect("call");
    assert_eq!(limited.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        limited.headers().get("retry-after").expect("retry-after"),
        "7",
        "a rate limit says when to come back, or a client never backs off"
    );
    let after: serde_json::Value = client
        .post(format!("{}/client.counts", slack.api_base()))
        .bearer_auth("xoxc-fake")
        .send()
        .await
        .expect("call")
        .json()
        .await
        .expect("json");
    assert_eq!(
        after["ok"], true,
        "the refusal was for one call, not for good"
    );
}

#[tokio::test]
async fn a_thread_comes_back_parent_first() {
    let slack = FakeSlack::start(small()).await.expect("start");
    let store = slack.store();
    let (channel, parent) = store
        .conversations()
        .find_map(|conversation| {
            let window = store
                .history(&conversation.id, None, None, false, usize::MAX)
                .expect("history");
            let parent = window.messages.iter().find(|m| m.reply_count > 0)?;
            Some((conversation.id.clone(), parent.ts))
        })
        .expect("a thread somewhere in the world");

    let client = client();
    let replies: serde_json::Value = client
        .post(format!("{}/conversations.replies", slack.api_base()))
        .bearer_auth("xoxc-fake")
        .form(&[("channel", channel.0.as_str()), ("ts", &parent.to_string())])
        .send()
        .await
        .expect("call")
        .json()
        .await
        .expect("json");
    let messages = replies["messages"].as_array().expect("messages");
    assert_eq!(messages[0]["ts"], parent.to_string(), "the parent is first");
    assert!(messages.len() > 1, "and its replies follow it");
    assert_eq!(messages[1]["thread_ts"], parent.to_string());
}

#[test]
fn a_conversation_that_is_not_there_is_not_invented() {
    let store = world::build(small());
    assert!(
        store
            .history(&ChannelId("C-nope".to_owned()), None, None, false, 10)
            .is_none()
    );
}

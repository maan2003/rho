//! The local mirror, against the fake: what rho keeps on disk, and how
//! little it asks Slack for once it has it.
//!
//! The request pattern is the point. An unofficial client that walks history
//! in the background is the kind Slack bans, so these tests assert what was
//! *not* fetched as carefully as what was.

use std::sync::Arc;

use rho_slack::api::Client;
use rho_slack::config::Credentials;
use rho_slack::fake::Fake;
use rho_slack::mirror::{Mirror, Scope};
use rho_slack::types::{ChannelId, Message, Ts};
use serde_json::json;

fn client(fake: &Fake) -> Arc<Client> {
    let credentials = Credentials::parse("acme", "xoxc-test", "cookie").unwrap();
    Arc::new(Client::with_base(credentials, fake.api_base()).unwrap())
}

fn mirror() -> (tempfile::TempDir, Mirror) {
    let dir = tempfile::tempdir().unwrap();
    let mirror = Mirror::open(dir.path().join("slack.redb")).unwrap();
    (dir, mirror)
}

fn message(ts: &str, text: &str) -> Message {
    Message {
        ts: Ts::from(ts),
        thread_ts: None,
        channel: ChannelId::from("C1"),
        user: Some(rho_slack::types::UserId::from("UD")),
        bot_name: None,
        blocks: Vec::new(),
        text: text.to_owned(),
        attachments: Vec::new(),
        files: Vec::new(),
        subtype: None,
        reply_count: 0,
        latest_reply: None,
        edited: false,
        reactions: Vec::new(),
    }
}

fn scope() -> Scope {
    Scope::conversation("acme", &ChannelId::from("C1"))
}

/// Opening the conversation list reads names and unread counts. It must not
/// read a single message: that is the fan-out over the workspace that gets a
/// client banned.
#[tokio::test]
async fn opening_the_list_fetches_no_history() {
    let fake = Fake::start().await.unwrap();
    fake.add_user_named("UD", "david", "David");
    fake.add_channel("C1", "design");
    fake.add_message(
        "C1",
        json!({"ts": "100.000000", "user": "UD", "text": "morning"}),
    );
    let client = client(&fake);

    let users = client.users().await.unwrap();
    let conversations = client.conversations().await.unwrap();
    client.counts().await.unwrap();

    assert!(!users.is_empty(), "the list has names");
    assert!(!conversations.is_empty(), "and conversations");
    assert_eq!(
        fake.calls("conversations.history"),
        0,
        "the list reads no history"
    );
    assert_eq!(fake.calls("conversations.replies"), 0, "and no threads");
}

/// Reopening a conversation rho already holds asks only for what came after
/// it. Everything already on disk is read from disk.
#[tokio::test]
async fn a_mirrored_conversation_asks_only_for_what_is_newer() {
    let fake = Fake::start().await.unwrap();
    fake.add_channel("C1", "design");
    for (ts, text) in [("100.000000", "one"), ("200.000000", "two")] {
        fake.add_message("C1", json!({"ts": ts, "user": "UD", "text": text}));
    }
    let client = client(&fake);
    let (_dir, mirror) = mirror();
    let channel = ChannelId::from("C1");

    // First open: nothing cached, so the newest page is fetched whole.
    let page = client.conversations_history(&channel, None).await.unwrap();
    mirror.insert_messages(&scope(), &page.messages);
    assert_eq!(mirror.newest_ts(&scope()), Some(Ts::from("200.000000")));

    // Reopen: the mirror renders first and bounds the request.
    let cached = mirror.newest_chunk(&scope(), 50);
    assert_eq!(
        cached.len(),
        2,
        "the conversation is readable before asking"
    );
    let since = mirror.newest_ts(&scope()).unwrap();
    let refreshed = client
        .conversations_history_since(&channel, &since)
        .await
        .unwrap();

    assert_eq!(
        fake.last_field("conversations.history", "oldest")
            .as_deref(),
        Some("200.000000"),
        "the refresh is bounded by what the mirror holds"
    );
    assert!(
        refreshed.messages.is_empty(),
        "and nothing already mirrored comes back"
    );
}

/// Coming back after downtime appends the live tail. If the tail does not
/// reach the newest message rho already had, the hole is written down rather
/// than papered over: the transcript must never imply a continuity it does
/// not have.
#[test]
fn a_tail_that_does_not_reach_the_cache_leaves_a_gap() {
    let (_dir, mirror) = mirror();
    let scope = scope();
    mirror.insert_messages(&scope, &[message("100.000000", "before the outage")]);

    // The socket comes back with messages that plainly skip a stretch.
    let tail = [
        message("500.000000", "after the outage"),
        message("600.000000", "and later"),
    ];
    mirror.insert_messages(&scope, &tail);
    mirror.put_gap(&scope, &Ts::from("500.000000"), &Ts::from("500.000000"));

    let (at, gap) = mirror
        .gap_below(&scope, None)
        .expect("the hole is a record");
    assert_eq!(at, Ts::from("500.000000"));
    assert_eq!(
        gap.page_before,
        Ts::from("500.000000"),
        "the gap carries the cursor that fills it"
    );

    let chunk = mirror.newest_chunk(&scope, 50);
    assert_eq!(
        chunk
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        vec!["after the outage", "and later"],
        "opening shows the newest chunk, not a run across the hole"
    );

    // Filling it joins the two runs.
    mirror.insert_messages(&scope, &[message("300.000000", "during the outage")]);
    mirror.clear_gap(&scope, &Ts::from("500.000000"));
    assert_eq!(mirror.newest_chunk(&scope, 50).len(), 4);
}

/// `has_more: false` is the beginning of history, and that is a fact worth
/// keeping: `shift-p` at the top is then an echo rather than a request.
#[test]
fn the_beginning_of_history_is_remembered() {
    let (_dir, mirror) = mirror();
    let scope = scope();
    assert!(!mirror.history_begins(&scope));
    mirror.set_history_begins(&scope);
    assert!(
        mirror.history_begins(&scope),
        "the top of the conversation is known without asking again"
    );
}

/// A deletion removes the message from the mirror too. A copy the user
/// cannot see anywhere else is not a cache, it is a leak.
#[test]
fn a_deleted_message_leaves_the_mirror() {
    let (_dir, mirror) = mirror();
    let scope = scope();
    mirror.insert_messages(
        &scope,
        &[message("100.000000", "one"), message("200.000000", "two")],
    );
    mirror.remove_message(&scope, &Ts::from("200.000000"));

    let held = mirror.all_messages(&scope);
    assert_eq!(
        held.iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        vec!["one"],
        "the deleted message is gone from disk"
    );
    assert_eq!(mirror.newest_ts(&scope), Some(Ts::from("100.000000")));
}

/// An edit overwrites in place. The timestamp is the identity, so a changed
/// message stays exactly one message.
#[test]
fn an_edited_message_overwrites_in_place() {
    let (_dir, mirror) = mirror();
    let scope = scope();
    mirror.insert_messages(&scope, &[message("100.000000", "frist")]);
    let mut corrected = message("100.000000", "first");
    corrected.edited = true;
    mirror.insert_messages(&scope, &[corrected]);

    let held = mirror.all_messages(&scope);
    assert_eq!(held.len(), 1, "an edit is not a second message");
    assert_eq!(held[0].text, "first");
    assert!(held[0].edited, "and it says it was edited");
}

/// A ping brings its own context, once. The budget is what Slack's own web
/// client spends when someone clicks the notification: the window the message
/// sits in, and the thread if it is one. A ping for something already
/// mirrored spends nothing.
#[tokio::test]
async fn a_ping_costs_one_window_and_one_thread() {
    let fake = Fake::start().await.unwrap();
    fake.add_channel("C1", "design");
    for index in 0..60 {
        fake.add_message(
            "C1",
            json!({
                "ts": format!("{}.000000", 100 + index),
                "user": "UD",
                "text": format!("message {index}"),
            }),
        );
    }
    let client = client(&fake);
    let (_dir, mirror) = mirror();
    let channel = ChannelId::from("C1");
    let pinged = Ts::from("150.000000");

    assert!(
        !mirror.holds(&scope(), &pinged),
        "the ping is news the first time"
    );
    let window = client
        .conversations_history_around(&channel, &pinged, 40)
        .await
        .unwrap();
    mirror.insert_messages(&scope(), &window.messages);
    client
        .conversations_replies(&channel, &pinged, None)
        .await
        .unwrap();

    assert_eq!(
        fake.calls("conversations.history"),
        1,
        "one window, never a page back"
    );
    assert_eq!(fake.calls("conversations.replies"), 1, "and one thread");
    assert_eq!(
        fake.last_field("conversations.history", "latest")
            .as_deref(),
        Some("150.000000"),
        "the window ends at the message the ping named"
    );
    assert_eq!(
        fake.last_field("conversations.history", "limit").as_deref(),
        Some("40"),
        "and is bounded"
    );
    assert!(
        window
            .messages
            .iter()
            .all(|message| !message.ts.is_newer_than(&pinged)),
        "nothing beyond the ping is pulled in"
    );

    // The second ping for the same message is free: the guard is the mirror.
    assert!(mirror.holds(&scope(), &pinged));
    assert_eq!(
        fake.calls("conversations.history"),
        1,
        "a mirrored ping costs no request"
    );
}

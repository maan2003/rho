//! Transport against a fake Slack: the websocket, the feed poll, loading a
//! thread, and sending a reply. These are the parts that cannot be tested by
//! parsing alone, because the bugs live in the loop, not the payload.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use futures::channel::mpsc;
use rho_slack::api::Client;
use rho_slack::config::Credentials;
use rho_slack::events::WsEvent;
use rho_slack::fake::Fake;
use rho_slack::model::{Change, Model, Waiting};
use rho_slack::socket::{Timings, Wire, poll_feed, run_feed, run_socket};
use rho_slack::types::{ChannelId, Ts};
use serde_json::json;
use tokio::sync::Notify;

/// Short enough that a reconnect and a poll happen inside a test.
fn timings() -> Timings {
    Timings {
        ping_interval: Duration::from_millis(50),
        pong_grace: Duration::from_millis(300),
        feed_interval: Duration::from_millis(80),
    }
}

fn client(fake: &Fake) -> Arc<Client> {
    let credentials = Credentials::parse("acme", "xoxc-test", "cookie").unwrap();
    Arc::new(Client::with_base(credentials, fake.api_base()).unwrap())
}

async fn next_wire(receiver: &mut mpsc::UnboundedReceiver<Wire>) -> Wire {
    tokio::time::timeout(Duration::from_secs(5), receiver.next())
        .await
        .expect("the transport went quiet")
        .expect("the transport hung up")
}

/// The socket announces who we are before it dials, so a test that wants to
/// push a frame has to wait for the socket itself. The catch-up notification
/// fires exactly once the connection is live.
async fn wait_until_live(catch_up: &Notify) {
    tokio::time::timeout(Duration::from_secs(5), catch_up.notified())
        .await
        .expect("the socket never came up");
}

#[tokio::test]
async fn the_websocket_connects_and_delivers_a_mention_live() {
    let fake = Fake::start().await.unwrap();
    let (sender, mut receiver) = mpsc::unbounded();
    let catch_up = Arc::new(Notify::new());
    let _socket = tokio::spawn(run_socket(
        client(&fake),
        sender,
        catch_up.clone(),
        timings(),
    ));

    let Wire::Connected(connection) = next_wire(&mut receiver).await else {
        panic!("the first thing a session learns is who it is");
    };
    assert_eq!(connection.self_id.0, fake.self_id());
    assert_eq!(connection.team_name, "acme");
    wait_until_live(&catch_up).await;

    assert!(
        matches!(next_wire(&mut receiver).await, Wire::Frame(WsEvent::Hello)),
        "Slack greets a fresh socket before anything else"
    );
    fake.push_frame(json!({
        "type": "message",
        "channel": "C1",
        "ts": "100.0",
        "user": "U1",
        "text": "hey <@ME>",
    }));
    let Wire::Frame(WsEvent::Message(message)) = next_wire(&mut receiver).await else {
        panic!("a pushed message arrives as a message");
    };
    assert_eq!(message.channel, ChannelId("C1".into()));
    assert_eq!(message.text, "hey <@ME>");
}

#[tokio::test]
async fn a_dropped_socket_reconnects_and_triggers_a_catch_up_poll() {
    let fake = Fake::start().await.unwrap();
    let (sender, mut receiver) = mpsc::unbounded();
    let catch_up = Arc::new(Notify::new());
    let _socket = tokio::spawn(run_socket(
        client(&fake),
        sender,
        catch_up.clone(),
        timings(),
    ));
    assert!(matches!(next_wire(&mut receiver).await, Wire::Connected(_)));
    wait_until_live(&catch_up).await;
    let connects = fake.calls("rtm.connect");

    fake.drop_sockets();
    // The socket comes back on its own. The second connect is what proves
    // the reconnect happened rather than the loop simply surviving.
    tokio::time::timeout(Duration::from_secs(5), async {
        while fake.calls("rtm.connect") <= connects {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the socket never came back");

    // And a live message flows again over the new socket.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            fake.push_frame(json!({
                "type": "message", "channel": "C1", "ts": "200.0", "user": "U1", "text": "back",
            }));
            if let Ok(wire) =
                tokio::time::timeout(Duration::from_millis(200), receiver.next()).await
                && matches!(wire, Some(Wire::Frame(WsEvent::Message(_))))
            {
                return;
            }
        }
    })
    .await
    .expect("no message after the reconnect");
}

#[tokio::test]
async fn the_socket_stays_up_across_pings_and_notifies_catch_up_on_connect() {
    let fake = Fake::start().await.unwrap();
    let (sender, mut receiver) = mpsc::unbounded();
    let catch_up = Arc::new(Notify::new());
    let _socket = tokio::spawn(run_socket(
        client(&fake),
        sender,
        catch_up.clone(),
        timings(),
    ));
    assert!(matches!(next_wire(&mut receiver).await, Wire::Connected(_)));
    wait_until_live(&catch_up).await;

    // Several ping intervals pass. A socket whose pongs were not being read
    // would be declared dead and reconnect; this one must not.
    let connects = fake.calls("rtm.connect");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        fake.calls("rtm.connect"),
        connects,
        "answered pings must not look like a dead socket"
    );
}

#[tokio::test]
async fn the_feed_poll_dedupes_against_the_websocket() {
    let fake = Fake::start().await.unwrap();
    fake.add_channel("C1", "design");
    fake.add_feed_mention("C1", "300.0");
    let client = client(&fake);

    let items = poll_feed(&client, None).await.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].ts, Ts("300.0".into()));

    let mut model = Model::new(rho_slack::WorkspaceName("acme".into()));
    model.set_self(rho_slack::UserId("ME".into()));
    assert!(matches!(
        model.note_activity(&items[0], 0),
        Some(Change::Raised(_))
    ));
    // The same mention arriving over the socket raises nothing further.
    let message = rho_slack::api::parse_message(
        &json!({"ts": "300.0", "user": "U1", "text": "hey <@ME>"}),
        &ChannelId("C1".into()),
    )
    .unwrap();
    assert_eq!(model.note_message(&message, 0), None);
    assert_eq!(model.obligations(0).len(), 1);
}

#[tokio::test]
async fn a_failing_feed_reports_every_failure_and_then_recovers() {
    let fake = Fake::start().await.unwrap();
    fake.add_feed_mention("C1", "400.0");
    fake.fail_next("activity.feed", 2);
    let (sender, mut receiver) = mpsc::unbounded();
    let catch_up = Arc::new(Notify::new());
    let _feed = tokio::spawn(run_feed(client(&fake), sender, catch_up.clone(), timings()));

    for _ in 0..2 {
        let Wire::FeedFailed(error) = next_wire(&mut receiver).await else {
            panic!("a refused poll is reported, not swallowed");
        };
        assert!(error.contains("activity.feed"), "{error}");
    }
    let Wire::Feed(items) = next_wire(&mut receiver).await else {
        panic!("the poll recovers on its own");
    };
    assert_eq!(items.len(), 1);
}

#[tokio::test]
async fn a_thread_loads_and_a_reply_is_sent_into_it() {
    let fake = Fake::start().await.unwrap();
    fake.add_user("U1", "ada");
    fake.add_channel("C1", "design");
    fake.add_message(
        "C1",
        json!({"ts": "500.0", "user": "U1", "text": "can you look at this?"}),
    );
    fake.add_message(
        "C1",
        json!({"ts": "501.0", "thread_ts": "500.0", "user": "U1", "text": "still open"}),
    );
    let client = client(&fake);

    let page = client
        .conversations_replies(&ChannelId("C1".into()), &Ts("500.0".into()), None)
        .await
        .unwrap();
    assert_eq!(page.messages.len(), 2);
    assert_eq!(page.messages[0].text, "can you look at this?");

    let sent = client
        .post_message(
            &ChannelId("C1".into()),
            Some(&Ts("500.0".into())),
            "looking now",
        )
        .await
        .unwrap();
    assert!(!sent.0.is_empty());
    let posted = fake.posted();
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].thread_ts.as_deref(), Some("500.0"));
    assert_eq!(posted[0].text, "looking now");

    // And the reply is the done verdict once it comes back through the model.
    let mut model = Model::new(rho_slack::WorkspaceName("acme".into()));
    model.set_self(rho_slack::UserId("ME".into()));
    model.add_conversations(client.conversations().await.unwrap());
    for message in client
        .conversations_replies(&ChannelId("C1".into()), &Ts("500.0".into()), None)
        .await
        .unwrap()
        .messages
    {
        model.note_message(&message, 0);
    }
    let key = model.key(&ChannelId("C1".into()), &Ts("500.0".into()));
    assert_eq!(model.thread(&key).unwrap().waiting(), Waiting::OnThem);
}

#[tokio::test]
async fn history_marking_and_the_conversation_list_come_from_slack() {
    let fake = Fake::start().await.unwrap();
    fake.add_user("U1", "ada");
    fake.add_channel("C1", "design");
    fake.add_dm("D1", "U1");
    fake.set_count("C1", true, 2, "600.0");
    fake.add_message("C1", json!({"ts": "599.0", "user": "U1", "text": "older"}));
    fake.add_message("C1", json!({"ts": "600.0", "user": "U1", "text": "newer"}));
    let client = client(&fake);

    let history = client
        .conversations_history(&ChannelId("C1".into()), None)
        .await
        .unwrap();
    assert_eq!(
        history
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        vec!["older", "newer"],
        "the surface renders oldest first"
    );

    client
        .mark_read(&ChannelId("C1".into()), &Ts("600.0".into()))
        .await
        .unwrap();
    assert_eq!(fake.marked(), vec![("C1".to_owned(), "600.0".to_owned())]);

    let mut model = Model::new(rho_slack::WorkspaceName("acme".into()));
    model.add_users(client.users().await.unwrap());
    model.add_conversations(client.conversations().await.unwrap());
    model.set_counts(client.counts().await.unwrap().conversations);
    let rows = model.conversation_rows();
    assert_eq!(rows[0].label, "#design");
    assert_eq!(rows[0].mention_count, 2);
    assert!(rows[0].unread);
    assert_eq!(rows[1].label, "@ada", "a DM is named from the roster");
}

/// Phase 0 of the UX checklist: the fake can reach the state the reference
/// screenshot was taken in. Everything the rendering items are judged
/// against comes from here, so the fixture itself is worth a test.
#[tokio::test]
async fn the_fake_serves_the_reference_workspace() {
    let fake = Fake::start().await.unwrap();
    fake.add_user_named("UD", "david", "David");
    fake.add_group("G1", "mpdm-david--manmeet--keith-1", &["ME", "UD", "UK"]);
    fake.add_private_channel("P1", "founders");
    for index in 0..120 {
        fake.add_message(
            "G1",
            json!({"ts": format!("{}.0", 1000 + index), "user": "UD", "text": "backlog"}),
        );
    }
    fake.set_last_read("G1", "1050.0");
    fake.add_emoji("forrest_gump_wave", "https://example.com/wave.png");
    let client = client(&fake);

    let conversations = client.conversations().await.unwrap();
    let group = conversations
        .iter()
        .find(|conversation| conversation.id == ChannelId("G1".into()))
        .expect("the group DM is served");
    assert_eq!(group.kind, rho_slack::types::ConversationKind::Group);
    assert_eq!(group.name, "mpdm-david--manmeet--keith-1");
    assert!(
        conversations
            .iter()
            .any(|conversation| conversation.id == ChannelId("P1".into())),
        "a private channel is a conversation like any other"
    );

    // A display name that differs from the handle is what the roster shows.
    assert_eq!(
        client
            .user_info(&rho_slack::UserId("UD".into()))
            .await
            .unwrap()
            .name,
        "David"
    );

    // History pages backwards, newest page first.
    let newest = client
        .conversations_history(&ChannelId("G1".into()), None)
        .await
        .unwrap();
    assert_eq!(newest.messages.len(), rho_slack::api::PAGE);
    let cursor = newest
        .older_cursor
        .expect("120 messages do not fit in one page");
    let older = client
        .conversations_history(&ChannelId("G1".into()), Some(&cursor))
        .await
        .unwrap();
    assert!(
        older
            .messages
            .last()
            .unwrap()
            .ts
            .is_newer_than(&Ts("0".into())),
        "the second page is real history"
    );
    assert!(
        !older
            .messages
            .iter()
            .any(|message| newest.messages.contains(message)),
        "paging never repeats a message"
    );

    // The two fields no client method reads yet, which the unread rule and
    // the emoji table will: they are served now so those items have a
    // fixture to build against.
    let raw = |method: &str, field: &str, value: &str| {
        let base = fake.api_base().to_owned();
        let method = method.to_owned();
        let body = format!("{field}={value}");
        async move {
            reqwest::Client::new()
                .post(format!("{base}/{method}"))
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(body)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    let info = raw("conversations.info", "channel", "G1").await;
    assert_eq!(info["channel"]["last_read"], json!("1050.0"));
    let emoji = raw("emoji.list", "token", "x").await;
    assert!(emoji["emoji"]["forrest_gump_wave"].is_string());

    // Avatars are real bytes on the same host, which is what the avatar
    // cache will fetch.
    let profile = raw("users.info", "user", "UD").await;
    let avatar = profile["user"]["profile"]["image_48"]
        .as_str()
        .expect("every user carries a picture")
        .to_owned();
    assert!(profile["user"]["avatar_hash"].is_string());
    let bytes = reqwest::get(avatar).await.unwrap().bytes().await.unwrap();
    assert_eq!(&bytes[1..4], b"PNG");
}

/// Checklist 0.7: the QA run drives live events by poking the fake over
/// HTTP, and every one of them both moves the workspace and goes out on the
/// socket. All the mocking is server side: nothing here reaches into the
/// client.
#[tokio::test]
async fn the_control_route_drives_live_events_over_the_socket() {
    let fake = Fake::start().await.unwrap();
    fake.add_user_named("UD", "david", "David");
    fake.add_channel("C1", "design");
    fake.set_count("C1", false, 0, "100.0");
    fake.add_message("C1", json!({"ts": "100.0", "user": "UD", "text": "parent"}));

    let (sender, mut receiver) = mpsc::unbounded();
    let catch_up = Arc::new(Notify::new());
    let _socket = tokio::spawn(run_socket(
        client(&fake),
        sender,
        catch_up.clone(),
        timings(),
    ));
    let Wire::Connected(_) = next_wire(&mut receiver).await else {
        panic!("the socket announces itself first");
    };
    wait_until_live(&catch_up).await;
    assert!(matches!(
        next_wire(&mut receiver).await,
        Wire::Frame(WsEvent::Hello)
    ));

    let poke = reqwest::Client::new();
    let control = |request: serde_json::Value| {
        let poke = poke.clone();
        let url = fake.control_url();
        async move {
            let response: serde_json::Value = poke
                .post(url)
                .json(&request)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(response["ok"], json!(true), "control refused {response}");
            response["ts"].as_str().unwrap_or_default().to_owned()
        }
    };

    let ts = control(json!({
        "kind": "message",
        "channel": "C1",
        "user": "UD",
        "text": "posted from the control route <@ME>",
    }))
    .await;
    let Wire::Frame(WsEvent::Message(message)) = next_wire(&mut receiver).await else {
        panic!("a live message reaches the socket");
    };
    assert_eq!(message.text, "posted from the control route <@ME>");
    assert_eq!(message.channel, ChannelId("C1".into()));

    control(json!({
        "kind": "reply",
        "channel": "C1",
        "thread_ts": ts,
        "user": "UD",
        "text": "and a reply",
    }))
    .await;
    let Wire::Frame(WsEvent::Message(reply)) = next_wire(&mut receiver).await else {
        panic!("a live reply reaches the socket");
    };
    assert_eq!(reply.thread_root(), Ts(ts.clone()));

    // A mention moves the unread counter the conversation list reads, and
    // lands in the feed, which is the source rho trusts over the socket.
    let client = client(&fake);
    let counts = client.counts().await.unwrap();
    let count = counts
        .conversations
        .iter()
        .find(|count| count.channel == ChannelId("C1".into()))
        .expect("the channel is counted");
    assert!(count.has_unreads);
    assert_eq!(count.mention_count, 1);
    assert!(!poll_feed(&client, None).await.unwrap().is_empty());

    control(json!({"kind": "reaction", "channel": "C1", "ts": &ts, "user": "UD", "name": "eyes"}))
        .await;
    control(json!({"kind": "edit", "channel": "C1", "ts": &ts, "text": "edited by control"})).await;
    // History is the proof the events are real: a client that reopens the
    // conversation must see exactly what the socket announced.
    let history = client
        .conversations_history(&ChannelId("C1".into()), None)
        .await
        .unwrap();
    let live = history
        .messages
        .iter()
        .find(|message| message.ts == Ts(ts.clone()))
        .expect("the live message is in history");
    assert_eq!(live.text, "edited by control");

    control(json!({"kind": "delete", "channel": "C1", "ts": &ts})).await;
    let history = client
        .conversations_history(&ChannelId("C1".into()), None)
        .await
        .unwrap();
    assert!(
        !history
            .messages
            .iter()
            .any(|message| message.ts == Ts(ts.clone())),
        "a deleted message leaves history"
    );
}

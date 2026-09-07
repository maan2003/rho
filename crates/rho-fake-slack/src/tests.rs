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
    // The guard is taken and given back before anything is awaited: holding
    // it across an await would hold up the schedule and everything else.
    let (channel, parent) = {
        let store = slack.store();
        store
            .conversations()
            .find_map(|conversation| {
                let window = store
                    .history(&conversation.id, None, None, false, usize::MAX)
                    .expect("history");
                let parent = window.messages.iter().find(|m| m.reply_count > 0)?;
                Some((conversation.id.clone(), parent.ts))
            })
            .expect("a thread somewhere in the world")
    };

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

/// A `rho-slack` client pointed at this server, with a session that is not a
/// real one because nothing here checks a real one.
fn connected(slack: &FakeSlack) -> rho_slack::api::Client {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    rho_slack::api::Client::with_base(
        rho_slack::config::Credentials {
            workspace: rho_slack::config::WorkspaceName("acme".to_owned()),
            token: "xoxc-test".to_owned(),
            cookie: "d=test".to_owned(),
        },
        slack.api_base(),
    )
    .expect("client")
}

/// Opens the socket the way rho does: `rtm.connect` for the URL, then the
/// handshake `rho-slack` itself builds.
async fn socket(
    slack: &FakeSlack,
    client: &rho_slack::api::Client,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let rtm = client.rtm_connect().await.expect("rtm.connect");
    assert_eq!(rtm.self_id.0, SELF_ID, "the socket signs in as the reader");
    let request = client.socket_request(&rtm.url).expect("handshake");
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("connecting");
    let _ = slack;
    socket
}

/// The next frame, or nothing if the server stayed quiet — a test that
/// waits forever for a frame that is not coming is a hung test.
async fn frame(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Option<serde_json::Value> {
    use futures_util::StreamExt as _;
    let next = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next()).await;
    match next {
        Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text)))) => {
            serde_json::from_str(&text).ok()
        }
        _ => None,
    }
}

#[tokio::test]
async fn the_socket_opens_with_hello_and_answers_a_ping() {
    use futures_util::SinkExt as _;
    let slack = FakeSlack::start(small()).await.expect("server");
    let client = connected(&slack);
    let mut socket = socket(&slack, &client).await;
    assert_eq!(
        frame(&mut socket).await.expect("hello")["type"],
        serde_json::json!("hello"),
        "a socket says hello before it says anything else"
    );
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({"type": "ping", "id": 7})
                .to_string()
                .into(),
        ))
        .await
        .expect("ping");
    let pong = frame(&mut socket).await.expect("pong");
    assert_eq!(pong["type"], serde_json::json!("pong"));
    assert_eq!(
        pong["reply_to"],
        serde_json::json!(7),
        "the pong names the ping, which is how the client knows it is alive"
    );
}

#[tokio::test]
async fn what_the_schedule_does_arrives_on_the_socket() {
    let slack = FakeSlack::start(small()).await.expect("server");
    let client = connected(&slack);
    let mut socket = socket(&slack, &client).await;
    assert_eq!(frame(&mut socket).await.expect("hello")["type"], "hello");
    // Wait for the connection to be counted before making anything happen,
    // so this is a test of the wire and not of a race.
    for _ in 0..100 {
        if slack.connected() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let happenings = slack.advance(40);
    // A reply is a message frame too — it carries `thread_ts` rather than a
    // subtype, which is exactly how Slack sends one.
    let said = happenings
        .iter()
        .filter(|happening| {
            matches!(
                happening,
                crate::Happening::Posted { .. } | crate::Happening::Replied { .. }
            )
        })
        .count();
    assert!(said > 0, "forty happenings include somebody talking");
    let mut messages = 0;
    for _ in 0..happenings.len() {
        let Some(frame) = frame(&mut socket).await else {
            break;
        };
        // Every frame is one the client understands; nothing here is a
        // shape rho would drop on the floor.
        assert_ne!(
            rho_slack::events::parse(&frame),
            rho_slack::events::WsEvent::Ignored,
            "the server sent a frame the client ignores: {frame}"
        );
        if frame["type"] == "message" && frame["subtype"].is_null() {
            messages += 1;
        }
    }
    assert_eq!(
        messages, said,
        "every message the schedule wrote reached the socket"
    );
}

#[tokio::test]
async fn the_same_seed_is_the_same_hour() {
    let one = FakeSlack::start(small()).await.expect("server");
    let two = FakeSlack::start(small()).await.expect("server");
    assert_eq!(
        one.advance(200),
        two.advance(200),
        "time comes out of the seed, so two runs live the same hour"
    );
    assert_eq!(
        one.store().message_count(),
        two.store().message_count(),
        "and end the same size"
    );
}

#[tokio::test]
async fn a_clients_write_reaches_the_other_clients() {
    let slack = FakeSlack::start(small()).await.expect("server");
    let writer = connected(&slack);
    let reader = connected(&slack);
    let mut socket = socket(&slack, &reader).await;
    assert_eq!(frame(&mut socket).await.expect("hello")["type"], "hello");
    for _ in 0..100 {
        if slack.connected() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let channel = {
        let store = slack.store();
        store.conversations().next().expect("a channel").id.clone()
    };
    let channel_id = rho_slack::types::ChannelId(channel.0.clone());
    let ts = writer
        .post_message(&channel_id, None, "shipping it")
        .await
        .expect("post");
    let sent = frame(&mut socket).await.expect("the message");
    assert_eq!(sent["type"], "message");
    assert_eq!(sent["channel"], serde_json::json!(channel.0));
    assert_eq!(sent["ts"], serde_json::json!(ts.0));
    assert_eq!(
        slack.store().latest(&channel).map(|ts| ts.to_string()),
        Some(ts.0.clone()),
        "and the server kept it, not just announced it"
    );

    // The same message, reacted to and then read: the two other things a
    // client does that every other client has to see.
    let store_ts = Ts::parse(&ts.0).expect("ts");
    writer
        .add_reaction(&channel_id, &ts, "tada")
        .await
        .expect("react");
    let reacted = frame(&mut socket).await.expect("the reaction");
    assert_eq!(reacted["type"], "reaction_added");
    assert_eq!(reacted["item"]["ts"], serde_json::json!(ts.0));
    writer.mark_read(&channel_id, &ts).await.expect("mark");
    let marked = frame(&mut socket).await.expect("the read cursor");
    assert!(
        marked["type"] == "channel_marked" || marked["type"] == "im_marked",
        "a read cursor moves with the frame for that kind of conversation: {marked}"
    );
    let store = slack.store();
    assert_eq!(
        store.read_cursor(&channel, &UserId(SELF_ID.to_owned())),
        Some(store_ts),
        "the reader's cursor is where they said it was"
    );
    assert_eq!(
        store.unread(&channel, &UserId(SELF_ID.to_owned())).messages,
        0,
        "and nothing is unread behind it"
    );
}

#[tokio::test]
async fn pressing_a_reaction_twice_is_not_two_reactions() {
    let slack = FakeSlack::start(small()).await.expect("server");
    let client = connected(&slack);
    let channel = slack
        .store()
        .conversations()
        .next()
        .expect("a channel")
        .id
        .clone();
    let latest = slack.store().latest(&channel).expect("a message");
    let channel_id = rho_slack::types::ChannelId(channel.0.clone());
    let ts = rho_slack::types::Ts(latest.to_string());
    client
        .add_reaction(&channel_id, &ts, "tada")
        .await
        .expect("first");
    // The client treats Slack's `already_reacted` as "done", which is only
    // true if the server actually says it.
    client
        .add_reaction(&channel_id, &ts, "tada")
        .await
        .expect("second");
    {
        let store = slack.store();
        let (_, message) = store.message(&channel, latest).expect("the message");
        let reaction = message
            .reactions
            .iter()
            .find(|reaction| reaction.name == "tada")
            .expect("the reaction");
        assert_eq!(
            reaction.users,
            vec![UserId(SELF_ID.to_owned())],
            "one person, once"
        );
    }
    client
        .remove_reaction(&channel_id, &ts, "tada")
        .await
        .expect("off");
    client
        .remove_reaction(&channel_id, &ts, "tada")
        .await
        .expect("off again");
    assert!(
        slack
            .store()
            .message(&channel, latest)
            .expect("the message")
            .1
            .reactions
            .iter()
            .all(|reaction| reaction.name != "tada"),
        "and taking it off twice leaves it off"
    );
}

#[tokio::test]
async fn time_stops_when_the_server_is_dropped() {
    let slack = FakeSlack::start(small()).await.expect("server");
    slack.live(2_000.0);
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let moved = slack.store().message_count();
    assert!(
        moved > small().messages,
        "a rate makes things happen without anyone asking: {moved}"
    );
    drop(slack);
    // Nothing to assert but the absence of a task: if the ticker outlived
    // the server it would be writing to a store nobody holds, which the
    // runtime would notice at the end of the test.
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
}

#[tokio::test]
async fn a_thread_reply_moves_the_thread_and_not_the_channel() {
    let slack = FakeSlack::start(small()).await.expect("server");
    let client = connected(&slack);
    let (channel, parent) = {
        let store = slack.store();
        let (channel, parent) = store.followed().first().expect("a followed thread").clone();
        (channel, parent)
    };
    let before = slack
        .store()
        .history(&channel, None, None, false, 1_000)
        .expect("history")
        .messages
        .len();
    let ts = client
        .post_message(
            &rho_slack::types::ChannelId(channel.0.clone()),
            Some(&rho_slack::types::Ts(parent.to_string())),
            "in the thread",
        )
        .await
        .expect("reply");
    let store = slack.store();
    let after = store
        .history(&channel, None, None, false, 1_000)
        .expect("history")
        .messages
        .len();
    assert_eq!(
        before, after,
        "a reply is in its thread, not in the conversation's history"
    );
    let (root, replies) = store.replies(&channel, parent).expect("the thread");
    assert_eq!(root.ts, parent, "the parent comes first");
    assert_eq!(
        replies.last().map(|reply| reply.ts.to_string()),
        Some(ts.0),
        "and the reply is at the end of it"
    );
    assert_eq!(
        root.latest_reply.map(|ts| ts.to_string()),
        replies.last().map(|reply| reply.ts.to_string()),
        "the parent's counters moved with it"
    );
}

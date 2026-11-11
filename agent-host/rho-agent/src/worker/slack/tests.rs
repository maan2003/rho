use rho_agent_types::AgentIdDomain;
use serde_json::json;

use super::*;

const BOT: &str = "UBOT";

fn agent(n: u64) -> AgentId {
    AgentId::from_counter(n, &AgentIdDomain(0)).unwrap()
}

fn message(channel: &str, ts: &str, thread_ts: Option<&str>, text: &str) -> Value {
    let mut event = json!({
        "type": "message",
        "channel": channel,
        "channel_type": "channel",
        "user": "UPERSON",
        "ts": ts,
        "text": text,
    });
    if let Some(thread_ts) = thread_ts {
        event["thread_ts"] = json!(thread_ts);
    }
    event
}

async fn routes() -> Routes {
    Routes::with(RhoDb::in_memory()).await
}

#[tokio::test]
async fn a_workset_without_an_owner_routes_nothing() {
    let routes = routes().await;
    let mention = message("C1", "1.0", None, "<@UBOT> hi");
    assert_eq!(routes.route(&mention, BOT).await, None);
    assert_eq!(routes.all().threads, vec![]);
}

#[tokio::test]
async fn a_mention_takes_its_thread_so_the_replies_before_an_answer_arrive() {
    let routes = routes().await;
    routes.set_owner(agent(1)).await;
    // Replies in a thread nobody was asked in are not heard.
    let reply = message("C1", "1.5", Some("1.0"), "what do you think");
    assert_eq!(routes.route(&reply, BOT).await, None);
    let mention = message("C1", "1.6", Some("1.0"), "<@UBOT> look at this");
    assert_eq!(routes.route(&mention, BOT).await, Some(agent(1)));
    let follow_up = message("C1", "1.7", Some("1.0"), "also this");
    assert_eq!(routes.route(&follow_up, BOT).await, Some(agent(1)));
    // Another thread of the channel is still not heard.
    let elsewhere = message("C1", "2.1", Some("2.0"), "unrelated");
    assert_eq!(routes.route(&elsewhere, BOT).await, None);
}

#[tokio::test]
async fn a_top_level_mention_takes_the_thread_its_replies_start() {
    let routes = routes().await;
    routes.set_owner(agent(1)).await;
    let mention = message("C1", "3.0", None, "hey <@UBOT>");
    assert_eq!(routes.route(&mention, BOT).await, Some(agent(1)));
    let reply = message("C1", "3.1", Some("3.0"), "more");
    assert_eq!(routes.route(&reply, BOT).await, Some(agent(1)));
}

#[tokio::test]
async fn direct_messages_edits_other_mentions_and_the_bots_own_posts_are_not_heard() {
    let routes = routes().await;
    routes.set_owner(agent(1)).await;
    let mut direct = message("D1", "1.0", None, "<@UBOT> hi");
    direct["channel_type"] = json!("im");
    assert_eq!(routes.route(&direct, BOT).await, None);
    let mut group = message("G1", "1.0", None, "<@UBOT> hi");
    group["channel_type"] = json!("mpim");
    assert_eq!(routes.route(&group, BOT).await, None);
    let mut edit = message("C1", "1.0", None, "<@UBOT> hi");
    edit["subtype"] = json!("message_changed");
    assert_eq!(routes.route(&edit, BOT).await, None);
    let other = message("C1", "1.0", None, "<@UBOTTOM> hi <@U2>");
    assert_eq!(routes.route(&other, BOT).await, None);
    routes.claim("C1", "4.0", agent(1)).await;
    let mut own = message("C1", "4.1", Some("4.0"), "my answer");
    own["user"] = json!(BOT);
    assert_eq!(routes.route(&own, BOT).await, None);
    let mut broadcast = message("C1", "4.2", Some("4.0"), "also sent to the channel");
    broadcast["subtype"] = json!("thread_broadcast");
    assert_eq!(routes.route(&broadcast, BOT).await, Some(agent(1)));
}

#[tokio::test]
async fn a_thread_rule_beats_the_channel_rule_which_beats_the_owner() {
    let routes = routes().await;
    routes.set_owner(agent(1)).await;
    // Taken by the owner before the channel was given away: it stays.
    let early = message("C1", "1.0", None, "<@UBOT> early");
    assert_eq!(routes.route(&early, BOT).await, Some(agent(1)));
    routes.delegate("C1", None, agent(2)).await;
    let reply = message("C1", "1.1", Some("1.0"), "reply");
    assert_eq!(routes.route(&reply, BOT).await, Some(agent(1)));
    // A new mention in the channel goes to its agent, and so do its replies.
    let late = message("C1", "2.0", None, "<@UBOT> late");
    assert_eq!(routes.route(&late, BOT).await, Some(agent(2)));
    assert_eq!(
        routes
            .route(&message("C1", "2.1", Some("2.0"), "reply"), BOT)
            .await,
        Some(agent(2))
    );
    // A thread given to a third agent goes there, whoever had it.
    routes.delegate("C1", Some("2.0"), agent(3)).await;
    assert_eq!(
        routes
            .route(&message("C1", "2.2", Some("2.0"), "reply"), BOT)
            .await,
        Some(agent(3))
    );
    // Other channels still go to the owner.
    let other = message("C2", "5.0", None, "<@UBOT>");
    assert_eq!(routes.route(&other, BOT).await, Some(agent(1)));
}

#[tokio::test]
async fn a_mode_widens_what_a_channel_delivers_without_taking_threads() {
    let routes = routes().await;
    routes.set_owner(agent(1)).await;
    routes.set_mode("C1", Mode::TopLevel).await;
    let top = message("C1", "1.0", None, "a new question");
    assert_eq!(routes.route(&top, BOT).await, Some(agent(1)));
    let reply = message("C1", "1.1", Some("1.0"), "a reply");
    assert_eq!(routes.route(&reply, BOT).await, None);
    routes.set_mode("C1", Mode::All).await;
    routes.delegate("C1", None, agent(2)).await;
    assert_eq!(routes.route(&reply, BOT).await, Some(agent(2)));
    assert_eq!(routes.all().threads, vec![]);
    // Giving the channel away kept its mode, and the mode its owner.
    assert_eq!(
        routes.all().channels,
        vec![(
            "C1".to_owned(),
            Channel {
                owner: Some(agent(2)),
                mode: Mode::All
            }
        )]
    );
}

#[tokio::test]
async fn a_claim_does_not_take_a_thread_from_its_owner() {
    let routes = routes().await;
    assert_eq!(routes.claim("C1", "1.0", agent(1)).await, agent(1));
    assert_eq!(routes.claim("C1", "1.0", agent(2)).await, agent(1));
    routes.delegate("C1", Some("1.0"), agent(2)).await;
    assert_eq!(routes.claim("C1", "1.0", agent(1)).await, agent(2));
}

#[tokio::test]
async fn routes_survive_reopening() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("slack.redb");
    {
        let routes = Routes::open(&path).await;
        routes.set_owner(agent(1)).await;
        routes.claim("C1", "1.0", agent(2)).await;
        routes.set_mode("C2", Mode::TopLevel).await;
    }
    let routes = Routes::open(&path).await;
    assert_eq!(routes.owner(), Some(agent(1)));
    let reply = message("C1", "1.1", Some("1.0"), "after the restart");
    assert_eq!(routes.route(&reply, BOT).await, Some(agent(2)));
    let top = message("C2", "2.0", None, "question");
    assert_eq!(routes.route(&top, BOT).await, Some(agent(1)));
}

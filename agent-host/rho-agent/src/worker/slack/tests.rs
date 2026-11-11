use rho_agent_types::AgentIdDomain;
use serde_json::json;

use super::*;

const BOT: Bot = Bot {
    user: "UBOT",
    id: "BBOT",
};

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

async fn db() -> RhoDb {
    let db = RhoDb::in_memory();
    Routes::create_tables(&db).await;
    db
}

fn channel(id: &str) -> Wide {
    Wide::Channel(id.to_owned())
}

#[tokio::test]
async fn a_workset_without_routes_hears_nothing() {
    let db = db().await;
    let routes = Routes::of(&db);
    let mention = message("C1", "1.0", None, "<@UBOT> hi");
    assert_eq!(routes.who_hears(&mention, BOT).await, None);
    assert_eq!(routes.all().threads, vec![]);
    assert_eq!(routes.owner(Some("C1"), Some("1.0")), None);
}

#[tokio::test]
async fn a_mention_routes_its_thread_so_the_replies_before_an_answer_arrive() {
    let db = db().await;
    let routes = Routes::of(&db);
    routes
        .route_wide(Wide::Workspace, agent(1), Mode::Mentions)
        .await;
    // Replies in a thread nobody was asked in are not heard.
    let reply = message("C1", "1.5", Some("1.0"), "what do you think");
    assert_eq!(routes.who_hears(&reply, BOT).await, None);
    let mention = message("C1", "1.6", Some("1.0"), "<@UBOT> look at this");
    assert_eq!(routes.who_hears(&mention, BOT).await, Some(agent(1)));
    let follow_up = message("C1", "1.7", Some("1.0"), "also this");
    assert_eq!(routes.who_hears(&follow_up, BOT).await, Some(agent(1)));
    // Another thread of the channel is still not heard.
    let elsewhere = message("C1", "2.1", Some("2.0"), "unrelated");
    assert_eq!(routes.who_hears(&elsewhere, BOT).await, None);
}

#[tokio::test]
async fn a_top_level_mention_routes_the_thread_its_replies_start() {
    let db = db().await;
    let routes = Routes::of(&db);
    routes
        .route_wide(Wide::Workspace, agent(1), Mode::Mentions)
        .await;
    let mention = message("C1", "3.0", None, "hey <@UBOT>");
    assert_eq!(routes.who_hears(&mention, BOT).await, Some(agent(1)));
    let reply = message("C1", "3.1", Some("3.0"), "more");
    assert_eq!(routes.who_hears(&reply, BOT).await, Some(agent(1)));
}

#[tokio::test]
async fn direct_messages_edits_other_mentions_and_the_bots_own_posts_are_not_heard() {
    let db = db().await;
    let routes = Routes::of(&db);
    routes
        .route_wide(Wide::Workspace, agent(1), Mode::All)
        .await;
    let mut direct = message("D1", "1.0", None, "<@UBOT> hi");
    direct["channel_type"] = json!("im");
    assert_eq!(routes.who_hears(&direct, BOT).await, None);
    let mut group = message("G1", "1.0", None, "<@UBOT> hi");
    group["channel_type"] = json!("mpim");
    assert_eq!(routes.who_hears(&group, BOT).await, None);
    let mut edit = message("C1", "1.0", None, "<@UBOT> hi");
    edit["subtype"] = json!("message_changed");
    assert_eq!(routes.who_hears(&edit, BOT).await, None);
    routes
        .route_wide(Wide::Workspace, agent(1), Mode::Mentions)
        .await;
    let other = message("C1", "1.0", None, "<@UBOTTOM> hi <@U2>");
    assert_eq!(routes.who_hears(&other, BOT).await, None);
    routes.route_thread("C1", "4.0", agent(2)).await;
    let mut own = message("C1", "4.1", Some("4.0"), "my answer");
    own["user"] = json!(BOT.user);
    own["bot_id"] = json!(BOT.id);
    assert_eq!(routes.who_hears(&own, BOT).await, None);
    let mut own_by_bot_id = message("C1", "4.1", Some("4.0"), "my answer");
    own_by_bot_id.as_object_mut().unwrap().remove("user");
    own_by_bot_id["bot_id"] = json!(BOT.id);
    assert_eq!(routes.who_hears(&own_by_bot_id, BOT).await, None);
    let mut other_app = message("C1", "4.3", Some("4.0"), "CI failed");
    other_app["user"] = json!("UCIBOT");
    other_app["bot_id"] = json!("BCI");
    assert_eq!(routes.who_hears(&other_app, BOT).await, Some(agent(2)));
    let mut broadcast = message("C1", "4.2", Some("4.0"), "also sent to the channel");
    broadcast["subtype"] = json!("thread_broadcast");
    assert_eq!(routes.who_hears(&broadcast, BOT).await, Some(agent(2)));
}

#[tokio::test]
async fn the_most_specific_route_wins() {
    let db = db().await;
    let routes = Routes::of(&db);
    routes
        .route_wide(Wide::Workspace, agent(1), Mode::Mentions)
        .await;
    // Routed to the workspace's agent before the channel got its own: it stays.
    let early = message("C1", "1.0", None, "<@UBOT> early");
    assert_eq!(routes.who_hears(&early, BOT).await, Some(agent(1)));
    routes
        .route_wide(channel("C1"), agent(2), Mode::Mentions)
        .await;
    let reply = message("C1", "1.1", Some("1.0"), "reply");
    assert_eq!(routes.who_hears(&reply, BOT).await, Some(agent(1)));
    // A new mention in the channel goes to its agent, and so do its replies.
    let late = message("C1", "2.0", None, "<@UBOT> late");
    assert_eq!(routes.who_hears(&late, BOT).await, Some(agent(2)));
    let late_reply = message("C1", "2.1", Some("2.0"), "reply");
    assert_eq!(routes.who_hears(&late_reply, BOT).await, Some(agent(2)));
    // A thread routed to a third agent goes there, whoever had it.
    routes.route_thread("C1", "2.0", agent(3)).await;
    let third = message("C1", "2.2", Some("2.0"), "reply");
    assert_eq!(routes.who_hears(&third, BOT).await, Some(agent(3)));
    // Other channels still go to the workspace's agent.
    let other = message("C2", "5.0", None, "<@UBOT>");
    assert_eq!(routes.who_hears(&other, BOT).await, Some(agent(1)));
    assert_eq!(routes.owner(Some("C1"), Some("2.0")), Some(agent(3)));
    assert_eq!(routes.owner(Some("C1"), Some("9.0")), Some(agent(2)));
    assert_eq!(routes.owner(Some("C9"), None), Some(agent(1)));
    assert_eq!(routes.owner(None, None), Some(agent(1)));
}

#[tokio::test]
async fn a_channel_mode_overrides_the_workspace_mode_without_routing_threads() {
    let db = db().await;
    let routes = Routes::of(&db);
    routes
        .route_wide(Wide::Workspace, agent(1), Mode::All)
        .await;
    routes
        .route_wide(channel("C1"), agent(2), Mode::TopLevel)
        .await;
    let top = message("C1", "1.0", None, "a new question");
    assert_eq!(routes.who_hears(&top, BOT).await, Some(agent(2)));
    let reply = message("C1", "1.1", Some("1.0"), "a reply");
    assert_eq!(routes.who_hears(&reply, BOT).await, None);
    let elsewhere = message("C2", "1.1", Some("1.0"), "a reply");
    assert_eq!(routes.who_hears(&elsewhere, BOT).await, Some(agent(1)));
    assert_eq!(routes.all().threads, vec![]);
    // Without its channel route, the channel follows the workspace again.
    routes.unroute_wide(channel("C1")).await;
    assert_eq!(routes.who_hears(&reply, BOT).await, Some(agent(1)));
}

#[tokio::test]
async fn a_claim_does_not_take_a_routed_thread() {
    let db = db().await;
    let routes = Routes::of(&db);
    assert_eq!(routes.claim("C1", "1.0", agent(1)).await, agent(1));
    assert_eq!(routes.claim("C1", "1.0", agent(2)).await, agent(1));
    routes.route_thread("C1", "1.0", agent(2)).await;
    assert_eq!(routes.claim("C1", "1.0", agent(1)).await, agent(2));
    routes.unroute_thread("C1", "1.0").await;
    assert_eq!(routes.owner(Some("C1"), Some("1.0")), None);
}

#[tokio::test]
async fn routes_survive_reopening() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("workset.redb");
    {
        let db = RhoDb::open(&path);
        Routes::create_tables(&db).await;
        let routes = Routes::of(&db);
        routes
            .route_wide(Wide::Workspace, agent(1), Mode::Mentions)
            .await;
        routes.claim("C1", "1.0", agent(2)).await;
        routes
            .route_wide(channel("C2"), agent(1), Mode::TopLevel)
            .await;
    }
    let db = RhoDb::open(&path);
    Routes::create_tables(&db).await;
    let routes = Routes::of(&db);
    let reply = message("C1", "1.1", Some("1.0"), "after the restart");
    assert_eq!(routes.who_hears(&reply, BOT).await, Some(agent(2)));
    let top = message("C2", "2.0", None, "question");
    assert_eq!(routes.who_hears(&top, BOT).await, Some(agent(1)));
}

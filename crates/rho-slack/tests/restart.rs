//! What rho knows about Slack across a restart: a window quit and started
//! again over the same state deals the cards Slack still has unread, and a
//! card's age is the age of the message that asks, not of rho having seen it.

use std::sync::Arc;
use std::time::Duration;

use gpui::{AppContext as _, Entity, TestAppContext};
use rho_dealer::Marks;
use rho_dealer::curve::Curve;
use rho_frame::attention::Verdict;
use rho_slack::cards;
use rho_slack::fake::Fake;
use rho_slack::model::{Attention, Unit};
use rho_slack::session::Session;
use rho_slack::types::ChannelId;
use rho_slack::ui::system::{say, unsay};

fn now_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn ts(seconds: i64) -> String {
    format!("{seconds}.000000")
}

async fn start_fake(cx: &mut TestAppContext) -> Fake {
    cx.update(gpui_tokio::init);
    cx.executor().allow_parking();
    cx.update(|cx| gpui_tokio::Tokio::spawn(cx, async { Fake::start().await }))
        .await
        .unwrap()
        .unwrap()
}

/// The window's session over the fake, keeping its tables in the one
/// client database, as the GUI does.
fn session(fake: &Fake, state: &std::path::Path, cx: &mut TestAppContext) -> Entity<Session> {
    let credentials = rho_slack::config::Credentials::parse("acme", "xoxc-test", "cookie").unwrap();
    let client = Arc::new(rho_slack::api::Client::with_base(credentials, fake.api_base()).unwrap());
    let paths = rho_slack::config::Paths::under(state);
    cx.update(|cx| cx.new(|cx| Session::with_client(client, paths, cx)))
}

fn asking(session: &Entity<Session>, cx: &mut TestAppContext) -> Vec<(Unit, Attention)> {
    session.read_with(cx, |session, _| {
        session
            .model()
            .asking()
            .map(|(unit, attention)| (unit.clone(), attention))
            .collect()
    })
}

/// Runs the app until `done` holds, or for three seconds.
async fn wait_until(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
    for _ in 0..300 {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
    }
}

/// A mention in `#random` two days back, with chatter after it, and a group
/// DM read to yesterday with a new message this morning; both unread in
/// Slack. The first run deals both, and the user says done on each and
/// takes it back, which tells Slack both times. Quitting and starting again
/// over the same files deals both again, because Slack still has them
/// unread; and each card is as old as its ask.
#[gpui::test]
async fn a_restart_deals_what_slack_still_has_unread(cx: &mut TestAppContext) {
    let fake = start_fake(cx).await;
    let now = now_s();
    let ping = now - 57 * 3600;
    let group_ask = now - 2 * 3600;
    fake.add_user("UA", "ada");
    fake.add_user("UD", "david");
    fake.add_channel("C2", "random");
    fake.add_group("G1", "mpdm-david--manmeet-1", &["ME", "UD"]);
    fake.add_message(
        "C2",
        serde_json::json!({"ts": ts(ping - 600), "user": "UA", "text": "earlier"}),
    );
    fake.add_message(
        "C2",
        serde_json::json!({"ts": ts(ping), "user": "UA", "text": "<@ME> release notes?"}),
    );
    fake.add_message(
        "C2",
        serde_json::json!({"ts": ts(now - 3600), "user": "UA", "text": "later chatter"}),
    );
    fake.set_count("C2", true, 1, &ts(ping));
    fake.add_feed_mention("C2", &ts(ping));
    fake.add_message(
        "G1",
        serde_json::json!({"ts": ts(now - 30 * 3600), "user": "UD", "text": "yesterday"}),
    );
    fake.add_message(
        "G1",
        serde_json::json!({"ts": ts(group_ask), "user": "UD", "text": "can you look?"}),
    );
    fake.set_last_read("G1", &ts(now - 30 * 3600));
    fake.set_count("G1", true, 0, &ts(group_ask));

    let random = Unit::conversation(&ChannelId("C2".into()));
    let group = Unit::conversation(&ChannelId("G1".into()));
    let expected = [
        (random.clone(), Attention::Mentioned),
        (group.clone(), Attention::DirectMessage),
    ];
    let state = tempfile::tempdir().unwrap();
    // The window's one database, as the GUI opens it: both runs read and
    // write the same tables, the way a quit and a start do.
    rho_db::client::open_shared(state.path()).unwrap();

    let first = session(&fake, state.path(), cx);
    wait_until(cx, |cx| {
        let dealt = asking(&first, cx);
        expected.iter().all(|want| dealt.contains(want))
    })
    .await;
    let dealt = asking(&first, cx);
    assert!(
        expected.iter().all(|want| dealt.contains(want)),
        "the first run deals both: {dealt:?}"
    );

    // Done, then undo, on each: Slack is told both times.
    for unit in [&random, &group] {
        let newest = first.read_with(cx, |session, _| {
            session.model().unit(unit).unwrap().newest.0.clone()
        });
        let said = first
            .update(cx, |session, cx| say(session, unit, &Verdict::Done, cx))
            .expect("done says something in Slack");
        let channel = unit.channel.as_str();
        wait_until(cx, |_| fake.last_read(channel) == Some(newest.clone())).await;
        assert_eq!(
            fake.last_read(channel),
            Some(newest),
            "done marks {channel} read in Slack"
        );
        first.update(cx, |session, cx| unsay(session, said, cx));
    }
    wait_until(cx, |_| {
        fake.unread("C2").1 == 1 && fake.last_read("G1") == Some(ts(now - 30 * 3600))
    })
    .await;
    // What Slack says now, which is what every other Slack app reads.
    assert_eq!(
        fake.unread("C2").1,
        1,
        "the mention in #random is unread in Slack again"
    );
    assert_eq!(
        fake.last_read("G1"),
        Some(ts(now - 30 * 3600)),
        "the group is read to yesterday in Slack again"
    );
    let dealt = asking(&first, cx);
    assert!(
        expected.iter().all(|want| dealt.contains(want)),
        "and both cards are back: {dealt:?}"
    );

    // Quit, and start again.
    drop(first);
    cx.run_until_parked();
    let second = session(&fake, state.path(), cx);
    wait_until(cx, |cx| {
        let dealt = asking(&second, cx);
        expected.iter().all(|want| dealt.contains(want))
    })
    .await;
    let dealt = asking(&second, cx);
    assert!(
        expected.iter().all(|want| dealt.contains(want)),
        "a restart deals both again: {dealt:?}"
    );

    // Each card is as old as the message that asks.
    let claims = second.read_with(cx, |session, _| {
        cards::claims(session.model(), session.mirror(), &Marks::default(), None)
    });
    for (unit, said) in [(&random, ping), (&group, group_ask)] {
        let claim = claims
            .iter()
            .find(|claim| {
                second.read_with(cx, |session, _| cards::address(session.model(), unit))
                    == claim.address
            })
            .expect("a claim for each card");
        match claim.curve {
            Curve::Waiting { since, .. } => assert_eq!(
                since.as_second(),
                said,
                "{unit:?} counts from the message that asks"
            ),
            ref other => panic!("a waiting curve: {other:?}"),
        }
    }
}

//! What a verdict on a Slack card does in Slack: the side the frame cannot
//! write in its ledger, and the undo that takes it back. A real session
//! over the fake server, so what is asserted is what Slack now holds.
#![cfg(feature = "ui")]

use std::sync::Arc;
use std::time::Duration;

use gpui::{AppContext as _, Entity, TestAppContext};
use rho_dealer::Until;
use rho_frame::attention::Verdict;
use rho_slack::fake::Fake;
use rho_slack::model::{Attention, Unit};
use rho_slack::session::Session;
use rho_slack::types::{ChannelId, Ts};
use rho_slack::ui::system::{say, unsay};

struct Rig {
    fake: Fake,
    session: Entity<Session>,
    _state: tempfile::TempDir,
}

const ROOT: &str = "1800000000.000000";

/// A session over the fake with a direct message from ada and a thread the
/// reader follows in `#ops`, both waiting.
async fn rig(cx: &mut TestAppContext) -> Rig {
    cx.update(gpui_tokio::init);
    cx.executor().allow_parking();
    let fake = cx
        .update(|cx| gpui_tokio::Tokio::spawn(cx, async { Fake::start().await }))
        .await
        .unwrap()
        .unwrap();
    fake.add_user("UA", "ada");
    fake.add_channel("C3", "ops");
    fake.add_dm("D1", "UA");
    fake.add_message(
        "C3",
        serde_json::json!({"ts": ROOT, "user": "UA", "text": "the deploy is stuck"}),
    );
    fake.follow_thread("C3", ROOT);
    let credentials = rho_slack::config::Credentials::parse("acme", "xoxc-test", "cookie").unwrap();
    let client = Arc::new(rho_slack::api::Client::with_base(credentials, fake.api_base()).unwrap());
    let state = tempfile::tempdir().expect("a state directory of this test's own");
    let paths = rho_slack::config::Paths::under(state.path());
    let session = cx.update(|cx| cx.new(|cx| Session::with_client(client, paths, cx)));
    // Nothing can be pushed at a conversation the session has not heard of.
    until(cx, "the listing", |cx| {
        session.read_with(cx, |session, _| !session.rows().is_empty())
    })
    .await;
    fake.push_frame(serde_json::json!({"type": "message", "channel": "D1",
        "ts": "1800000100.000000", "user": "UA", "text": "are you around?"}));
    fake.push_frame(serde_json::json!({"type": "message", "channel": "C3",
        "ts": "1800000200.000000", "thread_ts": ROOT, "user": "UA", "text": "still stuck"}));
    let rig = Rig {
        fake,
        session,
        _state: state,
    };
    until(cx, "both units asking", |cx| {
        rig.attention(cx, &direct()).is_some() && rig.attention(cx, &thread()).is_some()
    })
    .await;
    rig
}

impl Rig {
    fn attention(&self, cx: &mut TestAppContext, unit: &Unit) -> Option<Attention> {
        self.session
            .read_with(cx, |session, _| session.model().attention(unit))
    }

    fn say(
        &self,
        cx: &mut TestAppContext,
        unit: &Unit,
        verdict: Verdict,
    ) -> Option<rho_slack::ui::system::Said> {
        self.session
            .update(cx, |session, cx| say(session, unit, &verdict, cx))
    }

    fn unsay(&self, cx: &mut TestAppContext, said: rho_slack::ui::system::Said) {
        self.session
            .update(cx, |session, cx| unsay(session, said, cx));
    }
}

/// Runs the app until `done` holds, or fails naming what never happened:
/// the fake is a real server, so quiet executors prove nothing.
async fn until(
    cx: &mut TestAppContext,
    what: &str,
    mut done: impl FnMut(&mut TestAppContext) -> bool,
) {
    for _ in 0..300 {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
    }
    panic!("never: {what}");
}

fn direct() -> Unit {
    Unit::conversation(&ChannelId("D1".into()))
}

fn thread() -> Unit {
    Unit::thread(&ChannelId("C3".into()), &Ts(ROOT.into()))
}

/// S3/S5: done is read in Slack too, so the phone stops asking; the undo
/// puts rho's half back and the card is the reader's again.
#[gpui::test]
async fn done_reads_it_in_slack_and_the_undo_takes_it_back(cx: &mut TestAppContext) {
    let rig = rig(cx).await;
    assert_eq!(rig.attention(cx, &direct()), Some(Attention::DirectMessage));

    let said = rig
        .say(cx, &direct(), Verdict::Done)
        .expect("done does something in Slack");
    assert_eq!(
        rig.attention(cx, &direct()),
        None,
        "closed at once, before Slack answers"
    );
    until(cx, "Slack's read mark at the newest message", |_| {
        rig.fake.last_read("D1").as_deref() == Some("1800000100.000000")
    })
    .await;
    assert!(rig.fake.muted().is_empty(), "done is not a mute");

    rig.unsay(cx, said);
    assert_eq!(rig.attention(cx, &direct()), Some(Attention::DirectMessage));
}

/// S6: a mute of a conversation is Slack's mute, so it holds in every
/// client; the undo lifts it there.
#[gpui::test]
async fn a_conversation_mute_is_made_in_slack_and_undone_there(cx: &mut TestAppContext) {
    let rig = rig(cx).await;
    let said = rig.say(cx, &direct(), Verdict::Mute).unwrap();
    assert_eq!(rig.attention(cx, &direct()), None);
    until(cx, "Slack holding the mute", |_| rig.fake.muted() == ["D1"]).await;

    rig.unsay(cx, said);
    until(cx, "Slack lifting the mute", |_| {
        rig.fake.muted().is_empty()
    })
    .await;
    assert_eq!(rig.attention(cx, &direct()), Some(Attention::DirectMessage));
}

/// A mute of a thread is an unfollow in Slack, and its undo a follow.
#[gpui::test]
async fn a_thread_mute_unfollows_it_in_slack(cx: &mut TestAppContext) {
    let rig = rig(cx).await;
    let said = rig.say(cx, &thread(), Verdict::Mute).unwrap();
    assert_eq!(rig.attention(cx, &thread()), None);
    until(cx, "the unfollow reaching Slack", |_| {
        rig.fake.calls("subscriptions.thread.remove") == 1
    })
    .await;
    assert!(
        rig.fake.muted().is_empty(),
        "the channel is not muted for a thread"
    );

    rig.unsay(cx, said);
    until(cx, "the follow reaching Slack", |_| {
        rig.fake.calls("subscriptions.thread.add") == 1
    })
    .await;
    assert!(
        rig.attention(cx, &thread()).is_some(),
        "the thread is the reader's again"
    );
}

/// A snooze is rho's alone: Slack still says unread, so the unit is still
/// asking once the snooze runs out.
#[gpui::test]
async fn a_snooze_leaves_slack_alone(cx: &mut TestAppContext) {
    let rig = rig(cx).await;
    let tomorrow = Until::In(jiff::SignedDuration::from_hours(24));
    assert!(rig.say(cx, &direct(), Verdict::Snooze(tomorrow)).is_none());
    cx.run_until_parked();
    assert_eq!(rig.attention(cx, &direct()), Some(Attention::DirectMessage));
    assert_eq!(
        rig.fake.calls("conversations.mark"),
        0,
        "nothing told Slack"
    );
}

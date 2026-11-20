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
    rig_with(cx, |_| {}).await
}

/// [`rig`], with more of the workspace seeded before the session starts.
async fn rig_with(cx: &mut TestAppContext, seed: impl FnOnce(&Fake)) -> Rig {
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
    // Slack keeps a count for every conversation, read or not; the
    // fake's recount after a mark moves it only when one is there.
    fake.set_count("D1", false, 0, "1800000100.000000");
    fake.add_message(
        "C3",
        serde_json::json!({"ts": ROOT, "user": "UA", "text": "the deploy is stuck"}),
    );
    fake.follow_thread("C3", ROOT);
    seed(&fake);
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

/// done in rho is read in Slack, reply or not: the reader's own reply
/// already says they are through on rho's side, and Slack still shows
/// the conversation unread until done marks it there.
#[gpui::test]
async fn done_after_a_reply_still_reads_it_in_slack(cx: &mut TestAppContext) {
    let rig = rig(cx).await;
    // The reader's own reply, as Slack hands it to every client they are
    // signed in on, rho's own sends included.
    rig.fake
        .push_frame(serde_json::json!({"type": "message", "channel": "D1",
        "ts": "1800000300.000000", "user": "ME", "text": "yes, looks good"}));
    until(cx, "the reply in rho", |cx| {
        rig.session.read_with(cx, |session, _| {
            session
                .model()
                .unit(&direct())
                .is_some_and(|facts| facts.newest_from_you)
        })
    })
    .await;
    let reply = "1800000300.000000".to_owned();
    assert_ne!(
        rig.fake.last_read("D1").as_deref(),
        Some(reply.as_str()),
        "posting alone does not read it in Slack"
    );

    let said = rig.say(cx, &direct(), Verdict::Done);
    assert!(said.is_some(), "done after a reply still has a Slack side");
    until(cx, "Slack's read mark at the reply", |_| {
        rig.fake.last_read("D1").as_deref() == Some(reply.as_str())
    })
    .await;
    assert!(!rig.fake.unread("D1").0, "Slack badges nothing");
}

/// The undo of a mute takes back the read mark the mute made, not only
/// the mute: a conversation Slack held no mark for goes back to unread.
#[gpui::test]
async fn the_undo_of_a_mute_puts_slacks_read_mark_back(cx: &mut TestAppContext) {
    let rig = rig(cx).await;
    assert_eq!(rig.fake.last_read("D1"), None, "never read in Slack");
    let said = rig.say(cx, &direct(), Verdict::Mute).unwrap();
    until(cx, "the mute and the mark in Slack", |_| {
        rig.fake.muted() == ["D1"]
            && rig.fake.last_read("D1").as_deref() == Some("1800000100.000000")
    })
    .await;

    rig.unsay(cx, said);
    until(cx, "Slack unread again, and unmuted", |_| {
        rig.fake.muted().is_empty()
            && rig
                .fake
                .last_read("D1")
                .is_some_and(|read| read.as_str() < "1800000100.000000")
    })
    .await;
    assert_eq!(rig.attention(cx, &direct()), Some(Attention::DirectMessage));
}

/// The echo of rho's own mark, arriving after the undo that took the mark
/// back, is old news: it does not move rho's idea of Slack's mark, so the
/// next mute still marks read and its undo still puts the mark back.
#[gpui::test]
async fn a_late_echo_of_an_undone_mark_does_not_stick(cx: &mut TestAppContext) {
    let rig = rig(cx).await;
    let said = rig.say(cx, &direct(), Verdict::Mute).unwrap();
    rig.unsay(cx, said);
    until(cx, "Slack unread again, and unmuted", |_| {
        rig.fake.muted().is_empty()
            && rig.fake.last_read("D1").as_deref() == Some("1800000099.999999")
    })
    .await;
    cx.run_until_parked();

    let said = rig.say(cx, &direct(), Verdict::Mute).unwrap();
    until(cx, "the second mute marks read in Slack", |_| {
        rig.fake.muted() == ["D1"]
            && rig.fake.last_read("D1").as_deref() == Some("1800000100.000000")
    })
    .await;
    rig.unsay(cx, said);
    until(cx, "and its undo puts the mark back", |_| {
        rig.fake.muted().is_empty()
            && rig.fake.last_read("D1").as_deref() == Some("1800000099.999999")
    })
    .await;
}

/// The undo of a done on a followed thread takes back the thread's own
/// read mark in Slack.
#[gpui::test]
async fn the_undo_of_a_thread_done_puts_its_read_mark_back(cx: &mut TestAppContext) {
    let rig = rig(cx).await;
    let said = rig.say(cx, &thread(), Verdict::Done).unwrap();
    until(cx, "the thread read in Slack", |_| {
        rig.fake.thread_last_read("C3", ROOT).as_deref() == Some("1800000200.000000")
    })
    .await;

    rig.unsay(cx, said);
    until(cx, "the thread unread in Slack again", |_| {
        rig.fake
            .thread_last_read("C3", ROOT)
            .is_some_and(|read| read.as_str() < "1800000200.000000")
    })
    .await;
    assert!(rig.attention(cx, &thread()).is_some());
}

const BACKLOG_ROOT: &str = "1700000050.000000";

/// slack.Catch up after time away: the count the prompt shows is what is
/// marked, in rho and in Slack; a muted conversation and anything newer
/// than the cutoff are left alone; and one undo takes all of it back, in
/// Slack too.
#[gpui::test]
async fn mark_read_before_marks_what_it_counted_and_one_undo_takes_it_back(
    cx: &mut TestAppContext,
) {
    let rig = rig_with(cx, |fake| {
        fake.add_channel("C4", "random");
        for (ts, text) in [
            ("1700000000.000000", "old news"),
            (BACKLOG_ROOT, "who has the release?"),
            ("1700000100.000000", "<@ME> can you take the release notes?"),
            ("1700000200.000000", "never mind"),
        ] {
            fake.add_message(
                "C4",
                serde_json::json!({"ts": ts, "user": "UA", "text": text}),
            );
        }
        fake.set_count("C4", true, 1, "1700000200.000000");
        fake.follow_thread("C4", BACKLOG_ROOT);
        fake.add_channel("C5", "noise");
        fake.add_message(
            "C5",
            serde_json::json!({"ts": "1700000300.000000", "user": "UA", "text": "noise"}),
        );
        fake.set_count("C5", true, 0, "1700000300.000000");
        fake.mute("C5");
    })
    .await;
    let backlog_thread = Unit::thread(&ChannelId("C4".into()), &Ts(BACKLOG_ROOT.into()));
    rig.fake
        .push_frame(serde_json::json!({"type": "message", "channel": "C4",
        "ts": "1700000060.000000", "thread_ts": BACKLOG_ROOT, "user": "UA",
        "text": "anyone?"}));
    until(cx, "the old thread asking", |cx| {
        rig.attention(cx, &backlog_thread).is_some()
    })
    .await;
    let cutoff = 1_750_000_000.0;

    let plan = rig
        .session
        .read_with(cx, |session, _| session.mark_plan(cutoff));
    assert_eq!(
        plan.conversations,
        vec![(ChannelId("C4".into()), Ts("1700000200.000000".into()))],
        "#random's backlog, and neither the muted channel nor the newer DM"
    );
    assert_eq!(
        plan.threads.len(),
        1,
        "the old thread, and not the newer one"
    );
    assert_eq!(plan.threads[0].1, Ts("1700000060.000000".into()));

    let moved = rig
        .session
        .update(cx, |session, cx| session.mark_read_before(&plan, cx));
    assert_eq!(moved.len(), 2, "what was counted is what was marked");
    until(cx, "both marks in Slack", |_| {
        rig.fake.last_read("C4").as_deref() == Some("1700000200.000000")
            && rig.fake.thread_last_read("C4", BACKLOG_ROOT).as_deref() == Some("1700000060.000000")
    })
    .await;
    assert_eq!(rig.fake.unread("C4"), (false, 0, 0), "Slack badges nothing");
    assert_eq!(rig.fake.calls("conversations.mark"), 1, "one mark, no more");
    assert_eq!(rig.fake.calls("subscriptions.thread.mark"), 1);
    assert_eq!(rig.fake.last_read("C5"), None, "the muted channel is left");
    assert_eq!(rig.fake.last_read("D1"), None, "the newer DM is left");
    assert_eq!(rig.attention(cx, &direct()), Some(Attention::DirectMessage));
    rig.session.update(cx, |session, cx| {
        for (unit, _, before) in &moved {
            session.undo_handled(unit, before, cx);
        }
    });
    until(cx, "Slack's marks back where they stood", |_| {
        // #random holds no ask rho has seen, so Slack's never-read mark;
        // the thread goes back to just before its reply.
        rig.fake.last_read("C4").as_deref() == Some("0000000000.000000")
            && rig.fake.thread_last_read("C4", BACKLOG_ROOT).as_deref() == Some("1700000059.999999")
    })
    .await;
    assert_eq!(rig.fake.unread("C4").1, 1, "the mention is Slack's again");
    assert!(rig.attention(cx, &backlog_thread).is_some());

    // Slack's word that it marked them comes back over the socket after
    // the undo, and the badge comes back with the counts the undo asks
    // for, over the network rather than on this executor; neither may stop
    // the same catch-up from working again.
    until(cx, "the same catch-up planned again", |cx| {
        let again = rig
            .session
            .read_with(cx, |session, _| session.mark_plan(cutoff));
        again.conversations == plan.conversations && again.threads == plan.threads
    })
    .await;
}

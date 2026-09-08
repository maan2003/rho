//! A `Session` stood up the way it stands up in production: a real client
//! against the fake Slack server, a mirror in this test's own directory, and
//! nothing stubbed in between.
//!
//! The transport tests hold a bare `Client` and the model tests hold a bare
//! `Model`. The rules that need both — what a refused write tells the reader,
//! what goes on the wire when they send — had nowhere to be tested, and three
//! changes went out saying so. This is that place.

use std::sync::Arc;

use gpui::{AppContext as _, Entity, TestAppContext};
use rho_slack::fake::Fake;
use rho_slack::session::{Session, SessionEvent, Source};
use rho_slack::types::{ChannelId, Ts};

/// Everything a session test needs, kept together so the tempdir outlives
/// the session that writes into it.
struct Rig {
    fake: Fake,
    /// A second client on the same server, for asking Slack what it now
    /// holds. `chat.update` is posted as JSON rather than a form, so the
    /// fake's recorded fields cannot answer for it — and what Slack holds is
    /// the better question anyway, since that is what everyone else reads.
    client: Arc<rho_slack::api::Client>,
    session: Entity<Session>,
    notices: Entity<Vec<String>>,
    _state: tempfile::TempDir,
}

impl Rig {
    /// Runs the app until it is quiet, then hands back what rho has said to
    /// the reader since the rig was built.
    fn notices(&self, cx: &mut TestAppContext) -> Vec<String> {
        cx.run_until_parked();
        self.notices.read_with(cx, |said, _| said.clone())
    }

    /// Waits until the session knows who is in the workspace.
    ///
    /// The roster arrives over a real socket from a real server, and the
    /// executor going quiet says nothing about whether that has happened —
    /// so this waits on the answer itself. A loaded machine can then only
    /// make the test slower, never make it lie.
    async fn wait_for_roster(&self, cx: &mut TestAppContext) {
        for _ in 0..200 {
            cx.run_until_parked();
            let known = self.session.read_with(cx, |session, _| {
                !session
                    .model()
                    .suggestions(&ChannelId("C1".into()), '@', "ada")
                    .is_empty()
            });
            if known {
                return;
            }
            cx.executor()
                .timer(std::time::Duration::from_millis(10))
                .await;
        }
        panic!("the fake's roster never reached the session");
    }
}

/// A session over the fake, with `ada` in `#design` and one message of the
/// reader's own to rewrite.
async fn rig(cx: &mut TestAppContext) -> Rig {
    cx.update(gpui_tokio::init);
    // The fake is a real server on a real socket, so the deterministic test
    // executor has to be allowed to wait on it, and it has to be started
    // inside the tokio runtime the session's own loops run in.
    cx.executor().allow_parking();
    let fake = cx
        .update(|cx| gpui_tokio::Tokio::spawn(cx, async { Fake::start().await }))
        .await
        .unwrap()
        .unwrap();
    fake.add_user("UA", "ada");
    fake.add_channel("C1", "design");
    fake.add_message(
        "C1",
        serde_json::json!({"ts": "500.0", "user": "ME", "text": "on it"}),
    );

    let credentials = rho_slack::config::Credentials::parse("acme", "xoxc-test", "cookie").unwrap();
    let client =
        Arc::new(rho_slack::api::Client::with_base(credentials.clone(), fake.api_base()).unwrap());
    let asking = Arc::new(rho_slack::api::Client::with_base(credentials, fake.api_base()).unwrap());
    // rho-slack never resolves the user's state directory, so a test either
    // says where its files go or has none.
    let state = tempfile::tempdir().expect("a state directory of this test's own");
    let paths = rho_slack::config::Paths::under(state.path());

    let (session, notices) = cx.update(|cx| {
        let session = cx.new(|cx| Session::with_client(client, paths, cx));
        let notices = cx.new(|_| Vec::new());
        cx.subscribe(&session, {
            let notices = notices.clone();
            move |_, event: &SessionEvent, cx| {
                if let SessionEvent::Notice(said) = event {
                    notices.update(cx, |notices, _| notices.push(said.clone()));
                }
            }
        })
        .detach();
        (session, notices)
    });

    Rig {
        fake,
        client: asking,
        session,
        notices,
        _state: state,
    }
}

fn design() -> Source {
    Source::Conversation(ChannelId("C1".into()))
}

/// A rewrite Slack refuses answers with a no, so the surface knows to put the
/// reader's words back, and says so once in the notice line. Before this the
/// answer was nothing at all and the rewrite was lost.
#[gpui::test]
async fn a_rewrite_the_server_refuses_answers_with_an_error_and_says_so(cx: &mut TestAppContext) {
    let rig = rig(cx).await;
    rig.fake.fail_next("chat.update", 1);

    let editing = rig.session.update(cx, |session, cx| {
        session.edit_message(
            &design(),
            Ts("500.0".into()),
            "on it, by tuesday".into(),
            cx,
        )
    });
    let outcome = editing.await;

    assert!(outcome.is_err(), "a refused rewrite answers with a no");
    let said = rig.notices(cx);
    assert_eq!(said.len(), 1, "said once, not once a retry: {said:?}");
    assert!(
        said[0].starts_with("slack: "),
        "and named as Slack's, not rho's: {}",
        said[0]
    );
}

/// A rewrite goes out in the form that makes the mention count. `@ada` on
/// screen is `<@UA>` on the wire, and a rewrite that skipped that was the one
/// way to type a mention in rho and have nobody told.
#[gpui::test]
async fn a_rewrite_puts_its_mention_on_the_wire_in_the_form_slack_counts(cx: &mut TestAppContext) {
    let rig = rig(cx).await;
    // The roster is what turns `@ada` into an id, so the session has to have
    // it before the rewrite goes out. It comes from the server, like
    // everything else here.
    rig.wait_for_roster(cx).await;

    rig.session
        .update(cx, |session, cx| {
            session.edit_message(
                &design(),
                Ts("500.0".into()),
                "@ada can you look?".into(),
                cx,
            )
        })
        .await
        .unwrap();

    // What Slack holds now, which is what everyone else in the channel reads.
    // Asked from inside the tokio runtime, the same as every other request
    // in this crate: reqwest needs its reactor.
    let asking = rig.client.clone();
    let held = cx
        .update(|cx| {
            gpui_tokio::Tokio::spawn(cx, async move {
                asking
                    .conversations_history(&ChannelId("C1".into()), None)
                    .await
            })
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        held.messages.last().map(|message| message.text.as_str()),
        Some("<@UA> can you look?"),
        "the handle is resolved on a rewrite, the same as on a send"
    );
}

/// A write that fails does not write to the line that says whether history
/// loaded. Nothing about a write clears that line, so an error left there
/// outlives the failure — and outlived the retry that succeeded.
#[gpui::test]
async fn a_refused_send_leaves_no_complaint_above_the_history(cx: &mut TestAppContext) {
    let rig = rig(cx).await;
    rig.session
        .update(cx, |session, cx| session.open(&design(), cx));
    cx.run_until_parked();
    rig.fake.fail_next("chat.postMessage", 1);

    let sending = rig.session.update(cx, |session, cx| {
        session.send(&design(), "did this go?".into(), cx)
    });
    assert!(sending.await.is_err(), "a refused send answers with a no");

    assert_eq!(
        rig.notices(cx).len(),
        1,
        "the reader is told, once, beside the composer"
    );
    let complaint = rig.session.read_with(cx, |session, _| {
        session
            .loaded(&design())
            .and_then(|loaded| loaded.error.clone())
    });
    assert_eq!(
        complaint, None,
        "and the line above the history is about the history"
    );
}

/// A name rho does not have is asked for, once.
///
/// The roster is fetched once per connect, so someone who joins after that
/// has no name in it. Before this, every message they sent read `someone`
/// for the rest of the run: nothing asked Slack who they were, and
/// `users.info` was a method with no caller. Now the first message from an
/// unknown author is one ask, and the answer is a fact in the model.
#[gpui::test]
async fn a_message_from_someone_the_roster_never_had_is_asked_about_once(cx: &mut TestAppContext) {
    let rig = rig(cx).await;
    rig.wait_for_roster(cx).await;
    rig.session
        .update(cx, |session, cx| session.open(&design(), cx));

    // Somebody who joined after rho asked who was in the workspace, saying
    // two things rather than one: the ask is per person, not per message.
    rig.fake.add_user("UZ", "zed");
    rig.fake.live_message("C1", "UZ", "hello, just joined");
    rig.fake.live_message("C1", "UZ", "and again");

    let mut named = None;
    for _ in 0..200 {
        cx.run_until_parked();
        named = rig.session.read_with(cx, |session, _| {
            session
                .loaded(&design())
                .and_then(|loaded| {
                    loaded
                        .messages
                        .iter()
                        .find(|message| message.text == "and again")
                        .map(|message| session.model().author(message))
                })
                .filter(|author| author != "someone")
        });
        if named.is_some() {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    assert_eq!(
        named.as_deref(),
        Some("zed"),
        "the author rho had never heard of has a name"
    );
    let asked = rig
        .fake
        .fields("users.info", "user")
        .into_iter()
        .flatten()
        .filter(|user| user == "UZ")
        .count();
    assert_eq!(
        asked, 1,
        "asked once for the person, not once for each of their messages"
    );
}

/// A search hit, or any other place named from outside, can be in a part of
/// a conversation rho has never paged. Opening there used to leave the reader
/// at the newest messages with nothing saying so; now the window comes down
/// the same road a ping's does.
#[gpui::test]
async fn a_place_the_mirror_has_never_held_is_fetched_and_landed_on(cx: &mut TestAppContext) {
    let rig = rig(cx).await;
    rig.wait_for_roster(cx).await;
    rig.fake.add_channel("C2", "ops");
    // Far more than one page, so the message asked for is nowhere near the
    // newest ones that opening loads by itself.
    for n in 0..120 {
        rig.fake.add_message(
            "C2",
            serde_json::json!({"ts": format!("{}.0", 1000 + n), "user": "UA", "text": format!("line {n}")}),
        );
    }
    let ops = Source::Conversation(ChannelId("C2".into()));
    let deep = Ts("1005.0".into());

    rig.session
        .update(cx, |session, cx| session.open_at(&ops, &deep, cx));

    let mut landed = Vec::new();
    for _ in 0..200 {
        cx.run_until_parked();
        landed = rig.session.read_with(cx, |session, _| {
            session
                .loaded(&ops)
                .map(|loaded| {
                    loaded
                        .messages
                        .iter()
                        .map(|message| message.ts.0.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        });
        if landed.contains(&deep.0) {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }

    assert!(
        landed.contains(&deep.0),
        "the message asked for has to be in front of the reader: {landed:?}"
    );
    assert!(
        landed.contains(&"1004.0".to_owned()) && landed.contains(&"1006.0".to_owned()),
        "and it arrives with the conversation around it, not on its own: {landed:?}"
    );
}

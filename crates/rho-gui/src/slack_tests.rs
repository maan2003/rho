//! Slack's own tests against the GUI surface.
//!
//! Their own file rather than the end of `tests.rs`: every agent landing a
//! change was appending there, so every rebase collided on the same last
//! line. A test belongs to the surface it is about, and this is where
//! rho-slack's are.

use gpui::{AppContext as _, TestAppContext};

use super::tests::init_test_app;

/// The workspace both tests read, seeded on the server.
///
/// The fake starts empty, so a test that does not seed it has no listing
/// at all — and before this the tests took theirs from the mirror the
/// session opens by default, which is the user's own file. Everything
/// these tests see is now the fake's, which is where mocking belongs.
fn seed_workspace(fake: &rho_slack::fake::Fake) {
    fake.add_user("UA", "ada");
    fake.add_user("UD", "dana");
    fake.add_channel("C1", "design");
    fake.add_channel("C2", "random");
    fake.add_channel("C3", "dev-ops");
    fake.add_channel("C4", "ops-alerts");
    fake.add_dm("D1", "UA");
    fake.add_group("G1", "mpdm-dana--ada-1", &["UD", "UA"]);
}

/// Waits for the listing to have rows.
///
/// The session's own mirror is empty at the start of a test — it is this
/// test's file, not the user's — so the first rows arrive over a real
/// socket from the fake. The gpui executor going quiet says nothing about
/// whether that has happened, so this waits on the list itself. A loaded
/// machine can then only make the test slower, never make it lie.
async fn wait_for_rows(
    cx: &mut TestAppContext,
    window: &gpui::WindowHandle<rho_slack::ui::ListView>,
) -> Vec<String> {
    for _ in 0..200 {
        cx.run_until_parked();
        let rows = window
            .update(cx, |view, _, cx| view.drawn_conversations_for_test(cx))
            .unwrap();
        if !rows.is_empty() {
            return rows;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    panic!("the fake's conversations never reached the listing");
}

/// The point follows the conversation, not the line number.
///
/// A message arriving in a busier conversation moves rows above the reader.
/// If the point stayed on its line it would land on whatever slid under it,
/// which is how a reader opens the wrong conversation by pressing nothing.
///
/// Stood up against the fake Slack server, the way the surface stands up in
/// production: there is no stubbed client here and no test-only path
/// through the session.
#[gpui::test]
async fn rows_moving_above_the_point_leave_the_point_on_its_conversation(cx: &mut TestAppContext) {
    use rho_slack::fake::Fake;

    cx.update(init_test_app);
    // A real server on a real socket, so the deterministic test executor
    // has to be allowed to wait on it.
    cx.executor().allow_parking();
    // The fake is a real server on a real socket, so it has to be started
    // inside the tokio runtime the session's own loops run in.
    let fake = cx
        .update(|cx| gpui_tokio::Tokio::spawn(cx, async { Fake::start().await }))
        .await
        .unwrap()
        .unwrap();

    seed_workspace(&fake);

    let credentials = rho_slack::config::Credentials::parse("acme", "xoxc-test", "cookie").unwrap();
    let client = std::sync::Arc::new(
        rho_slack::api::Client::with_base(credentials, fake.api_base()).unwrap(),
    );
    // Everything Slack keeps on disk, under this test's own directory.
    // There is no default to fall into: rho-slack never resolves the
    // user's state directory, so a test either says where its files go or
    // has none.
    let state = tempfile::tempdir().expect("a state directory of this test's own");
    let paths = rho_slack::config::Paths::under(state.path());
    let window = cx.add_window(|window, cx| {
        let session = cx.new(|cx| rho_slack::session::Session::with_client(client, paths, cx));
        rho_slack::ui::ListView::new(session, rho_slack::ui::Hooks::inert(), window, cx)
    });
    wait_for_rows(cx, &window).await;

    // Sit on a conversation in the middle of the reference workspace's
    // listing, with rows both above and below it.
    let held = rho_slack::types::ChannelId("C2".into());
    window
        .update(cx, |view, window, cx| {
            let at = view
                .row_of_for_test(&held)
                .expect("#random is in the listing");
            view.place_cursor_for_test(at, window, cx);
        })
        .unwrap();
    let (before, was_at) = window
        .update(cx, |view, _, cx| {
            (view.cursor_source(cx), view.row_of_for_test(&held))
        })
        .unwrap();
    assert_eq!(
        before,
        Some(rho_slack::session::Source::Conversation(held.clone())),
        "the reader is on #random"
    );

    // Something lands in the group message at the bottom of the listing.
    // Unread sorts above read, so its row goes to the top and every row
    // between there and the reader moves down one.
    fake.push_frame(serde_json::json!({
        "type": "message",
        "channel": "G1",
        "ts": "9000.0",
        "user": "UD",
        "text": "anyone about?",
    }));

    // The frame crosses a real socket into the session's own tokio loop
    // and comes back, and the gpui executor going quiet says nothing about
    // whether that has happened yet. So wait for the row to move rather
    // than for the executor to park: a loaded machine can then only make
    // this test slower, never make it lie.
    let mut now_at = was_at;
    for _ in 0..200 {
        cx.run_until_parked();
        now_at = window
            .update(cx, |view, _, _| view.row_of_for_test(&held))
            .unwrap();
        if now_at != was_at {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }

    let after = window
        .update(cx, |view, _, cx| view.cursor_source(cx))
        .unwrap();
    assert_ne!(
        was_at, now_at,
        "the listing has to have moved, or this proves nothing"
    );
    assert_eq!(
        after, before,
        "a row moving above the reader must not move the reader"
    );
}

/// Narrowing edits the list rather than drawing it again, and it edits it
/// to the rows the query reaches.
///
/// Against the fake, through a real session: the reader types a word of a
/// name, the rows that do not answer it leave, and deleting a letter puts
/// them back. The point is on a conversation that survives both, so what
/// is asserted is the list, not the redraw.
#[gpui::test]
async fn typing_a_word_of_a_name_leaves_the_rows_it_reaches(cx: &mut TestAppContext) {
    use rho_slack::fake::Fake;

    cx.update(init_test_app);
    cx.executor().allow_parking();
    let fake = cx
        .update(|cx| gpui_tokio::Tokio::spawn(cx, async { Fake::start().await }))
        .await
        .unwrap()
        .unwrap();

    seed_workspace(&fake);

    let credentials = rho_slack::config::Credentials::parse("acme", "xoxc-test", "cookie").unwrap();
    let client = std::sync::Arc::new(
        rho_slack::api::Client::with_base(credentials, fake.api_base()).unwrap(),
    );
    // Everything Slack keeps on disk, under this test's own directory.
    // There is no default to fall into: rho-slack never resolves the
    // user's state directory, so a test either says where its files go or
    // has none.
    let state = tempfile::tempdir().expect("a state directory of this test's own");
    let paths = rho_slack::config::Paths::under(state.path());
    let window = cx.add_window(|window, cx| {
        let session = cx.new(|cx| rho_slack::session::Session::with_client(client, paths, cx));
        rho_slack::ui::ListView::new(session, rho_slack::ui::Hooks::inert(), window, cx)
    });
    let whole = wait_for_rows(cx, &window).await;
    assert!(
        whole.len() > 1,
        "the reference workspace has a list to narrow: {whole:?}"
    );
    let word = whole
        .iter()
        .find_map(|label| {
            let word = label.trim_start_matches(['#', '@']).split('-').next()?;
            (word.len() >= 3).then(|| word[..3].to_owned())
        })
        .expect("a name with a word in it");

    window
        .update(cx, |view, window, cx| {
            view.set_filter(word.clone(), window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    let narrowed = window
        .update(cx, |view, _, cx| view.drawn_conversations_for_test(cx))
        .unwrap();
    assert!(
        !narrowed.is_empty() && narrowed.len() < whole.len(),
        "typing {word:?} narrowed {whole:?} to {narrowed:?}"
    );
    assert!(
        narrowed.iter().all(|label| label
            .to_lowercase()
            .split(['#', '@', '-', '_', ' ', '.'])
            .any(|part| part.starts_with(&word))),
        "every row left answers the query: {narrowed:?}"
    );

    window
        .update(cx, |view, window, cx| {
            view.set_filter(String::new(), window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    let widened = window
        .update(cx, |view, _, cx| view.drawn_conversations_for_test(cx))
        .unwrap();
    assert_eq!(widened, whole, "clearing the query puts every row back");
}

//! Slack's own tests against the GUI surface.
//!
//! Their own file rather than the end of `tests.rs`: every agent landing a
//! change was appending there, so every rebase collided on the same last
//! line. A test belongs to the surface it is about, and this is where
//! rho-slack's are.

use gpui::{AppContext as _, TestAppContext};

use super::tests::{bind_test_keymaps, init_test_app, test_workspace};

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

/// `r` on a message: what the menu offers, and that pressing it lands on
/// Slack.
///
/// Through a real session against the fake, with the fake as the only
/// authority on whether the reaction is there: the assertion is what the
/// server holds, not what the client drew.
#[gpui::test]
async fn reacting_puts_the_emoji_on_the_server_and_pressing_again_takes_it_off(
    cx: &mut TestAppContext,
) {
    use rho_slack::fake::Fake;
    use rho_slack::session::Source;
    use rho_slack::types::{ChannelId, Ts};

    cx.update(init_test_app);
    cx.executor().allow_parking();
    let fake = cx
        .update(|cx| gpui_tokio::Tokio::spawn(cx, async { Fake::start().await }))
        .await
        .unwrap()
        .unwrap();
    seed_workspace(&fake);
    // One message, so the point is on it without any walking, and one
    // reaction already there from someone else — joining a reaction is the
    // commonest thing anyone does with one.
    fake.add_message(
        "C1",
        serde_json::json!({"type": "message", "ts": "100.0", "user": "UD", "text": "shipping it"}),
    );
    fake.live_reaction("C1", "100.0", "UD", "tada");

    let credentials = rho_slack::config::Credentials::parse("acme", "xoxc-test", "cookie").unwrap();
    let client = std::sync::Arc::new(
        rho_slack::api::Client::with_base(credentials, fake.api_base()).unwrap(),
    );
    let state = tempfile::tempdir().expect("a state directory of this test's own");
    let paths = rho_slack::config::Paths::under(state.path());
    let source = Source::Conversation(ChannelId("C1".into()));
    let window = cx.add_window(|window, cx| {
        let session = cx.new(|cx| rho_slack::session::Session::with_client(client, paths, cx));
        rho_slack::ui::ConversationView::new(
            session,
            source,
            rho_slack::ui::Hooks::inert(),
            window,
            cx,
        )
    });

    // The history crosses a real socket, so wait for the message rather
    // than for the executor to go quiet.
    let mut choices = None;
    for _ in 0..200 {
        cx.run_until_parked();
        choices = window
            .update(cx, |view, _, cx| view.reaction_choices(cx))
            .unwrap();
        if choices.is_some() {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    let choices = choices.expect("the message reached the surface");
    assert_eq!(choices.ts, Ts("100.0".into()));
    assert_eq!(
        choices
            .on_message
            .iter()
            .map(|choice| (choice.name.as_str(), choice.glyph.as_str(), choice.mine))
            .collect::<Vec<_>>(),
        vec![("tada", "🎉", false)],
        "what is already on the message, and it is not the reader's"
    );
    assert!(
        !choices.recent.is_empty() && !choices.recent.iter().any(|choice| choice.name == "tada"),
        "then what the reader reaches for, without repeating the row above"
    );

    // The menu the reader reads: the row that is already on the message
    // first, then what they reach for, then by name. No id, no "you".
    let menu = crate::transient::slack_react_menu(&choices);
    let rows: Vec<(&str, &str)> = menu
        .items()
        .iter()
        .map(|item| (item.key(), item.description()))
        .collect();
    assert_eq!(rows.first(), Some(&("a", "🎉 tada")));
    assert_eq!(rows.last(), Some(&("/", "by name…")));
    for (_, description) in &rows {
        assert!(
            !description.contains("U") && !description.to_lowercase().contains("you"),
            "the menu reads as names, not as ids or as \"you\": {description}"
        );
    }

    window
        .update(cx, |view, _, cx| {
            view.react(&Ts("100.0".into()), "tada", cx)
        })
        .unwrap();
    for _ in 0..200 {
        cx.run_until_parked();
        if fake
            .reactions("C1", "100.0")
            .iter()
            .any(|(_, users)| users.iter().any(|user| user == fake.self_id()))
        {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    assert_eq!(
        fake.reactions("C1", "100.0"),
        vec![(
            "tada".to_owned(),
            vec!["UD".to_owned(), fake.self_id().to_owned()]
        )],
        "the server has it, beside the one that was already there"
    );

    // And the same key again is the other half of the one state.
    window
        .update(cx, |view, _, cx| {
            view.react(&Ts("100.0".into()), "tada", cx)
        })
        .unwrap();
    for _ in 0..200 {
        cx.run_until_parked();
        if !fake
            .reactions("C1", "100.0")
            .iter()
            .any(|(_, users)| users.iter().any(|user| user == fake.self_id()))
        {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    assert_eq!(
        fake.reactions("C1", "100.0"),
        vec![("tada".to_owned(), vec!["UD".to_owned()])],
        "taking the reader's off leaves the other standing"
    );
}

/// `s` on the list narrows as the reader types, and escape puts back what
/// they were looking at.
///
/// The whole path the reader walks: the prompt opens, each keystroke
/// reaches the change handler and the list behind it narrows on that
/// keystroke rather than on submit, and escape restores the narrowing that
/// stood when the prompt opened. Through a workspace and a real session
/// against the fake, because the thing being asserted is what is on screen
/// after a key.
#[gpui::test]
async fn typing_narrows_the_list_per_keystroke_and_escape_puts_it_back(cx: &mut TestAppContext) {
    use rho_slack::fake::Fake;

    // The workspace first: `test_workspace` initialises the app, and doing
    // that after the fake had started would replace the runtime the
    // session's loops live in.
    let workspace = test_workspace(cx);
    cx.update(bind_test_keymaps);
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
    let state = tempfile::tempdir().expect("a state directory of this test's own");
    let paths = rho_slack::config::Paths::under(state.path());

    workspace
        .update(cx, |workspace, window, cx| {
            let session = cx.new(|cx| rho_slack::session::Session::with_client(client, paths, cx));
            workspace.install_slack_session_for_test(session, window, cx);
            workspace.open_slack(window, cx);
        })
        .unwrap();

    // The rows come over a real socket, so wait on the listing itself.
    let mut whole = Vec::new();
    for _ in 0..200 {
        cx.run_until_parked();
        // The rows a reader sees are the rows drawn, so draw a frame.
        cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
            .expect("draw a frame");
        cx.run_until_parked();
        whole = workspace
            .update(cx, |workspace, _, cx| workspace.slack_rows_for_test(cx))
            .unwrap();
        if whole.len() > 1 {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    assert!(
        whole.len() > 1,
        "the fake's conversations reached the list: {whole:?}"
    );

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.prompt_slack_search(window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        workspace
            .update(cx, |workspace, _, cx| workspace.slack_rows_for_test(cx))
            .unwrap(),
        whole,
        "opening the prompt is not an edit, so nothing has narrowed yet"
    );

    // One keystroke at a time, and the list is narrower after each: this is
    // the per-keystroke claim, and submitting is not what does it.
    cx.simulate_keystrokes(*workspace, "o");
    cx.run_until_parked();
    let after_one = workspace
        .update(cx, |workspace, _, cx| workspace.slack_rows_for_test(cx))
        .unwrap();
    assert!(
        !after_one.is_empty() && after_one.len() < whole.len(),
        "one keystroke narrowed {whole:?} to {after_one:?}"
    );

    cx.simulate_keystrokes(*workspace, "p s");
    cx.run_until_parked();
    let after_three = workspace
        .update(cx, |workspace, _, cx| workspace.slack_rows_for_test(cx))
        .unwrap();
    assert!(
        after_three.len() <= after_one.len(),
        "and each further keystroke narrows again: {after_one:?} then {after_three:?}"
    );
    assert!(
        after_three
            .iter()
            .all(|label| label.to_lowercase().contains("ops")),
        "every row left answers what was typed: {after_three:?}"
    );

    // Escape is the reader changing their mind: the list goes back to what
    // it was showing before the prompt opened.
    cx.simulate_keystrokes(*workspace, "escape");
    cx.run_until_parked();
    assert_eq!(
        workspace
            .update(cx, |workspace, _, cx| workspace.slack_rows_for_test(cx))
            .unwrap(),
        whole,
        "escape puts back what the reader was looking at"
    );
}

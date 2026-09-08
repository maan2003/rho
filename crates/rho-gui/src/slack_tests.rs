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

    // A narrowing the reader cannot see is a list with rows missing and no
    // reason for it: the narrowing outlives the prompt, and nothing else on
    // screen says so.
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .expect("draw a frame");
    cx.run_until_parked();
    assert_eq!(
        workspace
            .update(cx, |workspace, _, cx| workspace.slack_banner_for_test(cx))
            .unwrap(),
        vec![format!(
            "matching \"ops\" · {} of {}",
            after_three.len(),
            whole.len()
        )],
        "the list says what it is narrowed to, and how much it is keeping off the screen"
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
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .expect("draw a frame");
    cx.run_until_parked();
    assert!(
        workspace
            .update(cx, |workspace, _, cx| workspace.slack_banner_for_test(cx))
            .unwrap()
            .is_empty(),
        "and with nothing narrowed there is nothing to say"
    );
}

/// A picture offered while a rewrite is open is refused, and the rewrite
/// survives it.
///
/// Attaching used to be checked before the edit was, so `enter` posted a
/// brand new message carrying the picture and the rewrite's words, left the
/// original message unchanged and still tinted as being edited, and kept the
/// reader's half-written line stashed where they could not reach it. Two
/// messages where they wanted one. Slack cannot put a file on a message
/// that already exists, so the answer is a refusal at attach time, while
/// the rewrite is still there to be finished or left.
#[gpui::test]
async fn a_picture_offered_mid_rewrite_is_refused_and_the_rewrite_survives(
    cx: &mut TestAppContext,
) {
    use rho_slack::fake::Fake;
    use rho_slack::session::Source;
    use rho_slack::types::ChannelId;
    use rho_slack::ui::conversation::{Attaching, EditStart};

    cx.update(init_test_app);
    cx.executor().allow_parking();
    let fake = cx
        .update(|cx| gpui_tokio::Tokio::spawn(cx, async { Fake::start().await }))
        .await
        .unwrap()
        .unwrap();
    seed_workspace(&fake);
    // The reader's own message, which is the only kind that can be rewritten.
    fake.add_message(
        "C1",
        serde_json::json!({"type": "message", "ts": "100.0", "user": "ME", "text": "on it"}),
    );

    let credentials = rho_slack::config::Credentials::parse("acme", "xoxc-test", "cookie").unwrap();
    let client = std::sync::Arc::new(
        rho_slack::api::Client::with_base(credentials.clone(), fake.api_base()).unwrap(),
    );
    let asking = std::sync::Arc::new(
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

    // The history crosses a real socket, so wait for the rewrite to become
    // possible rather than for the executor to go quiet.
    let mut started = EditStart::Nothing;
    for _ in 0..200 {
        cx.run_until_parked();
        started = window
            .update(cx, |view, window, cx| view.edit_last_own(window, cx))
            .unwrap();
        if matches!(started, EditStart::Started(_)) {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    assert!(
        matches!(started, EditStart::Started(_)),
        "the reader's own message reached the surface and the rewrite opened"
    );

    let outcome = window
        .update(cx, |view, _, cx| {
            view.attach("shot.png".to_owned(), vec![0; 8], cx)
        })
        .unwrap();
    assert_eq!(
        outcome,
        Attaching::NotWhileEditing,
        "the picture is refused while the rewrite is open"
    );

    assert!(
        window
            .update(cx, |view, _, _| view.editing_message().is_some())
            .unwrap(),
        "and the rewrite is still open, waiting to be finished or left"
    );

    // Enter now finishes the rewrite, because that is the only thing open.
    // The answer is not what this test is about, and dropping it drops the
    // answer rather than the rewrite.
    drop(window.update(cx, |view, _, cx| view.submit(cx)).unwrap());
    cx.run_until_parked();
    assert!(
        window
            .update(cx, |view, _, _| view.editing_message().is_none())
            .unwrap(),
        "the rewrite is done with"
    );

    // What Slack holds, which is what everyone else in the channel reads.
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
        held.messages.len(),
        1,
        "one message, the one that was rewritten: {:?}",
        held.messages
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>()
    );
}

/// What goes in the record is what Slack accepted.
///
/// The journal was written the moment enter was pressed: a rewrite the
/// server refused left a record saying the message was edited, and an upload
/// that failed left one saying a file was sent, while the surface -- rightly
/// -- put the reader's words back and told them it had not happened. The
/// record and the screen have to agree, so `submit` now answers with what
/// became of the press and the host writes the record from that.
#[gpui::test]
async fn a_refused_rewrite_and_a_failed_upload_answer_with_a_refusal(cx: &mut TestAppContext) {
    use rho_slack::fake::Fake;
    use rho_slack::session::Source;
    use rho_slack::types::{ChannelId, Ts};
    use rho_slack::ui::conversation::{EditStart, Submitted};

    cx.update(init_test_app);
    cx.executor().allow_parking();
    let fake = cx
        .update(|cx| gpui_tokio::Tokio::spawn(cx, async { Fake::start().await }))
        .await
        .unwrap()
        .unwrap();
    seed_workspace(&fake);
    fake.add_message(
        "C1",
        serde_json::json!({"type": "message", "ts": "100.0", "user": "ME", "text": "on it"}),
    );

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

    // The history crosses a real socket, so wait for the rewrite to become
    // possible rather than for the executor to go quiet.
    let mut started = EditStart::Nothing;
    for _ in 0..200 {
        cx.run_until_parked();
        started = window
            .update(cx, |view, window, cx| view.edit_last_own(window, cx))
            .unwrap();
        if matches!(started, EditStart::Started(_)) {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    assert!(
        matches!(started, EditStart::Started(_)),
        "the reader's own message reached the surface and the rewrite opened"
    );

    fake.fail_next("chat.update", 1);
    let refused = window
        .update(cx, |view, _, cx| view.submit(cx))
        .unwrap()
        .await;
    assert_eq!(
        refused,
        Submitted::Refused,
        "a rewrite the server refused is not an edit that happened"
    );

    // The same press again, with the server willing: this is the edit the
    // record is for.
    let accepted = window
        .update(cx, |view, _, cx| view.submit(cx))
        .unwrap()
        .await;
    assert_eq!(
        accepted,
        Submitted::Edited(Ts("100.0".into())),
        "and an accepted rewrite names the message it rewrote"
    );

    // A picture the upload refuses, the same way.
    fake.fail_next("files.getUploadURLExternal", 1);
    window
        .update(cx, |view, _, cx| {
            view.attach("shot.png".to_owned(), vec![0; 8], cx)
        })
        .unwrap();
    let upload = window
        .update(cx, |view, _, cx| view.submit(cx))
        .unwrap()
        .await;
    assert_eq!(
        upload,
        Submitted::Refused,
        "an upload that failed is not a file that was sent"
    );
}

/// A narrowing that takes the point's row away puts the point on the first
/// match.
///
/// The list holds the rule that the point follows the conversation and never
/// the line number — an arriving message must not move the selection under a
/// keypress. `refresh` kept it by finding the held conversation again after
/// the draw, but only if it was still on screen. When a query filtered it
/// out, nothing placed the point at all: it fell to wherever the editor
/// clamped it, which was the blank line under the listing, and `enter`
/// answered nothing at all. Measured before the fix: six rows narrowed to
/// two, the point on row three, and `cursor_source` answering None.
#[gpui::test]
async fn a_narrowing_that_takes_the_point_s_row_away_puts_it_on_the_first_match(
    cx: &mut TestAppContext,
) {
    use rho_slack::fake::Fake;

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

    let mut whole = Vec::new();
    for _ in 0..200 {
        cx.run_until_parked();
        cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
            .expect("draw a frame");
        cx.run_until_parked();
        whole = workspace
            .update(cx, |workspace, _, cx| workspace.slack_rows_for_test(cx))
            .unwrap();
        if whole.len() > 4 {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    assert!(
        whole.len() > 4,
        "the fake's conversations reached the list: {whole:?}"
    );

    // A row the query will not reach, and not the first one, so the point
    // has somewhere to fall from.
    let held = whole
        .iter()
        .enumerate()
        .find(|(at, label)| *at > 0 && !label.to_lowercase().contains("ops"))
        .map(|(at, _)| at)
        .unwrap_or_else(|| panic!("a row outside the query, below the top: {whole:?}"));
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.slack_place_cursor_for_test(held, window, cx);
        })
        .unwrap();
    assert_eq!(
        workspace
            .update(cx, |workspace, _, cx| workspace
                .slack_cursor_conversation_for_test(cx))
            .unwrap()
            .as_deref(),
        Some(whole[held].as_str()),
        "the point starts on the row it was put on"
    );

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.slack_narrow_for_test("ops", window, cx);
        })
        .unwrap();
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .expect("draw a frame");
    cx.run_until_parked();

    let narrowed = workspace
        .update(cx, |workspace, _, cx| workspace.slack_rows_for_test(cx))
        .unwrap();
    assert!(
        !narrowed.contains(&whole[held]),
        "the query took the point's row away: {narrowed:?}"
    );
    assert_eq!(
        workspace
            .update(cx, |workspace, _, cx| workspace
                .slack_cursor_conversation_for_test(cx))
            .unwrap()
            .as_deref(),
        narrowed.first().map(String::as_str),
        "so the point is on the first match, which is what the reader typed for"
    );
}

/// A name that arrives after the row is drawn reaches the row.
///
/// Someone who joined after rho asked for the roster has no name when their
/// message lands, so the row draws as `someone`. rho asks Slack who they
/// are and the model learns it — and before this the row went on saying
/// `someone` anyway, because a name is not a change to the message and
/// nothing in the update log speaks for one. The reader had to leave the
/// conversation and come back to see who had been talking to them.
#[gpui::test]
async fn a_name_that_arrives_after_the_row_is_drawn_reaches_the_row(cx: &mut TestAppContext) {
    use rho_slack::fake::Fake;
    use rho_slack::session::Source;
    use rho_slack::types::ChannelId;

    cx.update(init_test_app);
    cx.executor().allow_parking();
    let fake = cx
        .update(|cx| gpui_tokio::Tokio::spawn(cx, async { Fake::start().await }))
        .await
        .unwrap()
        .unwrap();
    seed_workspace(&fake);
    fake.add_message(
        "C1",
        serde_json::json!({"type": "message", "ts": "100.0", "user": "UA", "text": "morning"}),
    );

    let credentials = rho_slack::config::Credentials::parse("acme", "xoxc-test", "cookie").unwrap();
    let client = std::sync::Arc::new(
        rho_slack::api::Client::with_base(credentials, fake.api_base()).unwrap(),
    );
    let state = tempfile::tempdir().expect("a state directory of this test's own");
    let paths = rho_slack::config::Paths::under(state.path());
    let window = cx.add_window(|window, cx| {
        let session = cx.new(|cx| rho_slack::session::Session::with_client(client, paths, cx));
        rho_slack::ui::ConversationView::new(
            session,
            Source::Conversation(ChannelId("C1".into())),
            rho_slack::ui::Hooks::inert(),
            window,
            cx,
        )
    });

    // The roster has landed: `ada` is named, which is how the test knows
    // the next person is one rho was never told about.
    for _ in 0..200 {
        cx.run_until_parked();
        let drawn = window
            .update(cx, |view, _, cx| view.drawn_lines_for_test(cx))
            .unwrap();
        if drawn.iter().any(|line| line.starts_with("ada:")) {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }

    fake.add_user("UZ", "zed");
    fake.live_message("C1", "UZ", "hello, just joined");

    let mut drawn = Vec::new();
    for _ in 0..200 {
        cx.run_until_parked();
        drawn = window
            .update(cx, |view, _, cx| view.drawn_lines_for_test(cx))
            .unwrap();
        if drawn.iter().any(|line| line.starts_with("zed:")) {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    assert!(
        drawn.iter().any(|line| line.starts_with("zed:")),
        "the row the newcomer's message drew says who they are: {drawn:?}"
    );
    assert!(
        !drawn.iter().any(|line| line.starts_with("someone:")),
        "and no row is left saying someone: {drawn:?}"
    );
}

/// Runs until the view says the thing the test is waiting for.
///
/// The wait ends on the session saying something changed -- the event the
/// test is about -- from a stream made before the state is read, so a
/// change landing between the read and the wait is buffered rather than
/// missed. The small timer it races decides nothing: it is what lets the
/// test executor's own clock move, so work scheduled behind a timer runs.
/// Nothing here counts tries.
///
/// The one real duration is a backstop. A test that gives up after so many
/// milliseconds fails on a loaded machine and proves nothing on a quiet
/// one, so this one gives up only after longer than any machine takes, and
/// says what it was waiting for when it does.
async fn until<R>(
    session: &gpui::Entity<rho_slack::session::Session>,
    window: gpui::WindowHandle<rho_slack::ui::ConversationView>,
    cx: &mut TestAppContext,
    waiting_for: &str,
    mut ready: impl FnMut(
        &mut rho_slack::ui::ConversationView,
        &mut gpui::Window,
        &mut gpui::Context<rho_slack::ui::ConversationView>,
    ) -> Option<R>,
) -> R {
    use futures::StreamExt as _;

    let mut changed = cx.notifications(session);
    let since = std::time::Instant::now();
    loop {
        cx.run_until_parked();
        if let Some(found) = window
            .update(cx, |view, window, cx| ready(view, window, cx))
            .expect("the conversation's window is open")
        {
            return found;
        }
        if since.elapsed() > std::time::Duration::from_secs(60) {
            panic!("waited for {waiting_for} and it never came");
        }
        let changed = std::pin::pin!(changed.next());
        let clock = std::pin::pin!(cx.executor().timer(std::time::Duration::from_millis(10)));
        futures::future::select(changed, clock).await;
    }
}

/// The point in a transcript is an anchor, so a message changing under it
/// does the right thing without anyone deciding.
///
/// This is a pin, not a fix. The list's cursor is a row index, and a row
/// arriving above it moved the reader onto a conversation they had not
/// chosen — that needed a decision and got one. The transcript looks like
/// the same problem and is not: the point is an editor selection over an
/// anchor, and an anchor inside deleted text collapses to the boundary,
/// which is where the message that followed now starts. Deleting the last
/// message leaves the anchor at the end of what is left, which is the
/// message before it. Both are what a reader wants, and neither is written
/// down anywhere, so this is where they are written down.
#[gpui::test]
async fn a_message_changing_under_the_point_leaves_it_somewhere_the_reader_chose(
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
    // The last one is a different day from the other two, so deleting it
    // takes its day rule with it and the anchor has two rows removed under
    // it rather than one.
    for (ts, text) in [
        ("100.0", "first"),
        ("200.0", "middle"),
        ("1780000000.0", "last"),
    ] {
        fake.add_message(
            "C1",
            serde_json::json!({"type": "message", "ts": ts, "user": "UA", "text": text}),
        );
    }

    let credentials = rho_slack::config::Credentials::parse("acme", "xoxc-test", "cookie").unwrap();
    let client = std::sync::Arc::new(
        rho_slack::api::Client::with_base(credentials, fake.api_base()).unwrap(),
    );
    let state = tempfile::tempdir().expect("a state directory of this test's own");
    let paths = rho_slack::config::Paths::under(state.path());
    let window = cx.add_window(|window, cx| {
        let session = cx.new(|cx| rho_slack::session::Session::with_client(client, paths, cx));
        rho_slack::ui::ConversationView::new(
            session,
            Source::Conversation(ChannelId("C1".into())),
            rho_slack::ui::Hooks::inert(),
            window,
            cx,
        )
    });

    // Everything this test is about reaches the session first and the view
    // refreshes off it, so the session saying something changed is the
    // event each wait below ends on.
    let session = window
        .update(cx, |view, _, _| view.session().clone())
        .expect("the conversation's window is open");

    // The history has to be in before the point goes on it. A point put on
    // a transcript that is still filling sits in text the fill replaces,
    // and comes back at the top: that is the load moving it, which is not
    // what this test is about, and waiting on a count of milliseconds for
    // the load instead is what made it fail on a loaded machine.
    let source = Source::Conversation(ChannelId("C1".into()));
    until(
        &session,
        window,
        cx,
        "the conversation to finish loading",
        |view, _, cx| {
            let loaded = view.session().read(cx).loaded(&source)?;
            (!loaded.loading && loaded.messages.len() == 3).then_some(())
        },
    )
    .await;

    // Then the reader puts the point on the middle message, the way they
    // would by scrolling to it. Placed is not on: the point is not on the
    // message until the view says that is the message under it.
    let point_on = async |ts: &str, text: &str, cx: &mut TestAppContext| {
        let ts = Ts(ts.to_owned());
        until(
            &session,
            window,
            cx,
            &format!("the point to come to rest on {text:?}"),
            |view, window, cx| {
                view.place_cursor_on_for_test(&ts, window, cx);
                view.cursor_message_for_test(cx)
                    .map(|message| message.text)
                    .filter(|under| under == text)
            },
        )
        .await;
    };
    let under = async |text: &str, cx: &mut TestAppContext| {
        until(
            &session,
            window,
            cx,
            &format!("{text:?} to come under the point"),
            |view, _, cx| {
                view.cursor_message_for_test(cx)
                    .map(|message| message.text)
                    .filter(|under| under == text)
            },
        )
        .await
    };

    // One: the message under the point is deleted from somewhere else. The
    // point is on the one that followed it, which is where the reader was
    // reading towards.
    point_on("200.0", "middle", cx).await;
    assert_eq!(
        under("middle", cx).await,
        "middle",
        "which is what the reader's next keypress is about"
    );
    fake.live_delete("C1", "200.0");
    assert_eq!(
        under("last", cx).await,
        "last",
        "the point is on the message that followed, not on the day rule \
         that now sits where it was"
    );

    // Two: the last message in the run is deleted, and its day rule with
    // it. There is nothing after it, so the point is on the one before.
    fake.live_delete("C1", "1780000000.0");
    assert_eq!(
        under("first", cx).await,
        "first",
        "with nothing after it, the point is on the message before"
    );

    // Three: someone rewrites the message the point is on. It is the same
    // message, so the point does not go anywhere.
    fake.live_edit("C1", "100.0", "first, rewritten");
    assert_eq!(
        under("first, rewritten", cx).await,
        "first, rewritten",
        "an edit leaves the point on the message it was on"
    );
}

/// A rewrite whose message someone else deletes closes, and the words are
/// kept.
///
/// Slack will not update a message that is not there. Before this the
/// rewrite stayed open over a message that had gone from the screen: enter
/// sent `chat.update`, Slack refused it, and the refusal put the reader
/// back into the same rewrite with the same words and no line saying why.
/// Pressing enter again did the same thing. The only way out was escape,
/// and nothing said so.
#[gpui::test]
async fn a_rewrite_whose_message_is_deleted_closes_and_keeps_the_words(cx: &mut TestAppContext) {
    use rho_slack::fake::Fake;
    use rho_slack::session::Source;
    use rho_slack::types::ChannelId;
    use rho_slack::ui::conversation::EditStart;

    cx.update(init_test_app);
    cx.executor().allow_parking();
    let fake = cx
        .update(|cx| gpui_tokio::Tokio::spawn(cx, async { Fake::start().await }))
        .await
        .unwrap()
        .unwrap();
    seed_workspace(&fake);
    fake.add_message(
        "C1",
        serde_json::json!({"type": "message", "ts": "100.0", "user": "ME", "text": "on it"}),
    );

    let credentials = rho_slack::config::Credentials::parse("acme", "xoxc-test", "cookie").unwrap();
    let client = std::sync::Arc::new(
        rho_slack::api::Client::with_base(credentials.clone(), fake.api_base()).unwrap(),
    );
    // A second client, for asking Slack what it holds at the end -- which is
    // what everyone else in the channel reads.
    let asking = std::sync::Arc::new(
        rho_slack::api::Client::with_base(credentials, fake.api_base()).unwrap(),
    );
    let state = tempfile::tempdir().expect("a state directory of this test's own");
    let paths = rho_slack::config::Paths::under(state.path());
    let window = cx.add_window(|window, cx| {
        let session = cx.new(|cx| rho_slack::session::Session::with_client(client, paths, cx));
        rho_slack::ui::ConversationView::new(
            session,
            Source::Conversation(ChannelId("C1".into())),
            rho_slack::ui::Hooks::inert(),
            window,
            cx,
        )
    });

    let mut started = EditStart::Nothing;
    for _ in 0..200 {
        cx.run_until_parked();
        started = window
            .update(cx, |view, window, cx| view.edit_last_own(window, cx))
            .unwrap();
        if matches!(started, EditStart::Started(_)) {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    assert!(
        matches!(started, EditStart::Started(_)),
        "the reader's own message is on screen and the rewrite is open"
    );
    window
        .update(cx, |view, _, cx| {
            view.set_compose_for_test("on it, by tuesday".to_owned(), cx)
        })
        .unwrap();

    // Someone else deletes it, from the Slack app or another client.
    fake.live_delete("C1", "100.0");
    let mut open = true;
    for _ in 0..200 {
        cx.run_until_parked();
        open = window
            .update(cx, |view, _, _| view.editing_message().is_some())
            .unwrap();
        if !open {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    assert!(
        !open,
        "the rewrite is closed, rather than open on a message that is gone"
    );
    assert_eq!(
        window
            .update(cx, |view, _, cx| view.compose_text_for_test(cx))
            .unwrap(),
        "on it, by tuesday",
        "and the words the reader typed are in the composer, to send if they \
         still want them"
    );

    // Enter now sends them, rather than refusing an update of a message
    // that is not there and putting the reader back where they started.
    drop(window.update(cx, |view, _, cx| view.submit(cx)).unwrap());
    cx.run_until_parked();
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
        held.messages
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        vec!["on it, by tuesday"],
        "the words reached Slack as a new message"
    );
}

/// A workspace with Slack over the fake server, seeded and drawn.
///
/// The workspace comes first: `test_workspace` initialises the app, and
/// doing that after the fake had started would replace the runtime the
/// session's loops live in. The fake is handed back because it has to
/// outlive the test that reads from it.
async fn slack_workspace(
    cx: &mut TestAppContext,
) -> (
    gpui::WindowHandle<crate::workspace::Workspace>,
    rho_slack::fake::Fake,
    tempfile::TempDir,
) {
    use rho_slack::fake::Fake;

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
    for _ in 0..200 {
        cx.run_until_parked();
        cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
            .expect("draw a frame");
        cx.run_until_parked();
        let rows = workspace
            .update(cx, |workspace, _, cx| workspace.slack_rows_for_test(cx))
            .unwrap();
        if rows.len() > 1 {
            return (workspace, fake, state);
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    panic!("the fake's conversations never reached the list");
}

/// Waits for the results surface to say something other than that it is
/// still asking. The search crosses a real socket, so the executor going
/// quiet says nothing about whether the answer has arrived.
async fn wait_for_results(
    cx: &mut TestAppContext,
    workspace: &gpui::WindowHandle<crate::workspace::Workspace>,
) -> Vec<String> {
    for _ in 0..200 {
        cx.run_until_parked();
        let lines = workspace
            .update(cx, |workspace, _, cx| workspace.slack_results_for_test(cx))
            .unwrap();
        if !lines.iter().any(|line| line.starts_with("looking for")) && !lines.is_empty() {
            return lines;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    panic!("the search never answered");
}

/// A hit is a place, and `enter` on one goes there.
///
/// The whole point of searching messages is arriving at the message. This
/// drives the reader's own keys — `shift-s`, the query, `enter` — and
/// asserts the transcript that comes up is the conversation the hit named,
/// showing the message that matched.
#[gpui::test]
async fn a_search_finds_places_and_enter_goes_to_one(cx: &mut TestAppContext) {
    let (workspace, fake, _state) = slack_workspace(cx).await;
    fake.add_message(
        "C3",
        serde_json::json!({"ts": "700.0", "user": "UD", "text": "the staging rollback is done"}),
    );
    fake.add_message(
        "C1",
        serde_json::json!({"ts": "600.0", "user": "UA", "text": "lunch?"}),
    );

    cx.simulate_keystrokes(*workspace, "shift-s");
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "r o l l b a c k enter");
    let lines = wait_for_results(cx, &workspace).await;

    assert_eq!(
        lines.first().map(String::as_str),
        Some("1 for rollback"),
        "the surface says what it is showing: {lines:?}"
    );
    assert!(
        lines[1].starts_with("dana  #dev-ops  "),
        "a row is who said it, where, and when, with no ids: {lines:?}"
    );
    assert_eq!(
        lines.get(2).map(String::as_str),
        Some("  the staging rollback is done"),
        "and the line that matched, under it: {lines:?}"
    );

    cx.simulate_keystrokes(*workspace, "enter");
    // The conversation is loaded over the same real socket, so wait on the
    // transcript rather than on the executor.
    let mut transcript = Vec::new();
    for _ in 0..200 {
        cx.run_until_parked();
        transcript = workspace
            .update(cx, |workspace, _, cx| {
                workspace.slack_transcript_for_test(cx)
            })
            .unwrap();
        if transcript.iter().any(|line| line.contains("rollback")) {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    assert!(
        transcript
            .iter()
            .any(|line| line.contains("the staging rollback is done")),
        "enter goes to the message, in its conversation: {transcript:?}"
    );
}

/// A search that does not answer says so, once, where the results would be.
/// Silence and "nobody said that" are different facts and the reader is
/// owed the difference.
#[gpui::test]
async fn a_search_that_fails_says_so_where_the_results_would_be(cx: &mut TestAppContext) {
    let (workspace, fake, _state) = slack_workspace(cx).await;
    fake.fail_next("search.messages", 1);

    cx.simulate_keystrokes(*workspace, "shift-s");
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "r o l l b a c k enter");
    let lines = wait_for_results(cx, &workspace).await;

    assert_eq!(
        lines,
        vec![
            "0 for rollback".to_owned(),
            "slack did not answer the search".to_owned(),
        ],
        "one line, in the reader's words: {lines:?}"
    );
}

/// `shift-n` moves the reader through the list they are looking at.
///
/// Narrowed to `ops`, with something unread both inside the narrowing and
/// outside it, the key goes to the one on screen. A key that took them to
/// a conversation the rows cannot show, under a banner still counting the
/// rows, would leave the list and the key disagreeing about where they are.
#[gpui::test]
async fn the_next_unread_key_stays_inside_the_narrowed_list(cx: &mut TestAppContext) {
    let (workspace, fake, _state) = slack_workspace(cx).await;

    // Unread inside the narrowing, and unread outside it -- the outside
    // one newer, so it is what the whole-list walk would reach first. A
    // test where both walks agree would pass either way and say nothing.
    fake.push_frame(serde_json::json!({
        "type": "message",
        "channel": "C3",
        "ts": "9000.0",
        "user": "UD",
        "text": "about ops",
    }));
    fake.push_frame(serde_json::json!({
        "type": "message",
        "channel": "C1",
        "ts": "9001.0",
        "user": "UD",
        "text": "and about the design",
    }));
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.slack_narrow_for_test("ops", window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    let shown = workspace
        .update(cx, |workspace, _, cx| workspace.slack_rows_for_test(cx))
        .unwrap();
    assert!(
        shown.iter().all(|label| label.contains("ops")),
        "the reader is looking at the ops conversations: {shown:?}"
    );

    cx.simulate_keystrokes(*workspace, "shift-n");
    cx.run_until_parked();
    let opened = workspace
        .update(cx, |workspace, _, cx| {
            workspace.slack_open_label_for_test(cx)
        })
        .unwrap();

    assert_eq!(
        opened.as_deref(),
        Some("#dev-ops"),
        "the key went to the unread conversation the list was showing, \
         and not to the one it was not: {shown:?}"
    );
}

/// At the edge of a narrowing the key stops and says what it is not
/// showing. Nothing jumps out of the list, and nothing claims the reader is
/// finished when a conversation they cannot see is waiting.
#[gpui::test]
async fn the_edge_of_a_narrowing_says_what_waits_outside_it(cx: &mut TestAppContext) {
    let (workspace, fake, _state) = slack_workspace(cx).await;

    // The same disagreeing walks as the test above: unread inside the
    // narrowing and unread outside it, the outside one newer.
    fake.push_frame(serde_json::json!({
        "type": "message",
        "channel": "C3",
        "ts": "9000.0",
        "user": "UD",
        "text": "about ops",
    }));
    fake.push_frame(serde_json::json!({
        "type": "message",
        "channel": "C1",
        "ts": "9001.0",
        "user": "UD",
        "text": "and about the design",
    }));
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.slack_narrow_for_test("ops", window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    // The one unread the narrowing reaches, which opening reads.
    cx.simulate_keystrokes(*workspace, "shift-n");
    cx.run_until_parked();
    assert_eq!(
        workspace
            .update(cx, |workspace, _, cx| workspace
                .slack_open_label_for_test(cx))
            .unwrap()
            .as_deref(),
        Some("#dev-ops"),
        "the first press goes to the unread one on screen"
    );

    // And again, with nothing left inside the narrowing.
    cx.simulate_keystrokes(*workspace, "shift-n");
    cx.run_until_parked();
    assert_eq!(
        workspace
            .update(cx, |workspace, _, cx| workspace
                .slack_open_label_for_test(cx))
            .unwrap()
            .as_deref(),
        Some("#dev-ops"),
        "the second press does not move the reader out of the narrowing"
    );
    assert_eq!(
        workspace
            .update(cx, |workspace, _, _| workspace
                .echo_text_for_test()
                .map(str::to_owned))
            .unwrap()
            .as_deref(),
        Some(
            "slack: 1 unread conversation outside the narrowing; \
             s with an empty query shows every conversation"
        ),
        "it says what it is not showing them"
    );
}

/// What one arriving message costs the two Slack surfaces, at a size a
/// reader would work in.
///
/// The per-event and per-frame paths are the two places in this client
/// where a number is the requirement rather than a nicety, so the number is
/// measured here rather than by hand when someone remembers to. Three
/// hundred conversations in the listing and a transcript of several hundred
/// lines is the workspace this is about; the fake's five rows say nothing
/// about either.
///
/// What is asserted is the shape, not the wall clock. The same arrival is
/// measured twice, once against a listing of a handful of conversations and
/// once against three hundred, with the same number of messages either way:
/// a cost that follows the listing shows up as a ratio between the two,
/// which a microsecond ceiling could never say on a machine that swings
/// five to ten times between runs.
///
/// Not in the ordinary run, because even the ratio has been seen to break
/// under a build loading every core: a redraw waiting on the machine is not
/// a redraw walking the listing, and a test that cannot tell them apart
/// while the machine is busy teaches people to ignore it. Run by name when
/// the numbers are wanted, which is what a change to either path owes:
///
/// ```text
/// cargo test -p rho-gui --lib one_arriving_message_costs_what_it_touches \
///     -- --ignored --nocapture
/// ```
#[gpui::test]
#[ignore = "measures a per-event cost, so it needs a quiet machine"]
async fn one_arriving_message_costs_what_it_touches(cx: &mut TestAppContext) {
    let small = arrival_cost(cx, 0).await;
    let big = arrival_cost(cx, 300).await;
    eprintln!("one arriving message, small listing: {small:?}");
    eprintln!("one arriving message, big listing:   {big:?}");
    assert!(
        big.rows > 100,
        "the listing has to be big enough to mean something: {} rows",
        big.rows
    );
    assert!(
        big.lines > 100,
        "and the transcript too: {} lines",
        big.lines
    );
    assert!(
        big.list < small.list * LISTING_FACTOR,
        "a message arriving cost the listing {:?} over {} rows against {:?} over {} rows; \
         that is a cost following the listing, not the row that moved",
        big.list,
        big.rows,
        small.list,
        small.rows
    );
    assert!(
        big.conversation < small.conversation * TRANSCRIPT_FACTOR,
        "a message arriving cost the transcript {:?} beside {:?}, with the same transcript \
         either way; the listing beside it is not the transcript's business",
        big.conversation,
        small.conversation
    );
    // The frame is not asserted yet: it is the next thing to fix, and a
    // ceiling it cannot meet would only be a test that is always red.
}

/// How much bigger a redraw of the listing may be against three hundred
/// conversations than against a handful.
///
/// Not three, which is what the rule wants and what the highlights now
/// cost: reading the cursor at the top of a redraw asks the editor for a
/// display snapshot, and that resyncs the whole buffer however little of it
/// moved. That is the same pass the frame pays and it is the next thing to
/// fix; this bound comes down with it. Twenty-five still catches a redraw
/// that walks the listing itself, which is what it is here for.
const LISTING_FACTOR: u32 = 25;

/// How much bigger a redraw of the open conversation may be when the
/// listing beside it is fifty times longer and its own transcript is the
/// same. One, in principle; three for the noise of a shared machine.
const TRANSCRIPT_FACTOR: u32 = 3;

/// One arrival on both surfaces, measured against a listing of `channels`
/// conversations beside the ones the fake always has.
#[derive(Debug)]
struct ArrivalCost {
    /// The worst redraw of the listing, and how many lines it was drawing.
    list: std::time::Duration,
    rows: usize,
    /// The worst redraw of the open conversation, and its transcript.
    conversation: std::time::Duration,
    lines: usize,
    /// One frame, drawn after the messages have all landed. Measured, not
    /// asserted.
    frame: std::time::Duration,
}

/// Messages delivered one at a time into the open conversation. The same
/// number either way, so the transcript is not what differs between the two
/// measurements.
const ARRIVALS: usize = 200;

async fn arrival_cost(cx: &mut TestAppContext, channels: usize) -> ArrivalCost {
    use rho_slack::fake::Fake;

    let workspace = test_workspace(cx);
    cx.update(bind_test_keymaps);
    cx.executor().allow_parking();
    let fake = cx
        .update(|cx| gpui_tokio::Tokio::spawn(cx, async { Fake::start().await }))
        .await
        .unwrap()
        .unwrap();
    seed_workspace(&fake);
    fake.add_user("UB", "bo");
    for at in 0..channels {
        fake.add_channel(&format!("K{at}"), &format!("team-{at}"));
        fake.add_message(
            &format!("K{at}"),
            serde_json::json!({
                "type": "message",
                "ts": format!("{}.000000", 1_700_000_000 + at),
                "user": "UB",
                "text": "seeded",
            }),
        );
    }

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
    // The listing arrives over a socket, so the executor going quiet says
    // nothing about whether it is there yet.
    for _ in 0..200 {
        cx.run_until_parked();
        cx.draw_window(workspace.into());
        cx.run_until_parked();
        let rows = workspace
            .update(cx, |workspace, _, cx| workspace.slack_rows_for_test(cx))
            .unwrap();
        if rows.len() > channels {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_slack_source(
                rho_slack::session::Source::Conversation(rho_slack::types::ChannelId("C2".into())),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
    cx.draw_window(workspace.into());
    cx.run_until_parked();

    // The messages, one at a time, the way a socket delivers them. The
    // worst of them is what is kept: a cost that creeps with the transcript
    // shows up there and not in an average.
    let mut cost = ArrivalCost {
        list: std::time::Duration::ZERO,
        rows: 0,
        conversation: std::time::Duration::ZERO,
        lines: 0,
        frame: std::time::Duration::ZERO,
    };
    for step in 0..ARRIVALS {
        fake.push_frame(serde_json::json!({
            "type": "message",
            "channel": "C2",
            "ts": format!("{}.000000", 1_800_000_000 + step),
            "user": "UD",
            "text": "another line of the transcript",
        }));
        // Waited for one at a time, because the cost of a message is the
        // cost of a redraw and two messages coalescing into one redraw
        // would measure the wrong thing.
        for _ in 0..50 {
            cx.run_until_parked();
            let drawn = workspace
                .update(cx, |workspace, _, cx| {
                    workspace
                        .slack_conversation_cost_for_test(cx)
                        .map_or(0, |(_, drawn)| drawn)
                })
                .unwrap();
            if drawn > cost.lines {
                break;
            }
            cx.executor()
                .timer(std::time::Duration::from_millis(5))
                .await;
        }
        let (list, conversation) = workspace
            .update(cx, |workspace, _, cx| {
                (
                    workspace.slack_list_cost_for_test(cx),
                    workspace.slack_conversation_cost_for_test(cx),
                )
            })
            .unwrap();
        if let Some((redraw, drawn)) = list {
            cost.list = cost.list.max(redraw);
            cost.rows = drawn;
        }
        if let Some((redraw, drawn)) = conversation {
            cost.conversation = cost.conversation.max(redraw);
            cost.lines = drawn;
        }
    }

    let frame_at = std::time::Instant::now();
    cx.draw_window(workspace.into());
    cost.frame = frame_at.elapsed();
    cost
}

/// A message that asks for the reader becomes a card, by every route that
/// makes one: named in a channel, a direct message, a reply in a followed
/// thread, and ordinary traffic in a channel.
///
/// Written for a report of no Slack cards after three changes to the
/// arrival path. It runs the whole way — socket frame, model, the facts
/// the desk is built from, and the ranking — because every earlier test
/// stopped at arrival, and arrival was never the part in doubt.
#[gpui::test]
async fn a_message_that_asks_for_the_reader_becomes_a_card(cx: &mut TestAppContext) {
    use rho_slack::fake::Fake;
    use rho_slack::model::Attention;

    let workspace = test_workspace(cx);
    cx.update(bind_test_keymaps);
    cx.executor().allow_parking();
    let fake = cx
        .update(|cx| gpui_tokio::Tokio::spawn(cx, async { Fake::start().await }))
        .await
        .unwrap()
        .unwrap();
    seed_workspace(&fake);
    let me = fake.self_id().to_owned();
    // The followed thread has to have a root in the channel before a reply
    // can hang off it, and Slack's follow list is what makes it the
    // reader's.
    fake.add_message(
        "C3",
        serde_json::json!({
            "type": "message",
            "ts": "1700000100.000000",
            "user": "UA",
            "text": "the deploy is stuck",
        }),
    );
    fake.follow_thread("C3", "1700000100.000000");

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
    for _ in 0..200 {
        cx.run_until_parked();
        let rows = workspace
            .update(cx, |workspace, _, cx| workspace.slack_rows_for_test(cx))
            .unwrap();
        if !rows.is_empty() {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    fake.push_frame(serde_json::json!({
        "type": "message",
        "channel": "C1",
        "ts": "1800000100.000000",
        "user": "UA",
        "text": format!("<@{me}> can you look at this?"),
    }));
    fake.push_frame(serde_json::json!({
        "type": "message",
        "channel": "D1",
        "ts": "1800000200.000000",
        "user": "UA",
        "text": "are you around?",
    }));
    fake.push_frame(serde_json::json!({
        "type": "message",
        "channel": "C3",
        "ts": "1800000300.000000",
        "thread_ts": "1700000100.000000",
        "user": "UD",
        "text": "still stuck",
    }));
    fake.push_frame(serde_json::json!({
        "type": "message",
        "channel": "C4",
        "ts": "1800000400.000000",
        "user": "UD",
        "text": "disk is filling up",
    }));

    let mut facts = std::collections::HashMap::new();
    for _ in 0..100 {
        cx.run_until_parked();
        facts = workspace
            .update(cx, |workspace, _, cx| workspace.slack_thread_facts(cx))
            .unwrap();
        if facts
            .values()
            .filter(|facts| facts.reason.is_some())
            .count()
            >= 4
        {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }

    let asking = facts
        .values()
        .filter_map(|facts| {
            facts
                .reason
                .map(|reason| (facts.conversation.clone(), reason))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(
        asking.get("#design"),
        Some(&Attention::Mentioned),
        "a message naming the reader in a channel asks; asking was {asking:?}"
    );
    assert_eq!(asking.get("@ada"), Some(&Attention::DirectMessage));
    assert_eq!(asking.get("#dev-ops"), Some(&Attention::FollowedThread));
    assert_eq!(asking.get("#ops-alerts"), Some(&Attention::ChannelTraffic));

    // And every one of them ranks above the floor the dealer drops cards at.
    let now = chrono::Local::now().fixed_offset();
    for (unit, facts) in &facts {
        if facts.reason.is_none() {
            continue;
        }
        let (label, priority) = crate::dashboard::thread_card_facts(facts, now);
        assert!(
            priority > crate::dashboard::DEAL_QUEUE_FLOOR,
            "{unit:?} says {label:?} at {priority}, which the dealer drops"
        );
    }
}

/// Opening a conversation tells Slack nothing about what has been read.
///
/// Reading is not a verdict: what has been dealt with is the later of rho's
/// own cursor and Slack's read mark, and only the reader saying done moves
/// rho's. A mark written on open would make walking into a channel to see
/// what is in it a done, on every client the reader owns.
///
/// Against the fake, with the server as the only authority: the assertion
/// is that no `conversations.mark` reached it, not that no code path was
/// taken.
#[gpui::test]
async fn opening_a_conversation_marks_nothing_read(cx: &mut TestAppContext) {
    use rho_slack::fake::Fake;
    use rho_slack::session::Source;
    use rho_slack::types::ChannelId;

    cx.update(init_test_app);
    cx.executor().allow_parking();
    let fake = cx
        .update(|cx| gpui_tokio::Tokio::spawn(cx, async { Fake::start().await }))
        .await
        .unwrap()
        .unwrap();
    seed_workspace(&fake);
    fake.add_message(
        "C1",
        serde_json::json!({"type": "message", "ts": "100.0", "user": "UD", "text": "shipping it"}),
    );

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
    let mut arrived = false;
    for _ in 0..200 {
        cx.run_until_parked();
        let drawn = window
            .update(cx, |view, _, cx| view.drawn_lines_for_test(cx))
            .unwrap();
        arrived = drawn.iter().any(|line| line.contains("shipping it"));
        if arrived {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    // Or the test proves nothing: a conversation that never loaded would
    // not have marked anything read either.
    assert!(arrived, "the history reached the surface");
    // A mark in flight would have been sent by now: the history it would
    // have followed is already on screen.
    cx.run_until_parked();
    assert_eq!(
        fake.calls("conversations.mark"),
        0,
        "opening a conversation is not a done"
    );
    assert_eq!(fake.last_read("C1"), None, "and the server's cursor stands");
}

/// Brings a session up against the fake and returns it with the workspace,
/// once the reader's own id is known: the units are about who wrote what,
/// so a card raised before that is a card about a stranger.
async fn workspace_with_slack(
    cx: &mut TestAppContext,
    workspace: gpui::WindowHandle<crate::workspace::Workspace>,
    fake: &rho_slack::fake::Fake,
    state: &tempfile::TempDir,
) -> gpui::WindowHandle<crate::workspace::Workspace> {
    let credentials = rho_slack::config::Credentials::parse("acme", "xoxc-test", "cookie").unwrap();
    let client = std::sync::Arc::new(
        rho_slack::api::Client::with_base(credentials, fake.api_base()).unwrap(),
    );
    let paths = rho_slack::config::Paths::under(state.path());
    workspace
        .update(cx, |workspace, window, cx| {
            let session = cx.new(|cx| rho_slack::session::Session::with_client(client, paths, cx));
            workspace.install_slack_session_for_test(session, window, cx);
            workspace.open_slack(window, cx);
        })
        .unwrap();
    // The listing crosses a real socket. Nothing can be pushed at a
    // conversation the session has not heard of yet.
    for _ in 0..300 {
        cx.run_until_parked();
        let rows = workspace
            .update(cx, |workspace, _, cx| workspace.slack_rows_for_test(cx))
            .unwrap();
        if !rows.is_empty() {
            return workspace;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    panic!("the fake's conversations never reached the workspace");
}

/// Waits until every named unit is asking, or says which one never did.
async fn wait_for_reasons(
    cx: &mut TestAppContext,
    workspace: &gpui::WindowHandle<crate::workspace::Workspace>,
    wanted: &[rho_desk::cells::SlackUnit],
) {
    for _ in 0..300 {
        cx.run_until_parked();
        let facts = workspace
            .update(cx, |workspace, _, cx| workspace.slack_thread_facts(cx))
            .unwrap();
        if wanted
            .iter()
            .all(|unit| facts.get(unit).is_some_and(|facts| facts.reason.is_some()))
        {
            return;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    panic!("the fake's units never asked for the reader");
}

fn slack_unit(channel: &str, thread: Option<&str>) -> rho_desk::cells::SlackUnit {
    rho_desk::cells::SlackUnit {
        workspace: "acme".to_owned(),
        channel: channel.to_owned(),
        thread: thread.map(str::to_owned),
    }
}

fn reason_of(
    workspace: &gpui::WindowHandle<crate::workspace::Workspace>,
    cx: &mut TestAppContext,
    unit: &rho_desk::cells::SlackUnit,
) -> Option<rho_slack::model::Attention> {
    workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .slack_thread_facts(cx)
                .get(unit)
                .and_then(|facts| facts.reason)
        })
        .unwrap()
}

/// `d` on a Slack card: rho's own cursor moves here and now, and the
/// outbox tells Slack the same thing. Done is the later of the two, so
/// either half closing the unit closes it, and the local half is what makes
/// the keystroke instant and true with the workspace offline.
///
/// The undo puts rho's half back and the card returns. What the outbox has
/// already pushed stays pushed: Slack has no way back from a read marker,
/// so this is the same bargain a mute has always made.
#[gpui::test]
async fn a_done_moves_rhos_cursor_and_the_outbox_tells_slack(cx: &mut TestAppContext) {
    use rho_slack::fake::Fake;
    use rho_slack::model::Attention;

    let workspace = test_workspace(cx);
    cx.update(bind_test_keymaps);
    cx.executor().allow_parking();
    let fake = cx
        .update(|cx| gpui_tokio::Tokio::spawn(cx, async { Fake::start().await }))
        .await
        .unwrap()
        .unwrap();
    seed_workspace(&fake);

    let state = tempfile::tempdir().expect("a state directory of this test's own");
    let workspace = workspace_with_slack(cx, workspace, &fake, &state).await;
    fake.push_frame(
        serde_json::json!({"type": "message", "channel": "D1", "ts": "1800000100.000000", "user": "UA", "text": "are you around?"}),
    );
    let unit = slack_unit("D1", None);
    wait_for_reasons(cx, &workspace, std::slice::from_ref(&unit)).await;
    assert_eq!(
        reason_of(&workspace, cx, &unit),
        Some(Attention::DirectMessage)
    );

    let moved = workspace
        .update(cx, |workspace, _, cx| {
            workspace.advance_slack_cursor(&unit, None, cx)
        })
        .unwrap()
        .expect("the unit is one the session knows, so its cursor moves");
    assert_eq!(
        moved.1,
        Default::default(),
        "nothing was handled in it before"
    );
    assert_eq!(
        reason_of(&workspace, cx, &unit),
        None,
        "the card is gone the moment the cursor moved, with nothing sent"
    );

    // And the outbox says the same thing to Slack, which is what closes it
    // on the reader's phone.
    let mut pushed = None;
    for _ in 0..300 {
        cx.run_until_parked();
        pushed = fake.last_read("D1");
        if pushed.is_some() {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    assert_eq!(
        pushed.as_deref(),
        Some("1800000100.000000"),
        "the outbox pushed the read mark rho's cursor said"
    );

    workspace
        .update(cx, |workspace, _, cx| {
            workspace.restore_slack_cursors(std::slice::from_ref(&moved), cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        reason_of(&workspace, cx, &unit),
        Some(Attention::DirectMessage),
        "the undo takes rho's half back, so the card is the reader's again"
    );
}

/// `mark read before` closes a backlog in one keystroke, so it comes back
/// in one: `shift-u` puts every cursor it moved back, not the last of them.
/// The cursor each unit lands on is the caller's rather than the unit's
/// newest, which is what "before an age" means: a conversation with
/// something newer than the cutoff keeps its card.
#[gpui::test]
async fn marking_the_backlog_moves_every_cursor_and_undoes_as_one(cx: &mut TestAppContext) {
    use rho_slack::fake::Fake;

    let workspace = test_workspace(cx);
    cx.update(bind_test_keymaps);
    cx.executor().allow_parking();
    let fake = cx
        .update(|cx| gpui_tokio::Tokio::spawn(cx, async { Fake::start().await }))
        .await
        .unwrap()
        .unwrap();
    seed_workspace(&fake);
    let me = fake.self_id().to_owned();

    let state = tempfile::tempdir().expect("a state directory of this test's own");
    let workspace = workspace_with_slack(cx, workspace, &fake, &state).await;
    fake.push_frame(
        serde_json::json!({"type": "message", "channel": "D1", "ts": "1800000100.000000", "user": "UA", "text": "are you around?"}),
    );
    // The direct message has a second line, after the cutoff the reader
    // will name; the mention has nothing after it.
    fake.push_frame(
        serde_json::json!({"type": "message", "channel": "D1", "ts": "1800000300.000000", "user": "UA", "text": "still there?"}),
    );
    fake.push_frame(
        serde_json::json!({"type": "message", "channel": "C1", "ts": "1800000200.000000", "user": "UA", "text": format!("<@{me}> can you look?")}),
    );
    let direct = slack_unit("D1", None);
    let mention = slack_unit("C1", None);
    wait_for_reasons(cx, &workspace, &[direct.clone(), mention.clone()]).await;

    let closed = workspace
        .update(cx, |workspace, window, cx| {
            let cursor = |ts: &str| rho_desk::cells::SlackTs(ts.to_owned());
            workspace.mark_cards_done(
                rho_agents::HostId::default(),
                vec![
                    (
                        rho_desk::cells::Id::Slack(direct.clone()),
                        cursor("1800000100.000000"),
                    ),
                    (
                        rho_desk::cells::Id::Slack(mention.clone()),
                        cursor("1800000200.000000"),
                    ),
                ],
                "mark read before".to_owned(),
                window,
                cx,
            )
        })
        .unwrap();
    assert_eq!(closed, 2, "both cursors moved");
    cx.run_until_parked();
    assert_eq!(
        reason_of(&workspace, cx, &mention),
        None,
        "the mention was marked at its newest, so it is dealt with"
    );
    assert!(
        reason_of(&workspace, cx, &direct).is_some(),
        "what arrived after the cutoff is still the reader's"
    );
    assert_eq!(
        workspace
            .update(cx, |workspace, _, _| workspace
                .verdict_undo_count_for_test())
            .unwrap(),
        1,
        "one keystroke leaves one thing to undo"
    );

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.undo_verdict(window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert!(
        reason_of(&workspace, cx, &mention).is_some(),
        "the undo puts the whole batch back"
    );
    assert_eq!(
        workspace
            .update(cx, |workspace, _, _| workspace
                .verdict_undo_count_for_test())
            .unwrap(),
        0
    );
}

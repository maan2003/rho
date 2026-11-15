//! The transcript's own streaming path, asked whether the fold map's edits
//! account for its own output.
//!
//! These cover a settled message whose markdown is concealed before more
//! text arrives, markup streaming until its delimiters close, and a turn
//! streaming at the end of a long settled history. Each used to produce an
//! output edit whose new start was derived independently from the new tree.
//!
//! The bad edits' accounting records were:
//!
//! ```text
//! plain assistant, streaming after concealment       37 + 16 -> 54
//! streamed markup, delimiters closing                37 +  3 -> 41
//! edited turn at the end of settled history        2277 + 30 -> 2309
//! ```
//!
//! They were short by one, one and two bytes. These regressions assert that
//! the accounting record remains empty while the original concealment and
//! history-locality assertions stay in their owning tests.

use gpui::{Entity, TestAppContext};

use super::{
    UiMessagePhase, active_editor, agent, assistant, display_text, feed_edit, feed_frame, state,
    stream_text, test_workspace, user,
};
use crate::workspace::Workspace;

/// Everything the fold map has failed to account for since it was last
/// asked, and forgets it.
fn accounting(
    workspace: &gpui::WindowHandle<Workspace>,
    editor: &Entity<editor::Editor>,
    cx: &mut TestAppContext,
) -> Vec<String> {
    workspace
        .update(cx, |_, _, cx| {
            editor.update(cx, |editor, cx| {
                editor
                    .display_map
                    .update(cx, |map, _| map.take_fold_accounting_violations())
            })
        })
        .expect("read what the fold map's accounting recorded")
}

/// Text arriving after a settled message whose markdown is already
/// concealed. `37 + 16 -> 54` before the widening cut.
#[gpui::test]
fn streaming_after_a_concealed_message_accounts_for_the_fold_maps_output(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let original = "**bold** and `code`\n";
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(
            vec![user("go")],
            vec![assistant(original, Some(UiMessagePhase::FinalAnswer))],
        ),
    );
    cx.run_until_parked();

    let editor = active_editor(&workspace, cx);
    // Forget what building the first frame recorded. The sync under test
    // is the one streaming causes, not the one that folded.
    accounting(&workspace, &editor, cx);

    feed_edit(&workspace, cx, agent(1), |state| {
        stream_text(state, 1, original.len(), "more plain text\n")
    });
    cx.run_until_parked();

    let books = accounting(&workspace, &editor, cx);
    assert!(
        books.is_empty(),
        "streaming after a concealed message must account for its output: {books:#?}"
    );
}

/// Markup arriving three bytes at a time until its delimiters close, which
/// is a concealment fold appearing under a stream rather than beside one.
/// `37 + 3 -> 41` before the widening cut.
#[gpui::test]
fn markup_concealing_as_it_streams_accounts_for_the_fold_maps_output(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(vec![user("go")], vec![assistant("", None)]),
    );
    let editor = active_editor(&workspace, cx);
    accounting(&workspace, &editor, cx);

    let message = "Here is **bold** text, `code`, and **more strong** words.\n";
    let mut sent = 0;
    while sent < message.len() {
        let mut next = (sent + 3).min(message.len());
        while !message.is_char_boundary(next) {
            next += 1;
        }
        feed_edit(&workspace, cx, agent(1), |state| {
            stream_text(state, 1, sent, &message[sent..next])
        });
        sent = next;
    }
    for _ in 0..64 {
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(20));
    }
    cx.run_until_parked();

    // The path this drives is the one the other test asserts conceals; if
    // it stopped concealing, this test would be asking its question of a
    // document with no folds in it and would pass for the wrong reason.
    let text = display_text(&workspace, cx);
    assert!(
        !text.contains("**"),
        "the streamed markup concealed, so there are folds to account for: {text:?}"
    );

    let books = accounting(&workspace, &editor, cx);
    assert!(
        books.is_empty(),
        "markup concealing as it streams must account for its output: {books:#?}"
    );
}

/// A turn streaming at the end of a long settled history, which is the
/// transcript as the user actually has it. `2277 + 30 -> 2309` before the
/// widening cut, and the only one of the three off by two.
#[gpui::test]
fn streaming_at_the_end_of_a_settled_history_accounts_for_the_fold_maps_output(
    cx: &mut TestAppContext,
) {
    let workspace = test_workspace(cx);
    let mut history = Vec::new();
    for index in 0..250 {
        history.push(user(&format!("question {index}")));
        history.push(assistant(
            &format!("settled **answer {index}**"),
            Some(UiMessagePhase::FinalAnswer),
        ));
    }
    history.push(user("latest question"));
    let active_index = history.len();
    let initial = "## Initial heading\n\n**initial bold**";
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(history, vec![assistant(initial, None)]),
    );
    for _ in 0..64 {
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(20));
    }

    let editor = active_editor(&workspace, cx);
    accounting(&workspace, &editor, cx);

    feed_edit(&workspace, cx, agent(1), |state| {
        stream_text(
            state,
            active_index,
            initial.len(),
            "\n\n## New heading\n\n**new bold**",
        )
    });
    cx.run_until_parked();

    let books = accounting(&workspace, &editor, cx);
    assert!(
        books.is_empty(),
        "streaming after settled history must account for its output: {books:#?}"
    );
}

//! The transcript's own streaming path, asked whether the fold map's edits
//! account for its own output.
//!
//! These three shapes are not new. They are the shapes of three tests that
//! were already in this suite when the fault was found - a settled message
//! whose markdown is concealed and more text arriving after it, markup
//! streaming in three bytes at a time until its delimiters close, and a
//! turn streaming at the end of a long settled history. All three passed
//! throughout, because each asserts something else about the path:
//! concealment is preserved, markup conceals, history is not revisited.
//! None of them asked whether the fold map told the layers above the truth
//! about how much its output had changed, and all three were carrying a
//! sync that did not.
//!
//! What the accounting record says, on main, today:
//!
//! ```text
//! plain assistant, streaming after concealment       37 + 16 -> 54
//! streamed markup, delimiters closing                37 +  3 -> 41
//! edited turn at the end of settled history        2277 + 30 -> 2309
//! ```
//!
//! Over by one byte, one byte and two bytes: a boundary miscounted rather
//! than a region lost. In each case the output is *larger* than the edits
//! admit, so the layers above are told the document grew less than it did,
//! and go looking for rows at offsets the snapshot does not have.
//!
//! These assertions are written as full characterisation and are **meant to
//! fail on the day the fault is fixed** rather than guarded. Emptiness is
//! the assertion when a rule is fixed; characterisation is the assertion
//! while a known fault remains and the day it changes should be loud. The
//! distinction is eng-8gpr's, arrived at by being caught by each shape in
//! turn.
//!
//! One correction belongs here rather than only in a note, because this
//! file exists because of it. These three were reported as carrying the
//! fault, then retracted on a sweep that said they did not fire, then found
//! to fire after all when the tests were written and run. The sweep counted
//! its marker with a line-anchored pattern; under `--nocapture` the harness
//! prints `test tests::name ... ` without a newline and the test's own
//! output continues that line, so every marker printed during a test landed
//! mid-line and was not counted. The same log read without the anchor had
//! all of them. Twenty seconds of running these three directly would have
//! settled what an hour of experiment did not.
//!
//! The three tests they are drawn from stay where they are and keep
//! asserting what they assert. Duplicating the shape here rather than
//! adding a fourth assertion to each of them keeps the accounting question
//! in one file, where the next person looking for it will find it.

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
    assert_eq!(
        books.len(),
        1,
        "one sync on this document does not account for its own output; it \
         said {books:#?}"
    );
    assert!(
        books[0].contains("net 16, which is 53; the new output is 54"),
        "streaming after a settled concealed message: the output is larger than the edits admit, and by \
         how much is the thing worth knowing; it said {:?}",
        books[0]
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
    assert_eq!(
        books.len(),
        1,
        "one sync on this document does not account for its own output; it \
         said {books:#?}"
    );
    assert!(
        books[0].contains("net 3, which is 40; the new output is 41"),
        "markup concealing as it streams: the output is larger than the edits admit, and by \
         how much is the thing worth knowing; it said {:?}",
        books[0]
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
    assert_eq!(
        books.len(),
        1,
        "one sync on this document does not account for its own output; it \
         said {books:#?}"
    );
    assert!(
        books[0].contains("net 30, which is 2307; the new output is 2309"),
        "a turn streaming after a settled history: the output is larger than the edits admit, and by \
         how much is the thing worth knowing; it said {:?}",
        books[0]
    );
}

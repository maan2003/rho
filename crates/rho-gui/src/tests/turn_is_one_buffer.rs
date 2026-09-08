//! A turn's prose is one buffer, across the user's own words.
//!
//! Every buffer costs an excerpt to compose and a parse to draw, and the
//! floor of opening a long transcript was the count of them. A turn used
//! to be at least three — the question, the answer, and whatever the
//! answer was split from — because the user's words were not markdown and
//! the flag that picks a language also picked the boundary.
//!
//! They are one now, which is only allowed because two things that a
//! buffer edge used to guarantee are guaranteed on their own: the user's
//! markup is not concealed, because concealment is suppressed over their
//! ranges, and a fence one turn leaves open cannot reach the next,
//! because a block that leaves one open ends its chunk.

use gpui::TestAppContext;

use super::{
    UiMessagePhase, agent, assistant, display_text, feed_frame, state, test_workspace, user,
};

/// How many buffers hold transcript text. The multibuffer also carries an
/// empty one for the turn that has not arrived; it holds nothing and costs
/// no parse, so it is not one of these.
fn buffers(workspace: &gpui::WindowHandle<super::Workspace>, cx: &mut TestAppContext) -> usize {
    let editor = super::active_editor(workspace, cx);
    workspace
        .update(cx, |_, _, cx| {
            editor
                .read(cx)
                .buffer()
                .read(cx)
                .all_buffers()
                .into_iter()
                .filter(|buffer| !buffer.read(cx).is_empty())
                .count()
        })
        .expect("count the buffers")
}

/// Two turns of question and answer: two buffers, not six.
#[gpui::test]
fn a_turn_of_prose_is_one_buffer(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(
            Vec::new(),
            vec![
                user("first question"),
                assistant("first answer", Some(UiMessagePhase::FinalAnswer)),
                user("second question"),
                assistant("second answer", Some(UiMessagePhase::FinalAnswer)),
            ],
        ),
    );

    assert_eq!(
        buffers(&workspace, cx),
        1,
        "four blocks of prose are one chunk, so they are one buffer"
    );
    let text = display_text(&workspace, cx);
    for said in ["first question", "first answer", "second question"] {
        assert!(text.contains(said), "{said:?} is not drawn: {text:?}");
    }
}

/// An answer that ends inside a code fence keeps its own buffer, so the
/// fence dies with it and the next turn is drawn as prose.
#[gpui::test]
fn an_unclosed_fence_does_not_reach_the_next_turn(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(
            Vec::new(),
            vec![
                user("show me"),
                assistant(
                    "```rust\nlet unfinished =",
                    Some(UiMessagePhase::FinalAnswer),
                ),
                user("never mind"),
                assistant("**done**", Some(UiMessagePhase::FinalAnswer)),
            ],
        ),
    );

    assert!(
        buffers(&workspace, cx) > 1,
        "the turn with the open fence shares a buffer with the one after it"
    );
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("done") && !text.contains("**done**"),
        "the next turn's markup was read as the fence's contents: {text:?}"
    );
}

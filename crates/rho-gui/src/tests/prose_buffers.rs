//! Which blocks share a buffer, now that only the model's words are
//! markdown.
//!
//! Every buffer costs an excerpt to compose and a parse to draw, so the
//! prose of one turn is kept together where it can be. It can be across
//! the model's own blocks, which are all markdown; it cannot be across
//! the reader's words or a call, because those are shown as they were
//! written and a plain buffer is what shows them that way. A fence one
//! turn leaves open still cannot reach the next, because a block that
//! leaves one open ends its chunk.

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

/// The model's own blocks share one buffer; the reader's words keep their
/// own, because they are not parsed.
#[gpui::test]
fn the_models_blocks_share_a_buffer_and_the_readers_do_not(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(
            Vec::new(),
            vec![
                user("first question"),
                assistant("first answer", Some(UiMessagePhase::Commentary)),
                assistant("still answering", Some(UiMessagePhase::FinalAnswer)),
            ],
        ),
    );

    assert_eq!(
        buffers(&workspace, cx),
        2,
        "the reader's words and the model's two blocks are two buffers"
    );
    let text = display_text(&workspace, cx);
    for said in ["first question", "first answer", "still answering"] {
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

/// A hidden source and its growing message must not replace the visible
/// buffer every frame; Activity still needs the newest source.
#[gpui::test]
fn streaming_message_edits_its_buffer_in_place(cx: &mut TestAppContext) {
    use rho_agents_client::protocol::transcript::{
        ArgumentsFormat, InferenceState, Item, Live, RuntimeState, StreamingResponse,
    };

    let workspace = test_workspace(cx);
    let agent_id = agent(1);
    feed_frame(
        &workspace,
        cx,
        agent_id,
        state(vec![user("question")], vec![]),
    );
    let live = |source: &str, draft: &str| Live::Snapshot {
        state: RuntimeState {
            inference: InferenceState::Responding,
            ..Default::default()
        },
        response: Some(StreamingResponse {
            id: "response".into(),
            items: vec![Item::ToolCall {
                id: "exec".into(),
                name: "exec".into(),
                arguments: source.into(),
                format: ArgumentsFormat::Text,
            }],
        }),
        draft: Some(draft.into()),
    };
    let apply = |workspace: &gpui::WindowHandle<super::Workspace>,
                 cx: &mut TestAppContext,
                 source,
                 draft| {
        workspace
            .update(cx, |workspace, window, cx| {
                workspace.live_transcript_for_test(agent_id, live(source, draft), window, cx);
            })
            .unwrap();
        cx.run_until_parked();
    };
    apply(&workspace, cx, "human.send('Hel", "Hel");
    let message_buffer = |workspace: &gpui::WindowHandle<super::Workspace>,
                          cx: &mut TestAppContext| {
        let editor = super::active_editor(workspace, cx);
        workspace
            .update(cx, |_, _, cx| {
                editor
                    .read(cx)
                    .buffer()
                    .read(cx)
                    .all_buffers()
                    .into_iter()
                    .find(|buffer| buffer.read(cx).text().contains("Hel"))
                    .expect("draft buffer")
                    .read(cx)
                    .remote_id()
            })
            .unwrap()
    };
    let before = message_buffer(&workspace, cx);
    apply(&workspace, cx, "human.send('Hello", "Hello");
    assert_eq!(
        message_buffer(&workspace, cx),
        before,
        "streaming replaced the message buffer"
    );
    assert!(display_text(&workspace, cx).contains("Hello"));
    apply(&workspace, cx, "human.send('Hello\\nworld", "Hello\nworld");
    assert_eq!(
        message_buffer(&workspace, cx),
        before,
        "a line break replaced the draft buffer"
    );
    assert!(display_text(&workspace, cx).contains("Hello\nworld"));
    super::open_activity(&workspace, cx);
    assert!(display_text(&workspace, cx).contains("human.send('Hello\\nworld"));
}

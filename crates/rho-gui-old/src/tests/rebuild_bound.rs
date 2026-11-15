//! What one arriving block costs when the blocks before it are all alike.
//!
//! A buffer is replaced whole: a change that cannot be respliced in place
//! is pulled back to the start of the buffer that holds it, and a block
//! arriving at the end is pulled back into the last buffer so it joins the
//! run it belongs to. Without a cap on how much one buffer holds, that
//! makes an append cost the run: two hundred finished calls and one more
//! rebuilt two hundred and one records, for one block.
//!
//! The cap is on the chunk, which is what a buffer is built from, so the
//! bound is structural — no buffer is longer than a chunk, so no rebuild
//! is longer than one either.

use gpui::TestAppContext;

use super::{
    UiBlock, UiToolStatus, agent, display_text, feed_frame, state, test_workspace, tool, user,
};

/// Comfortably above the cap and the rows of the one block that may cross
/// it, and far below the two hundred calls this transcript holds: the
/// point is that the number does not grow with the run.
const ROWS_A_BUFFER_MAY_HOLD: u32 = 48;

fn call(id: &str) -> UiBlock {
    UiBlock::Tool(tool(id, UiToolStatus::Success, Some(1_000), Some(1_200)))
}

#[gpui::test]
fn no_buffer_holds_a_whole_run_of_calls(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let agent_id = agent(1);
    let mut live = vec![user("do the thing")];
    for index in 0..200 {
        live.push(call(&format!("call-{index}")));
    }
    feed_frame(&workspace, cx, agent_id, state(Vec::new(), live.clone()));
    live.push(call("call-last"));
    feed_frame(&workspace, cx, agent_id, state(Vec::new(), live));

    let editor = super::active_editor(&workspace, cx);
    let longest = workspace
        .update(cx, |_, _, cx| {
            editor
                .read(cx)
                .buffer()
                .read(cx)
                .all_buffers()
                .into_iter()
                .map(|buffer| buffer.read(cx).max_point().row + 1)
                .max()
                .expect("the transcript has buffers")
        })
        .expect("measure the buffers");

    assert!(
        longest <= ROWS_A_BUFFER_MAY_HOLD,
        "a run of two hundred calls is held in one buffer, so an append \
         rebuilds all of it: longest buffer is {longest} rows"
    );
    assert!(
        display_text(&workspace, cx).contains("$ echo ok"),
        "and the call that arrived is drawn"
    );
}

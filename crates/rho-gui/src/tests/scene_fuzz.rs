use std::collections::HashSet;

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DriveEvent {
    ComposerKey { ordinal: u8, character: char },
}

/// A prompt edit is one row-sized visual event. In particular, changing the
/// last excerpt must not repaint wrapped transcript or tool rows above it.
#[gpui::test]
fn composer_keystrokes_change_one_composer_scene(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    cx.simulate_window_resize(*workspace, size(px(720.), px(800.)));

    let mut finished_tool = tool("generated-tool", UiToolStatus::Success, Some(10), Some(20));
    finished_tool.arguments = "printf '%s' generated-argument ".repeat(30);
    finished_tool.output = Some("generated tool output with wrapped columns ".repeat(40));
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(
            vec![
                user("generated request with enough words to wrap across several editor rows "),
                assistant(
                    &"generated assistant transcript row ".repeat(8),
                    Some(UiMessagePhase::FinalAnswer),
                ),
                UiBlock::Tool(finished_tool),
                assistant(
                    &"generated tail row which also wraps ".repeat(6),
                    Some(UiMessagePhase::FinalAnswer),
                ),
            ],
            Vec::new(),
        ),
    );

    let editor = active_editor(&workspace, cx);
    let recorder = cx.record_scenes::<DriveEvent>((*workspace).into());
    cx.draw_window((*workspace).into());
    let (row_top, row_bottom) = workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                let prompt = editor.selections.newest_display(&snapshot).head();
                let position = editor
                    .window_position_for_display_point(prompt, &snapshot, window, cx)
                    .expect("the prompt row is visible");
                let line_height = editor
                    .style(cx)
                    .text
                    .line_height_in_pixels(window.rem_size());
                let scale = window.scale_factor();
                (
                    f32::from(position.y) * scale,
                    (f32::from(position.y) + f32::from(line_height)) * scale,
                )
            })
        })
        .expect("measure the prompt row");

    for (ordinal, character) in "abcdefghij".chars().enumerate() {
        let frame_start = recorder.frames().len();
        recorder.precede(DriveEvent::ComposerKey {
            ordinal: ordinal as u8,
            character,
        });
        workspace
            .update(cx, |_, window, cx| {
                editor.update(cx, |editor, cx| {
                    editor.insert(&character.to_string(), window, cx)
                });
            })
            .expect("type one generated composer character");
        cx.run_until_parked();
        cx.draw_window((*workspace).into());

        let frames = recorder.frames();
        let event_frames = &frames[frame_start..];
        assert!(
            !event_frames.is_empty(),
            "keystroke {ordinal} drew no scene"
        );
        let distinct = event_frames
            .iter()
            .map(|frame| frame.distinct_scene)
            .collect::<HashSet<_>>();
        assert!(
            distinct.len() <= 1,
            "keystroke {ordinal} produced {} distinct scenes",
            distinct.len()
        );
        let changed = event_frames
            .iter()
            .filter_map(|frame| frame.change_bounds)
            .reduce(|left, right| left.union(&right))
            .expect("the typed character changes a scene");
        println!(
            "composer key {ordinal}: frames={} distinct={} changed_primitives={} damage_y={}..{}",
            event_frames.len(),
            distinct.len(),
            event_frames
                .iter()
                .map(|frame| frame.changes.len())
                .sum::<usize>(),
            changed.origin.y.0,
            changed.bottom().0,
        );
        assert!(
            changed.origin.y.0 >= row_top - 1. && changed.bottom().0 <= row_bottom + 1.,
            "keystroke {ordinal} changed y={}..{}, outside composer row {row_top}..{row_bottom}",
            changed.origin.y.0,
            changed.bottom().0,
        );
    }
}

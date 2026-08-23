//! Benchmarks for a desk-shaped composition: one host buffer sliced at
//! many cuts with tiny read-only row buffers interleaved, rendered in a
//! full editor. This is the rho dashboard's exact multibuffer shape.

use editor::{Editor, EditorMode, MultiBuffer};
use gpui::{AppContext as _, BenchAppContext, Entity, Focusable as _};
use language::{Buffer, Capability};
use multi_buffer::composition::{Composition, CompositionSpec, CutSpec, RowSpec, SectionSpec};
use settings::SettingsStore;
use zed_actions::editor::{MoveDown, MoveUp};

struct DeskFixture {
    multi_buffer: Entity<MultiBuffer>,
    composition: Composition,
    spec: CompositionSpec,
}

fn desk_sizes() -> Vec<usize> {
    vec![10, 50, 150]
}

fn build_desk(
    heading_count: usize,
    rows_per_heading: usize,
    cx: &mut BenchAppContext,
) -> DeskFixture {
    let mut text = String::new();
    let mut cut_positions = Vec::new();
    for index in 0..heading_count {
        text.push_str(&format!("* TODO Topic number {index}\n"));
        text.push_str(&format!(
            "a body line describing topic {index} in enough detail to wrap\n"
        ));
        // The cut sits at the body's end with its newline hidden, the
        // same shape the dashboard generates.
        cut_positions.push(text.len() - 1);
    }
    let host = cx.update(|cx| cx.new(|cx| Buffer::local(text, cx)));

    let mut cuts = Vec::new();
    for (index, position) in cut_positions.iter().enumerate() {
        let rows = (0..rows_per_heading)
            .map(|row| RowSpec {
                id: (index * rows_per_heading + row + 1) as u64,
                buffer: cx.update(|cx| {
                    cx.new(|cx| {
                        let mut buffer = Buffer::local(
                            format!(
                                "· agent-{index}-{row} — working on part {row} of topic {index}"
                            ),
                            cx,
                        );
                        buffer.set_capability(Capability::Read, cx);
                        buffer
                    })
                }),
            })
            .collect();
        cuts.push(CutSpec {
            id: (index + 1) as u64,
            position: *position,
            resume: position + 1,
            rows,
        });
    }
    let tail = vec![RowSpec {
        id: u64::MAX,
        buffer: cx.update(|cx| {
            cx.new(|cx| {
                let mut buffer = Buffer::local("+ new agent".to_owned(), cx);
                buffer.set_capability(Capability::Read, cx);
                buffer
            })
        }),
    }];
    let spec = CompositionSpec {
        sections: vec![SectionSpec {
            host,
            start: 0,
            end: None,
            lead: Vec::new(),
            cuts,
        }],
        tail,
    };

    let multi_buffer = cx.update(|cx| {
        cx.new(|_| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::ReadWrite);
            multi_buffer.set_multiple_paths_per_buffer(true);
            multi_buffer
        })
    });
    let mut composition = Composition::default();
    cx.update(|cx| composition.sync(&multi_buffer, &spec, cx));

    DeskFixture {
        multi_buffer,
        composition,
        spec,
    }
}

fn build_editor(
    window: &mut gpui::BenchWindowContext<'_, '_>,
    multi_buffer: Entity<MultiBuffer>,
) -> Entity<Editor> {
    window.update(|window, cx| {
        let editor = window.replace_root(cx, |window, cx| {
            let mut editor = Editor::new(EditorMode::full(), multi_buffer, None, window, cx);
            editor.set_style(editor::EditorStyle::default(), window, cx);
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        editor
    })
}

#[gpui::bench(
    inputs = desk_sizes(),
    group = "Desk composition render",
    input_name = "headings",
    sample_size = 10
)]
fn desk_composition_render(heading_count: &usize, cx: &mut BenchAppContext) {
    init_context(cx);
    let fixture = build_desk(*heading_count, 2, cx);
    let mut window = cx.add_empty_window();
    let editor = build_editor(&mut window, fixture.multi_buffer.clone());

    let mut move_down = true;
    cx.bench_renderer(editor, move |editor, window, cx| {
        if move_down {
            editor.move_down(&MoveDown, window, cx);
        } else {
            editor.move_up(&MoveUp, window, cx);
        }
        move_down = !move_down;
    });
}

#[gpui::bench(
    inputs = desk_sizes(),
    group = "Desk composition typing",
    input_name = "headings",
    sample_size = 10
)]
fn desk_composition_typing(heading_count: &usize, cx: &mut BenchAppContext) {
    init_context(cx);
    let fixture = build_desk(*heading_count, 2, cx);
    let mut window = cx.add_empty_window();
    let editor = build_editor(&mut window, fixture.multi_buffer.clone());

    let mut insert = true;
    cx.bench_renderer(editor, move |editor, window, cx| {
        if insert {
            editor.handle_input("x", window, cx);
        } else {
            editor.backspace(&editor::actions::Backspace, window, cx);
        }
        insert = !insert;
    });
}

#[gpui::bench(
    inputs = desk_sizes(),
    group = "Desk composition resync",
    input_name = "headings",
    sample_size = 10
)]
fn desk_composition_resync_noop(heading_count: &usize, cx: &mut BenchAppContext) {
    init_context(cx);
    let mut fixture = build_desk(*heading_count, 2, cx);

    cx.bench_iter(move |cx| {
        let changed = cx.update(|cx| {
            fixture
                .composition
                .sync(&fixture.multi_buffer, &fixture.spec, cx)
        });
        assert!(!changed);
    });
}

#[gpui::bench(
    inputs = desk_sizes(),
    group = "Desk composition structural",
    input_name = "headings",
    sample_size = 10
)]
fn desk_composition_structural(heading_count: &usize, cx: &mut BenchAppContext) {
    init_context(cx);
    let mut fixture = build_desk(*heading_count, 2, cx);
    // Alternate between the full spec and one with a middle heading's
    // rows removed: each sync rebuilds a couple of excerpts, the way a
    // staffing change or an opened reply would.
    let mut without_middle = fixture.spec.clone();
    let middle = heading_count / 2;
    without_middle.sections[0].cuts[middle].rows.clear();

    let mut use_full = false;
    cx.bench_iter(move |cx| {
        let spec = if use_full {
            &fixture.spec
        } else {
            &without_middle
        };
        use_full = !use_full;
        let changed = cx.update(|cx| fixture.composition.sync(&fixture.multi_buffer, spec, cx));
        assert!(changed);
    });
}

fn init_context(cx: &mut BenchAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        assets::Assets.load_test_fonts(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
    });
}

gpui::bench_group!(
    benches,
    desk_composition_render,
    desk_composition_typing,
    desk_composition_resync_noop,
    desk_composition_structural
);
gpui::bench_main!(benches);

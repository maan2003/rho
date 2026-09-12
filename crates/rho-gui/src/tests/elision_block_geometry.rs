//! Random elisions, blocks and edits against one document, checked for the
//! geometry the block map is allowed to describe.
//!
//! This is here for a crash from the user: `display point out of range`,
//! out of `BlockMap::sync` summarising a range past the end of the wrapped
//! text. The sequences that reach it are not ones anybody writes down, so
//! this drives them: elisions inserted, removed and expanded over rows that
//! meet and overlap, blocks placed below anchors that later lose their
//! rows, and edits that grow and shrink the text under both, with the
//! display snapshot taken after every step because that is what syncs the
//! map.

use gpui::{AppContext as _, TestAppContext, px, size};

/// A seeded shuffle with no dependency behind it. The sequences matter,
/// not the distribution.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound.max(1) as u64) as usize
    }
}

/// `RHO_WALK_PROBE=1` says what each step did, and `RHO_WALK_SEED=n`
/// walks one seed, which is how a failure gets cut down to a test.
fn probing() -> bool {
    std::env::var_os("RHO_WALK_PROBE").is_some()
}

/// Twelve seeds rather than as many as the machine will bear: the walk is
/// in the gate, and a gate pays for its seeds on every run. Both faults it
/// found so far are inside this many - the multi-buffer splice at seed 1
/// and the elision widening at seed 10 - so a shorter walk than this would
/// have missed one of them.
const SEEDS: u64 = 12;
const STEPS: usize = 40;

#[gpui::test]
fn elisions_blocks_and_edits_leave_the_block_map_a_document(cx: &mut TestAppContext) {
    for seed in 1..=SEEDS {
        one_walk(cx, seed);
    }
}

fn one_walk(cx: &mut TestAppContext, seed: u64) {
    if probing() {
        eprintln!("WALK seed {seed}");
    }
    let mut rng = Rng(seed.wrapping_mul(0x9e3779b97f4a7c15) | 1);
    cx.update(crate::tests::init_test_app);
    let long = "a turn of the transcript, long enough that a reader would scroll it";
    let body = (0..40)
        .map(|row| format!("{row} {long}\n"))
        .collect::<String>();
    let editor = cx.add_window(|window, cx| {
        let mut editor = editor::Editor::multi_line(window, cx);
        editor.set_text(body, window, cx);
        editor
    });
    editor
        .update(cx, |editor, window, cx| {
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            window.refresh();
        })
        .expect("soft wrap at the editor's width");
    cx.simulate_window_resize(*editor, size(px(500.), px(800.)));
    cx.run_until_parked();

    let mut elisions: Vec<editor::DisplayElisionId> = Vec::new();
    let mut blocks: Vec<editor::display_map::CustomBlockId> = Vec::new();

    for step in 0..STEPS {
        let choice = rng.below(10);
        if probing() {
            eprintln!("WALK seed {seed} step {step} choice {choice}");
        }
        editor
            .update(cx, |editor, window, cx| {
                let len = editor.buffer().read(cx).read(cx).len().0;
                match choice {
                    0..=2 => {
                        // An elision over a random run of the document,
                        // keeping a tail, which is what a turn gets.
                        let start = rng.below(len);
                        let end = (start + 1 + rng.below(len)).min(len);
                        let snapshot = editor.buffer().read(cx).snapshot(cx);
                        let range = snapshot.anchor_before(multi_buffer::MultiBufferOffset(start))
                            ..snapshot.anchor_before(multi_buffer::MultiBufferOffset(end));
                        let id = editor.insert_display_elisions(
                            vec![editor::DisplayElisionProperties {
                                range,
                                tail_rows: rng.below(3) as u32,
                                height: Some(1),
                                style: editor::display_map::BlockStyle::Flex,
                                render: std::sync::Arc::new(|_| {
                                    gpui::IntoElement::into_any_element(gpui::Empty)
                                }),
                                priority: 0,
                                type_tag: None,
                            }],
                            None,
                            cx,
                        );
                        elisions.extend(id);
                    }
                    3 if !elisions.is_empty() => {
                        let id = elisions.remove(rng.below(elisions.len()));
                        editor.remove_display_elisions([id].into_iter().collect(), None, cx);
                    }
                    4 if !elisions.is_empty() => {
                        let id = elisions[rng.below(elisions.len())];
                        editor.set_display_elisions_expanded(
                            [id].into_iter().collect(),
                            rng.below(2) == 0,
                            None,
                            cx,
                        );
                    }
                    5 => {
                        // A block below an anchor, the shape a transcript
                        // places under an item.
                        let at = rng.below(len);
                        let snapshot = editor.buffer().read(cx).snapshot(cx);
                        let anchor = snapshot.anchor_after(multi_buffer::MultiBufferOffset(at));
                        let ids = editor.insert_blocks(
                            vec![editor::display_map::BlockProperties {
                                placement: editor::display_map::BlockPlacement::Below(anchor),
                                height: Some(1),
                                style: editor::display_map::BlockStyle::Fixed,
                                render: std::sync::Arc::new(|_| {
                                    gpui::IntoElement::into_any_element(gpui::Empty)
                                }),
                                priority: 0,
                            }],
                            None,
                            cx,
                        );
                        blocks.extend(ids);
                    }
                    6 if !blocks.is_empty() => {
                        let id = blocks.remove(rng.below(blocks.len()));
                        editor.remove_blocks([id].into_iter().collect(), None, cx);
                    }
                    7 => {
                        // Text arriving, which is a turn growing.
                        let at = rng.below(len);
                        editor.buffer().update(cx, |buffer, cx| {
                            let snapshot = buffer.snapshot(cx);
                            let anchor =
                                snapshot.anchor_before(multi_buffer::MultiBufferOffset(at));
                            let text = if rng.below(2) == 0 {
                                format!("more text at step {step}\n")
                            } else {
                                format!("more text at step {step} ")
                            };
                            buffer.edit([(anchor..anchor, text)], None, cx);
                        });
                    }
                    8 => {
                        // Text going away, which is a turn replaced by a
                        // shorter one.
                        let start = rng.below(len);
                        let end = (start + 1 + rng.below(200)).min(len);
                        if start < end {
                            editor.buffer().update(cx, |buffer, cx| {
                                let snapshot = buffer.snapshot(cx);
                                let range = snapshot
                                    .anchor_before(multi_buffer::MultiBufferOffset(start))
                                    ..snapshot.anchor_before(multi_buffer::MultiBufferOffset(end));
                                buffer.edit([(range, "")], None, cx);
                            });
                        }
                    }
                    _ => {
                        // Nothing of its own: a sync with no change in
                        // front of it, which is what a frame does.
                        let _ = window;
                    }
                }
                // The snapshot after every step: this is what syncs the
                // block map, and the fault is inside the sync.
                let snapshot = editor.display_snapshot(cx);
                let _ = snapshot.text();
                let _ = snapshot.max_point();
            })
            .unwrap_or_else(|error| panic!("seed {seed} step {step}: {error}"));
        if rng.below(3) == 0 {
            cx.run_until_parked();
        }
    }
    cx.run_until_parked();
}

/// The same walk against a multi-buffer, which is the shape both surfaces
/// have: a turn per buffer under a path of its own, replaced and removed
/// whole, with elisions and blocks standing over rows the replacement is
/// about to take away.
#[gpui::test]
fn a_transcript_of_excerpts_keeps_the_block_map_a_document(cx: &mut TestAppContext) {
    let only = std::env::var("RHO_WALK_SEED")
        .ok()
        .and_then(|seed| seed.parse::<u64>().ok());
    for seed in 1..=SEEDS {
        if only.is_some_and(|only| only != seed) {
            continue;
        }
        one_excerpt_walk(cx, seed);
    }
}

const PATHS: usize = 6;

fn turn_text(tag: usize, rows: usize) -> String {
    (0..rows)
        .map(|row| format!("row {row} of turn {tag}, long enough that a reader would scroll it\n"))
        .collect()
}

fn one_excerpt_walk(cx: &mut TestAppContext, seed: u64) {
    if probing() {
        eprintln!("EXCERPT WALK seed {seed}");
    }
    let mut rng = Rng(seed.wrapping_mul(0x9e3779b97f4a7c15) | 1);
    cx.update(crate::tests::init_test_app);

    let mut buffers: Vec<gpui::Entity<language::Buffer>> = Vec::new();
    let multi_buffer = cx.update(|cx| {
        cx.new(|cx| {
            let mut multi_buffer =
                multi_buffer::MultiBuffer::without_headers(language::Capability::ReadWrite);
            for path in 0..PATHS {
                let buffer = cx.new(|cx| language::Buffer::local(turn_text(path, 6), cx));
                let end = buffer.read(cx).max_point();
                multi_buffer.set_excerpts_for_path(
                    multi_buffer::PathKey::sorted(path as u64),
                    buffer.clone(),
                    [language::Point::zero()..end],
                    0,
                    cx,
                );
                buffers.push(buffer);
            }
            multi_buffer
        })
    });
    let window = cx.add_window(|window, cx| {
        editor::Editor::new(
            editor::EditorMode::Full {
                scale_ui_elements_with_buffer_font_size: true,
                show_active_line_background: false,
                sizing_behavior: editor::SizingBehavior::ExcludeOverscrollMargin,
            },
            multi_buffer.clone(),
            None,
            window,
            cx,
        )
    });
    let editor = window.root(cx).expect("editor");
    window
        .update(cx, |editor, window, cx| {
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            window.refresh();
        })
        .expect("soft wrap at the editor's width");
    cx.simulate_window_resize(window.into(), size(px(500.), px(800.)));
    cx.run_until_parked();

    let mut elisions: Vec<editor::DisplayElisionId> = Vec::new();
    let mut blocks: Vec<editor::display_map::CustomBlockId> = Vec::new();
    let mut present = [true; PATHS];

    for step in 0..STEPS {
        let choice = rng.below(10);
        if probing() {
            eprintln!("EXCERPT WALK seed {seed} step {step} choice {choice}");
        }
        cx.update(|cx| {
            let len = multi_buffer.read(cx).read(cx).len().0;
            editor.update(cx, |editor, cx| match choice {
                0..=2 => {
                    let start = rng.below(len);
                    let end = (start + 1 + rng.below(len)).min(len);
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    let range = snapshot.anchor_before(multi_buffer::MultiBufferOffset(start))
                        ..snapshot.anchor_before(multi_buffer::MultiBufferOffset(end));
                    elisions.extend(editor.insert_display_elisions(
                        vec![editor::DisplayElisionProperties {
                            range,
                            tail_rows: rng.below(3) as u32,
                            height: Some(1),
                            style: editor::display_map::BlockStyle::Flex,
                            render: std::sync::Arc::new(|_| {
                                gpui::IntoElement::into_any_element(gpui::Empty)
                            }),
                            priority: 0,
                            type_tag: None,
                        }],
                        None,
                        cx,
                    ));
                }
                3 if !elisions.is_empty() => {
                    let id = elisions.remove(rng.below(elisions.len()));
                    editor.remove_display_elisions([id].into_iter().collect(), None, cx);
                }
                4 if !elisions.is_empty() => {
                    let id = elisions[rng.below(elisions.len())];
                    editor.set_display_elisions_expanded(
                        [id].into_iter().collect(),
                        rng.below(2) == 0,
                        None,
                        cx,
                    );
                }
                5 => {
                    let at = rng.below(len);
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    let anchor = snapshot.anchor_after(multi_buffer::MultiBufferOffset(at));
                    blocks.extend(editor.insert_blocks(
                        vec![editor::display_map::BlockProperties {
                            placement: editor::display_map::BlockPlacement::Below(anchor),
                            height: Some(1),
                            style: editor::display_map::BlockStyle::Fixed,
                            render: std::sync::Arc::new(|_| {
                                gpui::IntoElement::into_any_element(gpui::Empty)
                            }),
                            priority: 0,
                        }],
                        None,
                        cx,
                    ));
                }
                6 if !blocks.is_empty() => {
                    let id = blocks.remove(rng.below(blocks.len()));
                    editor.remove_blocks([id].into_iter().collect(), None, cx);
                }
                7 => {
                    // A turn replaced by a new buffer, shorter or longer,
                    // which is what a frame batch does.
                    let path = rng.below(PATHS);
                    let rows = 1 + rng.below(12);
                    if probing() {
                        eprintln!("  replace path {path} with {rows} rows");
                    }
                    let buffer = cx.new(|cx| language::Buffer::local(turn_text(path, rows), cx));
                    let end = buffer.read(cx).max_point();
                    editor.buffer().update(cx, |multi_buffer, cx| {
                        multi_buffer.set_excerpts_for_path(
                            multi_buffer::PathKey::sorted(path as u64),
                            buffer.clone(),
                            [language::Point::zero()..end],
                            0,
                            cx,
                        );
                    });
                    editor.disable_header_for_buffer(buffer.read(cx).remote_id(), cx);
                    buffers[path] = buffer;
                    present[path] = true;
                }
                8 if present.iter().any(|there| *there) => {
                    // A turn taken away entirely.
                    let mut path = rng.below(PATHS);
                    while !present[path] {
                        path = (path + 1) % PATHS;
                    }
                    present[path] = false;
                    if probing() {
                        eprintln!("  remove path {path}");
                    }
                    let buffer = buffers[path].clone();
                    editor.buffer().update(cx, |multi_buffer, cx| {
                        multi_buffer.set_excerpts_for_paths(
                            [(
                                multi_buffer::PathKey::sorted(path as u64),
                                buffer,
                                Vec::new(),
                            )],
                            0,
                            cx,
                        );
                    });
                }
                _ => {
                    // Text arriving at the end of a turn, which is
                    // streaming: the turn's own buffer, never the
                    // multi-buffer over it.
                    let path = rng.below(PATHS);
                    if probing() {
                        eprintln!("  append to path {path}");
                    }
                    let text = if rng.below(2) == 0 {
                        format!("more at {step}\n")
                    } else {
                        format!("more at {step} ")
                    };
                    buffers[path].update(cx, |buffer, cx| {
                        let end = buffer.len();
                        buffer.edit([(end..end, text)], None, cx);
                    });
                    let _ = len;
                }
            });
            // The snapshot after every step, which is what syncs the block
            // map.
            editor.update(cx, |editor, cx| {
                let snapshot = editor.display_snapshot(cx);
                let _ = snapshot.text();
                let _ = snapshot.max_point();
            });
        });
        if rng.below(3) == 0 {
            cx.run_until_parked();
        }
    }
    cx.run_until_parked();
}

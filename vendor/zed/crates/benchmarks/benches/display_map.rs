use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use editor::{MultiBuffer, display_map::*};
use gpui::{AppContext as _, HighlightStyle, Hsla, TestDispatcher, font, px};
use itertools::Itertools;
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBufferOffset, PathKey};
use project::project_settings::DiagnosticSeverity;
use rand::{Rng, SeedableRng, rngs::StdRng};
use settings::SettingsStore;
use std::{num::NonZeroU32, ops::Range, time::Duration};
use text::Bias;
use util::RandomCharIter;

fn markdown_history(
    cx: &mut gpui::TestAppContext,
    turn_count: usize,
) -> (gpui::Entity<DisplayMap>, Vec<Range<MultiBufferOffset>>) {
    let multi_buffer = cx.new(|_| MultiBuffer::without_headers(Capability::Read));
    for turn in 0..turn_count {
        let text = format!(
            "## Response {turn}\n\nThis is **important history** with `inline code`.\n\n{}\n",
            "A long paragraph keeps the fixture representative of transcript wrapping. ".repeat(3)
        );
        let buffer = cx.new(|cx| Buffer::local(&text, cx));
        let end = buffer.read_with(cx, |buffer, _| buffer.max_point());
        multi_buffer.update(cx, |multi_buffer, cx| {
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(turn as u64),
                buffer,
                [Point::zero()..end],
                0,
                cx,
            );
        });
    }

    let snapshot = multi_buffer.read_with(cx, |multi_buffer, cx| multi_buffer.snapshot(cx));
    let text = snapshot.text();
    let mut ranges = Vec::new();
    let mut cursor = 0;
    for line in text.split_inclusive('\n') {
        if line.starts_with("##") {
            ranges.push(cursor..cursor + 3);
        }
        for needle in ["**", "`"] {
            let mut rest = line;
            let mut line_offset = 0;
            while let Some(found) = rest.find(needle) {
                let start = cursor + line_offset + found;
                ranges.push(start..start + needle.len());
                line_offset += found + needle.len();
                rest = &rest[found + needle.len()..];
            }
        }
        cursor += line.len();
    }
    let ranges = ranges
        .into_iter()
        .map(|range| MultiBufferOffset(range.start)..MultiBufferOffset(range.end))
        .collect();
    let map = cx.new(|cx| {
        DisplayMap::new(
            multi_buffer,
            font("Courier"),
            px(16.0),
            None,
            1,
            1,
            FoldPlaceholder::default(),
            DiagnosticSeverity::Warning,
            cx,
        )
    });
    (map, ranges)
}

fn markdown_concealment_benchmark(c: &mut Criterion) {
    let dispatcher = TestDispatcher::new(1);
    let mut cx = gpui::TestAppContext::build(dispatcher, None);
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        editor::init(cx);
    });

    let mut group = c.benchmark_group("Markdown concealment");
    group.sample_size(20);
    for turn_count in [100usize, 500] {
        let (map, ranges) = markdown_history(&mut cx, turn_count);
        cx.update(|cx| {
            map.update(cx, |map, cx| {
                map.replace_concealments(ranges.clone(), cx);
            });
        });
        group.bench_with_input(
            BenchmarkId::new("unchanged history refresh", turn_count),
            &(&map, &ranges),
            |bench, (map, ranges)| {
                bench.iter(|| {
                    cx.update(|cx| {
                        map.update(cx, |map, cx| {
                            map.replace_concealments(black_box((*ranges).clone()), cx);
                        });
                    });
                });
            },
        );
        let mut changed = ranges.clone();
        let tail = changed.last().unwrap().end;
        changed.push(tail + 1usize..tail + 2usize);
        let use_changed = std::cell::Cell::new(true);
        group.bench_with_input(
            BenchmarkId::new("tail concealment changed", turn_count),
            &(&map, &ranges, &changed),
            |bench, (map, original, changed)| {
                bench.iter(|| {
                    let next = if use_changed.replace(!use_changed.get()) {
                        *changed
                    } else {
                        *original
                    };
                    cx.update(|cx| {
                        map.update(cx, |map, cx| {
                            map.replace_concealments(black_box(next.clone()), cx);
                        });
                    });
                });
            },
        );
    }
    group.finish();
}

fn to_tab_point_benchmark(c: &mut Criterion) {
    let dispatcher = TestDispatcher::new(1);
    let cx = gpui::TestAppContext::build(dispatcher, None);

    let create_tab_map = |length: usize| {
        let mut rng = StdRng::seed_from_u64(1);
        let text = RandomCharIter::new(&mut rng)
            .take(length)
            .collect::<String>();
        let buffer = cx.update(|cx| MultiBuffer::build_simple(&text, cx));

        let buffer_snapshot = cx.read(|cx| buffer.read(cx).snapshot(cx));
        use editor::display_map::*;
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot.clone());
        let fold_point = fold_snapshot.to_fold_point(
            inlay_snapshot.to_point(InlayOffset(
                rng.random_range(MultiBufferOffset(0)..MultiBufferOffset(length)),
            )),
            Bias::Left,
        );
        let (_, snapshot) = TabMap::new(fold_snapshot, NonZeroU32::new(4).unwrap());

        (length, snapshot, fold_point)
    };

    let inputs = [1024].into_iter().map(create_tab_map).collect_vec();

    let mut group = c.benchmark_group("To tab point");

    for (batch_size, snapshot, fold_point) in inputs {
        group.bench_with_input(
            BenchmarkId::new("to_tab_point", batch_size),
            &snapshot,
            |bench, snapshot| {
                bench.iter(|| {
                    snapshot.fold_point_to_tab_point(fold_point);
                });
            },
        );
    }

    group.finish();
}

fn to_fold_point_benchmark(c: &mut Criterion) {
    let dispatcher = TestDispatcher::new(1);
    let cx = gpui::TestAppContext::build(dispatcher, None);

    let create_tab_map = |length: usize| {
        let mut rng = StdRng::seed_from_u64(1);
        let text = RandomCharIter::new(&mut rng)
            .take(length)
            .collect::<String>();
        let buffer = cx.update(|cx| MultiBuffer::build_simple(&text, cx));

        let buffer_snapshot = cx.read(|cx| buffer.read(cx).snapshot(cx));
        use editor::display_map::*;
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot.clone());

        let fold_point = fold_snapshot.to_fold_point(
            inlay_snapshot.to_point(InlayOffset(
                rng.random_range(MultiBufferOffset(0)..MultiBufferOffset(length)),
            )),
            Bias::Left,
        );

        let (_, snapshot) = TabMap::new(fold_snapshot, NonZeroU32::new(4).unwrap());
        let tab_point = snapshot.fold_point_to_tab_point(fold_point);

        (length, snapshot, tab_point)
    };

    let inputs = [1024].into_iter().map(create_tab_map).collect_vec();

    let mut group = c.benchmark_group("To fold point");

    for (batch_size, snapshot, tab_point) in inputs {
        group.bench_with_input(
            BenchmarkId::new("to_fold_point", batch_size),
            &snapshot,
            |bench, snapshot| {
                bench.iter(|| {
                    snapshot.tab_point_to_fold_point(tab_point, Bias::Left);
                });
            },
        );
    }

    group.finish();
}

fn create_highlight_endpoints_benchmark(c: &mut Criterion) {
    const LINE_COUNT: usize = 20_000;
    const LINE_VIEW_PORT_COUNT: usize = 100;
    const HIGHLIGHTS_PER_LINE: usize = 4;

    let dispatcher = TestDispatcher::new(1);
    let mut cx = gpui::TestAppContext::build(dispatcher, None);
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        editor::init(cx);
    });

    let mut text = String::new();
    let mut highlight_ranges = Vec::with_capacity(LINE_COUNT * HIGHLIGHTS_PER_LINE);
    for line in 0..LINE_COUNT {
        text.push_str("fn item_");
        text.push_str(&format!("{line:05}"));
        text.push_str("() { ");

        let start = text.len();
        text.push_str("alpha_highlight");
        highlight_ranges.push(MultiBufferOffset(start)..MultiBufferOffset(text.len()));

        text.push_str(" + ");
        let start = text.len();
        text.push_str("beta_highlight");
        highlight_ranges.push(MultiBufferOffset(start)..MultiBufferOffset(text.len()));

        text.push_str(" + ");
        let start = text.len();
        text.push_str("gamma_highlight");
        highlight_ranges.push(MultiBufferOffset(start)..MultiBufferOffset(text.len()));

        text.push_str(" + ");
        let start = text.len();
        text.push_str("delta_highlight");
        highlight_ranges.push(MultiBufferOffset(start)..MultiBufferOffset(text.len()));

        text.push_str("; }\n");
    }

    let buffer = cx.update(|cx| MultiBuffer::build_simple(&text, cx));
    let buffer_snapshot = cx.read(|cx| buffer.read(cx).snapshot(cx));
    let highlight_ranges = highlight_ranges
        .into_iter()
        .map(|range| {
            buffer_snapshot.anchor_before(range.start)..buffer_snapshot.anchor_before(range.end)
        })
        .collect();

    let map = cx.new(|cx| {
        DisplayMap::new(
            buffer,
            font("Courier"),
            px(16.0),
            None,
            1,
            1,
            FoldPlaceholder::default(),
            DiagnosticSeverity::Warning,
            cx,
        )
    });
    cx.update(|cx| {
        map.update(cx, |map, cx| {
            map.highlight_text(
                HighlightKey::Editor,
                highlight_ranges,
                HighlightStyle {
                    color: Some(Hsla::blue()),
                    ..Default::default()
                },
                false,
                cx,
            );
        });
    });
    let snapshot = cx.update(|cx| map.update(cx, |map, cx| map.snapshot(cx)));

    let mut group = c.benchmark_group("Create highlight endpoints");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));
    group.bench_with_input(
        BenchmarkId::new("text_highlights", LINE_VIEW_PORT_COUNT),
        &snapshot,
        |bench, snapshot| {
            bench.iter(|| {
                black_box(snapshot.chunks(
                    DisplayRow(400)..DisplayRow(400 + LINE_VIEW_PORT_COUNT as u32),
                    language::LanguageAwareStyling {
                        tree_sitter: false,
                        diagnostics: false,
                    },
                    Default::default(),
                ));
            });
        },
    );
    group.finish();
}

criterion_group!(
    benches,
    to_tab_point_benchmark,
    to_fold_point_benchmark,
    create_highlight_endpoints_benchmark,
    markdown_concealment_benchmark
);
criterion_main!(benches);

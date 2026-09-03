use editor::{Editor, EditorMode, HighlightKey, SizingBehavior};
use gpui::{App, AppContext as _, Entity, HighlightStyle, TestAppContext};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey};
use text::{ToOffset as _, ToPoint as _};

use crate::{Item, Style, Transcript};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Class {
    Author,
    Body,
}

const KEY_BASE: usize = usize::MAX - 4000;

impl Style for Class {
    fn highlight_key(self, bucket: u32) -> HighlightKey {
        let slot = match self {
            Self::Author => 0,
            Self::Body => 1,
        };
        HighlightKey::SyntaxTreeView(KEY_BASE + bucket as usize * 2 + slot)
    }

    fn highlight_style(self, _cx: &App) -> HighlightStyle {
        HighlightStyle::default()
    }

    fn background(self, bucket: u32, _cx: &App) -> Option<(HighlightKey, gpui::Hsla)> {
        // Only the body tints, the way an unfurl's card does.
        matches!(self, Self::Body).then(|| {
            (
                HighlightKey::SyntaxTreeView(KEY_BASE - 1 - bucket as usize),
                gpui::red(),
            )
        })
    }
}

type Sheet = Transcript<String, Class, ()>;

fn item(key: &str, text: &str) -> Item<String, Class, ()> {
    let body = text.len().min(3)..text.trim_end().len();
    Item::new(key.to_owned(), text)
        .with_styles(vec![
            (Class::Author, 0..text.len().min(3)),
            (Class::Body, body.clone()),
        ])
        .with_backgrounds(vec![(Class::Body, body)])
}

fn buffer(cx: &mut TestAppContext) -> Entity<Buffer> {
    cx.update(|cx| {
        cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        })
    })
}

/// The edits one operation made, as the buffer itself reports them.
fn edits(
    buffer: &Entity<Buffer>,
    cx: &mut TestAppContext,
    operation: impl FnOnce(&mut App),
) -> Vec<std::ops::Range<usize>> {
    let subscription = cx.update(|cx| buffer.update(cx, |buffer, _| buffer.subscribe()));
    cx.update(operation);
    subscription
        .consume()
        .into_inner()
        .into_iter()
        .map(|edit| edit.old.start..edit.old.end)
        .collect()
}

#[gpui::test]
fn a_prepended_run_edits_only_the_front(cx: &mut TestAppContext) {
    let buffer = buffer(cx);
    let mut sheet = Sheet::new(buffer.clone());
    cx.update(|cx| {
        sheet.insert_before(None, vec![item("b", "second\n"), item("c", "third\n")], cx);
    });
    // An anchor on the last item, the way a cursor or a scroll anchor sits
    // on a message the reader is looking at.
    let anchor = cx.update(|cx| {
        let snapshot = buffer.read(cx).snapshot();
        snapshot.anchor_after(
            snapshot
                .text()
                .find("third")
                .expect("third is in the buffer"),
        )
    });

    let edited = edits(&buffer, cx, |cx| {
        sheet.insert_before(Some(&"b".to_owned()), vec![item("a", "first\n")], cx);
    });

    assert_eq!(edited, vec![0..0], "a prepend is one edit at offset 0");
    cx.update(|cx| {
        let snapshot = buffer.read(cx).snapshot();
        assert_eq!(snapshot.text(), "first\nsecond\nthird\n");
        let offset = anchor.to_offset(&snapshot);
        assert!(
            snapshot.text()[offset..].starts_with("third"),
            "the anchor stays on its own message"
        );
    });
}

#[gpui::test]
fn an_appended_item_edits_only_the_end(cx: &mut TestAppContext) {
    let buffer = buffer(cx);
    let mut sheet = Sheet::new(buffer.clone());
    cx.update(|cx| {
        sheet.insert_before(None, vec![item("a", "first\n"), item("b", "second\n")], cx);
    });
    let anchor = cx.update(|cx| {
        let snapshot = buffer.read(cx).snapshot();
        snapshot.anchor_after(0)
    });

    let edited = edits(&buffer, cx, |cx| {
        sheet.insert_before(None, vec![item("c", "third\n")], cx);
    });

    assert_eq!(edited, vec![13..13], "an append is one edit at the end");
    cx.update(|cx| {
        let snapshot = buffer.read(cx).snapshot();
        assert_eq!(snapshot.text(), "first\nsecond\nthird\n");
        assert_eq!(anchor.to_offset(&snapshot), 0);
    });
}

#[gpui::test]
fn replacing_an_item_edits_only_its_range_and_its_bucket(cx: &mut TestAppContext) {
    let buffer = buffer(cx);
    let mut sheet = Sheet::new(buffer.clone());
    // Two buckets' worth, so a replacement in the first must leave the
    // second's highlights alone.
    let many = (0..crate::BUCKET + 4)
        .map(|index| item(&format!("m{index}"), &format!("message {index}\n")))
        .collect::<Vec<_>>();
    cx.update(|cx| sheet.insert_before(None, many, cx));
    let first_bucket = sheet.last_painted_buckets().to_vec();
    assert_eq!(first_bucket.len(), 2, "two buckets were filled");

    let target = "m5".to_owned();
    let range = sheet.range_of(&target).expect("the item is there");
    let (start, end) = cx.update(|cx| {
        let snapshot = buffer.read(cx).snapshot();
        (
            range.start.to_offset(&snapshot),
            range.end.to_offset(&snapshot),
        )
    });

    let edited = edits(&buffer, cx, |cx| {
        sheet.replace(&target, item("m5", "message 5 (edited)\n"), cx);
    });

    assert_eq!(edited, vec![start..end], "only that item's range is edited");
    assert_eq!(
        sheet.last_painted_buckets(),
        &[0],
        "only the replaced item's bucket is re-sent"
    );
    cx.update(|cx| {
        assert!(
            buffer.read(cx).text().contains("message 5 (edited)\n"),
            "the new text is in place"
        );
    });
}

#[gpui::test]
fn an_unchanged_replacement_costs_nothing(cx: &mut TestAppContext) {
    let buffer = buffer(cx);
    let mut sheet = Sheet::new(buffer.clone());
    cx.update(|cx| sheet.insert_before(None, vec![item("a", "first\n")], cx));

    let edited = edits(&buffer, cx, |cx| {
        sheet.replace(&"a".to_owned(), item("a", "first\n"), cx);
    });

    assert!(edited.is_empty(), "an identical item is not re-rendered");
    assert!(sheet.last_painted_buckets().is_empty());
}

#[gpui::test]
fn removing_an_item_leaves_its_neighbours_anchored(cx: &mut TestAppContext) {
    let buffer = buffer(cx);
    let mut sheet = Sheet::new(buffer.clone());
    cx.update(|cx| {
        sheet.insert_before(
            None,
            vec![
                item("a", "first\n"),
                item("b", "second\n"),
                item("c", "third\n"),
            ],
            cx,
        );
    });
    let before = sheet.range_of(&"a".to_owned()).expect("first is there");
    let after = cx.update(|cx| {
        let snapshot = buffer.read(cx).snapshot();
        snapshot.anchor_after(snapshot.text().find("third").expect("third is there"))
    });

    cx.update(|cx| sheet.remove(&"b".to_owned(), cx));

    cx.update(|cx| {
        let snapshot = buffer.read(cx).snapshot();
        assert_eq!(snapshot.text(), "first\nthird\n");
        assert_eq!(before.start.to_offset(&snapshot), 0);
        let offset = after.to_offset(&snapshot);
        assert!(
            snapshot.text()[offset..].starts_with("third"),
            "the surviving neighbour keeps its anchor"
        );
    });
}

#[gpui::test]
fn line_metadata_follows_the_line_it_was_given_for(cx: &mut TestAppContext) {
    let buffer = buffer(cx);
    let mut sheet: Transcript<String, Class, Option<u32>> = Transcript::new(buffer.clone());
    cx.update(|cx| {
        sheet.insert_before(
            None,
            vec![
                Item::new("a".to_owned(), "first\ncontinued\n").with_lines(vec![Some(1), Some(1)]),
                Item::new("b".to_owned(), "second\n").with_lines(vec![Some(2)]),
            ],
            cx,
        );
    });
    cx.update(|cx| {
        assert_eq!(sheet.line_meta(0, cx), Some(&Some(1)));
        assert_eq!(sheet.line_meta(1, cx), Some(&Some(1)));
        assert_eq!(sheet.line_meta(2, cx), Some(&Some(2)));
        assert_eq!(sheet.key_at(0, cx), Some(&"a".to_owned()));
        assert_eq!(sheet.key_at(18, cx), Some(&"b".to_owned()));
    });
}

#[gpui::test]
fn an_attached_editor_takes_the_highlights(cx: &mut TestAppContext) {
    init_editor(cx);
    let transcript = buffer(cx);
    let multi_buffer = cx.update(|cx| {
        cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::Read);
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(0),
                transcript.clone(),
                [Point::zero()..transcript.read(cx).max_point()],
                0,
                cx,
            );
            multi_buffer
        })
    });
    let window = cx.add_window(|window, cx| {
        Editor::new(
            EditorMode::Full {
                scale_ui_elements_with_buffer_font_size: true,
                show_active_line_background: false,
                sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
            },
            multi_buffer.clone(),
            None,
            window,
            cx,
        )
    });
    let editor = window.root(cx).expect("the editor is the window's root");

    let mut sheet = Sheet::new(transcript.clone());
    cx.update(|cx| {
        sheet.insert_before(None, vec![item("a", "alice hello\n")], cx);
        sheet.attach(&editor, cx);
    });

    cx.update(|cx| {
        let highlighted = editor
            .read(cx)
            .text_highlights(Class::Author.highlight_key(0), cx)
            .map(|(_, ranges)| ranges.len())
            .unwrap_or_default();
        assert_eq!(highlighted, 1, "the author span is painted in its bucket");
    });

    cx.update(|cx| sheet.insert_before(None, vec![item("b", "bob hello\n")], cx));
    cx.update(|cx| {
        let highlighted = editor
            .read(cx)
            .text_highlights(Class::Author.highlight_key(0), cx)
            .map(|(_, ranges)| ranges.len())
            .unwrap_or_default();
        assert_eq!(highlighted, 2, "the appended item joins the same bucket");
    });

    cx.update(|cx| {
        let tinted = editor.update(cx, |editor, cx| {
            editor.clear_background_highlights(
                Class::Body
                    .background(0, cx)
                    .expect("the body class tints")
                    .0,
                cx,
            )
        });
        let ranges = tinted.expect("the tint reached the editor").1;
        assert_eq!(ranges.len(), 2, "both items' bodies are tinted");
    });
}

fn init_editor(cx: &mut TestAppContext) {
    cx.update(|cx| {
        assets::Assets.load_test_fonts(cx);
        let store = settings::SettingsStore::test(cx);
        cx.set_global(store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        editor::init(cx);
    });
}

/// The point of the row lookup: a block sits under a line of its own item.
#[gpui::test]
fn item_rows_are_where_the_item_starts(cx: &mut TestAppContext) {
    let buffer = buffer(cx);
    let mut sheet = Sheet::new(buffer.clone());
    cx.update(|cx| {
        sheet.insert_before(None, vec![item("a", "first\n"), item("b", "second\n")], cx);
    });
    cx.update(|cx| {
        let snapshot = buffer.read(cx).snapshot();
        let range = sheet.range_of(&"b".to_owned()).expect("second is there");
        assert_eq!(range.start.to_point(&snapshot).row, 1);
    });
}

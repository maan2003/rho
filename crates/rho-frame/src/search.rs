//! Search in whatever is in view. `/` and `?` ask `search:` in the
//! minibuffer and put the point on the next match after it, or the one
//! before; `n` and `N` go on in the same direction or back. Every surface
//! that is a buffer searches the same way, because the frame does it on
//! the surface's editor rather than each surface on its own.
//!
//! vim's own search reaches for a Zed pane's search bar, which the frame
//! has none of, so the frame answers the keys itself.

use std::ops::Range;
use std::rc::Rc;

use editor::scroll::Autoscroll;
use editor::{Editor, HighlightKey, SelectionEffects};
use gpui::{Context, Entity, Window};
use multi_buffer::{MultiBufferOffset, ToOffset as _};
use rho_window::style::StyleClass;
use text::Bias;

use crate::frame::Frame;

/// The last search, which `n` and `N` repeat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Search {
    query: String,
    backwards: bool,
}

impl Frame {
    /// `/`, or `?` when `backwards`: asks for the words.
    pub(crate) fn search(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = match backwards {
            false => "search:",
            true => "search back:",
        };
        self.open_prompt(
            prompt,
            Rc::new(|_, _, _| Vec::new()),
            Rc::new(move |frame, query, window, cx| {
                if query.is_empty() {
                    return;
                }
                let search = Search { query, backwards };
                frame.search = Some(search.clone());
                frame.find_match(&search, true, window, cx);
            }),
            window,
            cx,
        );
    }

    /// `n`, or `N` when `reverse`: the last search again from the point.
    pub(crate) fn search_again(
        &mut self,
        reverse: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(search) = self.search.clone() else {
            self.echo("search: nothing searched yet", StyleClass::SystemInfo, cx);
            return;
        };
        let search = Search {
            backwards: search.backwards != reverse,
            ..search
        };
        self.find_match(&search, false, window, cx);
    }

    fn find_match(
        &mut self,
        search: &Search,
        first: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = self.active_editor(cx);
        let Found {
            matches,
            at,
            wrapped,
        } = found(&editor, search, first, cx);
        let Some(at) = at else {
            editor.update(cx, |editor, cx| {
                editor.clear_highlights(HighlightKey::BufferSearchHighlights, cx);
            });
            self.echo(
                &format!("search: no {}", search.query),
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let anchors = matches
                .iter()
                .map(|range| {
                    snapshot.anchor_after(MultiBufferOffset(range.start))
                        ..snapshot.anchor_before(MultiBufferOffset(range.end))
                })
                .collect::<Vec<_>>();
            let selected = anchors[at].clone();
            editor.highlight_text(
                HighlightKey::BufferSearchHighlights,
                anchors,
                StyleClass::UserMessage.resolve(cx),
                cx,
            );
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::center()),
                window,
                cx,
                |selections| selections.select_anchor_ranges([selected]),
            );
        });
        let echo = match wrapped {
            true => format!(
                "search: {} of {}, from the other end",
                at + 1,
                matches.len()
            ),
            false => format!("search: {} of {}", at + 1, matches.len()),
        };
        self.echo(&echo, StyleClass::SystemInfo, cx);
    }
}

struct Found {
    matches: Vec<Range<usize>>,
    at: Option<usize>,
    wrapped: bool,
}

fn found(editor: &Entity<Editor>, search: &Search, first: bool, cx: &mut Context<Frame>) -> Found {
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let snapshot = display.buffer_snapshot();
        let point = editor
            .selections
            .newest_anchor()
            .start
            .to_offset(snapshot)
            .0;
        // The text is the buffer's source, which holds what the reader
        // cannot see: a link's address behind its label, the markup a
        // parse conceals, a folded preview. A match there would put the
        // point nowhere on screen, so only one that starts where the
        // reader can see it counts.
        let shown = |offset: usize| {
            let at = snapshot.offset_to_point(MultiBufferOffset(offset));
            display
                .display_point_to_point(display.point_to_display_point(at, Bias::Left), Bias::Left)
                == at
        };
        let matches = matches(&snapshot.text(), &search.query)
            .into_iter()
            .filter(|found| shown(found.start))
            .collect::<Vec<_>>();
        let editable = |offset| {
            Editor::point_is_editable(
                snapshot,
                snapshot.offset_to_point(MultiBufferOffset(offset)),
            )
        };
        let draft = first && !snapshot.is_singleton() && editable(point);
        let selection = if draft {
            next_in_draft(&matches, point, search.backwards, editable).or_else(|| {
                // The history precedes the draft. The first history match
                // can be reached directly going back, but going forward
                // from the draft wraps into it.
                next(&matches, point, search.backwards)
            })
        } else {
            next(&matches, point, search.backwards)
        };
        let (at, wrapped) = match selection {
            Some((at, wrapped)) => (Some(at), wrapped),
            None => (None, false),
        };
        Found {
            matches,
            at,
            wrapped,
        }
    })
}

/// Where `query` is in `text`. Case does not count unless the query has a
/// capital in it, as in Helix and in vim's `smartcase`.
fn matches(text: &str, query: &str) -> Vec<Range<usize>> {
    let exact = query.chars().any(char::is_uppercase);
    let same = |a: char, b: char| match exact {
        true => a == b,
        false => a.to_lowercase().eq(b.to_lowercase()),
    };
    let mut found = Vec::new();
    let mut starts = text.char_indices();
    while let Some((start, _)) = starts.next() {
        let mut rest = text[start..].char_indices();
        let mut end = None;
        let mut wanted = query.chars();
        loop {
            match (wanted.next(), rest.next()) {
                (None, Some((at, _))) => {
                    end = Some(start + at);
                    break;
                }
                (None, None) => {
                    end = Some(text.len());
                    break;
                }
                (Some(want), Some((_, got))) if same(want, got) => {}
                _ => break,
            }
        }
        if let Some(end) = end {
            found.push(start..end);
            // Matches do not overlap: the next one starts after this one.
            while starts.offset() < end {
                starts.next();
            }
        }
    }
    found
}

/// The match after `point`, or before it when `backwards`, going round
/// the end when there is none that way; and whether it went round.
fn next(matches: &[Range<usize>], point: usize, backwards: bool) -> Option<(usize, bool)> {
    let last = matches.len().checked_sub(1)?;
    Some(match backwards {
        false => match matches.iter().position(|found| found.start > point) {
            Some(at) => (at, false),
            None => (0, true),
        },
        true => match matches.iter().rposition(|found| found.start < point) {
            Some(at) => (at, false),
            None => (last, true),
        },
    })
}

/// Start in the draft even if the point is after its last match. Only
/// `n`/`N` advance into the rest of the buffer after the first selection.
fn next_in_draft(
    matches: &[Range<usize>],
    point: usize,
    backwards: bool,
    editable: impl Fn(usize) -> bool,
) -> Option<(usize, bool)> {
    let draft = matches
        .iter()
        .enumerate()
        .filter(|(_, hit)| editable(hit.start));
    if backwards {
        draft
            .clone()
            .rfind(|(_, hit)| hit.start <= point)
            .or_else(|| draft.last())
    } else {
        draft
            .clone()
            .find(|(_, hit)| hit.start >= point)
            .or_else(|| draft.into_iter().next())
    }
    .map(|(index, _)| (index, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said<'a>(text: &'a str, query: &str) -> Vec<&'a str> {
        matches(text, query)
            .into_iter()
            .map(|range| &text[range])
            .collect()
    }

    #[test]
    fn initial_search_stays_in_the_draft_without_reporting_a_wrap() {
        let matches = [3..6, 20..23, 32..35];
        let draft = |at| at >= 15;
        assert_eq!(next_in_draft(&matches, 39, false, draft), Some((1, false)));
        assert_eq!(next_in_draft(&matches, 24, false, draft), Some((2, false)));
        assert_eq!(next_in_draft(&matches, 16, true, draft), Some((2, false)));
        assert_eq!(next_in_draft(&matches[..1], 19, false, draft), None);
        assert_eq!(next(&matches, 32, false), Some((0, true)));
    }

    #[test]
    fn lower_case_finds_any_case_and_a_capital_finds_only_itself() {
        assert_eq!(
            said("Deploy, deploy, DEPLOY", "deploy"),
            ["Deploy", "deploy", "DEPLOY"]
        );
        assert_eq!(said("Deploy, deploy, DEPLOY", "Deploy"), ["Deploy"]);
    }

    #[test]
    fn matches_are_byte_ranges_of_the_text_whatever_its_letters() {
        // `é` is two bytes and `ü` folds from `Ü`: the ranges must still
        // land on the text's own boundaries.
        let text = "café ÜBER über";
        assert_eq!(matches(text, "über"), [6..11, 12..17]);
        assert_eq!(said(text, "über"), ["ÜBER", "über"]);
        assert_eq!(said("aaaa", "aa"), ["aa", "aa"], "matches do not overlap");
        assert_eq!(said("ends on it", "it"), ["it"], "a match may end the text");
    }

    #[test]
    fn next_goes_on_from_the_point_and_round_the_end() {
        let found = [2..4, 10..12, 20..22];
        // On a match, the next one is the one after it, either way.
        assert_eq!(next(&found, 10, false), Some((2, false)));
        assert_eq!(next(&found, 10, true), Some((0, false)));
        // Between matches.
        assert_eq!(next(&found, 5, false), Some((1, false)));
        assert_eq!(next(&found, 5, true), Some((0, false)));
        // Past the last one forward, and before the first one back.
        assert_eq!(next(&found, 20, false), Some((0, true)));
        assert_eq!(next(&found, 2, true), Some((2, true)));
        assert_eq!(next(&[], 0, false), None);
    }
}

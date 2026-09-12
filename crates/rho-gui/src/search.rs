//! Buffer search: what was searched for, which way it runs, and where the
//! next match is.
//!
//! One query for the whole workspace, the way vim's search register is one
//! for the whole editor: it follows the reader from surface to surface, and
//! each surface searches its own buffer with it. That is why this is not a
//! transcript's state or the dashboard's — both search, with the same
//! query, and `n` in one continues what `/` typed in the other.

use gpui::{App, Entity};
use rho_ui_proto::AgentId;

/// Which way a search runs. Not a `bool`: three call sites in a row read
/// `backwards`, and the one that repeats a search in the other direction
/// read `backwards != reverse`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Forward,
    Backward,
}

impl Direction {
    /// Which way an editor's search request runs. The event carries a
    /// `backwards` flag; this is the last place that word appears.
    pub(crate) fn of(backwards: bool) -> Self {
        if backwards {
            Self::Backward
        } else {
            Self::Forward
        }
    }

    /// The other way, for `N`.
    pub(crate) fn reversed(self) -> Self {
        match self {
            Self::Forward => Self::Backward,
            Self::Backward => Self::Forward,
        }
    }

    /// What the minibuffer asks for.
    pub(crate) fn prompt(self) -> &'static str {
        match self {
            Self::Forward => "search:",
            Self::Backward => "search backward:",
        }
    }

    /// What is said when a search runs off one end and starts from the
    /// other, so a reader who is suddenly somewhere else knows why.
    pub(crate) fn wrap_notice(self) -> &'static str {
        match self {
            Self::Forward => "search: wrapped to the top",
            Self::Backward => "search: wrapped to the bottom",
        }
    }
}

/// A search: the text and the way it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Query {
    pub(crate) text: String,
    pub(crate) direction: Direction,
}

/// A transcript search waiting for the history it has to look through to be
/// composed. It belongs to one agent: another agent's transcript arriving
/// first is not what it was waiting for.
pub(crate) struct Pending {
    pub(crate) agent: AgentId,
    pub(crate) query: Query,
}

/// Where a match starts, and whether the search went round the end of the
/// buffer to reach it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Match {
    pub(crate) start: usize,
    pub(crate) wrapped: bool,
}

/// The workspace's search register, and the one search that is waiting.
#[derive(Default)]
pub(crate) struct Search {
    last: Option<Query>,
    pending: Option<Pending>,
}

impl Search {
    /// The last search anyone ran, so `n` and `N` have something to repeat.
    pub(crate) fn last(&self) -> Option<&Query> {
        self.last.as_ref()
    }

    /// Records a search that found something. A query that matched nothing
    /// is not recorded: repeating it would only fail again.
    pub(crate) fn record(&mut self, query: Query) {
        self.last = Some(query);
    }

    /// Holds a search until the transcript it looks through is composed.
    pub(crate) fn wait_for(&mut self, pending: Pending) {
        self.pending = Some(pending);
    }

    /// Takes the waiting search if this is the agent it was waiting for,
    /// and otherwise leaves it waiting.
    pub(crate) fn take_waiting_for(&mut self, agent: AgentId) -> Option<Query> {
        if self.pending.as_ref().is_some_and(|it| it.agent == agent) {
            self.pending.take().map(|it| it.query)
        } else {
            None
        }
    }
}

/// Where the point is in an editor, as an offset into its text — which is
/// what a search counts from. The start of the selection rather than its
/// head, because a search leaves the match selected and vim's point in that
/// state is the match's first character: `n` from there finds the next
/// match and `N` the previous one, rather than the one already under the
/// point.
pub(crate) fn point_offset(editor: &Entity<editor::Editor>, cx: &mut App) -> usize {
    editor.update(cx, |editor, cx| {
        editor
            .selections
            .newest::<editor::MultiBufferOffset>(&editor.display_snapshot(cx))
            .start
            .0
    })
}

/// The next match for `query` from the point, wrapping once around the
/// buffer. A search that always started at the top could not be repeated:
/// `n` would land on the same match forever. Forward searches start after
/// the point so a repeat moves on; backward searches end before it, for the
/// same reason.
pub(crate) fn find(text: &str, query: &Query, from: usize) -> Option<Match> {
    if query.text.is_empty() || text.is_empty() {
        return None;
    }
    match query.direction {
        Direction::Backward => {
            // Every match that begins before the point, nearest first — not
            // `rfind` over the text before it, which would miss a match the
            // point is standing in the middle of.
            if let Some((start, _)) = text
                .match_indices(&query.text)
                .take_while(|(start, _)| *start < from)
                .last()
            {
                return Some(Match {
                    start,
                    wrapped: false,
                });
            }
            text.rfind(&query.text).map(|start| Match {
                start,
                wrapped: true,
            })
        }
        Direction::Forward => {
            let after = ceil_boundary(text, from.saturating_add(1));
            if let Some(offset) = text[after..].find(&query.text) {
                return Some(Match {
                    start: after + offset,
                    wrapped: false,
                });
            }
            text.find(&query.text).map(|start| Match {
                start,
                wrapped: true,
            })
        }
    }
}

/// The nearest character boundary at or above `index`, so a point that sits
/// inside a multi-byte character never splits one when the text is sliced
/// there.
fn ceil_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(text: &str, direction: Direction) -> Query {
        Query {
            text: text.to_owned(),
            direction,
        }
    }

    fn found(start: usize, wrapped: bool) -> Option<Match> {
        Some(Match { start, wrapped })
    }

    /// A forward search starts after the point, so repeating it moves on
    /// rather than finding the match the point is already sitting on.
    #[test]
    fn a_forward_search_starts_after_the_point() {
        let text = "one two one two";
        assert_eq!(
            find(text, &query("one", Direction::Forward), 0),
            found(8, false)
        );
        assert_eq!(
            find(text, &query("two", Direction::Forward), 0),
            found(4, false)
        );
    }

    /// A backward search ends before the point, for the same reason.
    #[test]
    fn a_backward_search_ends_before_the_point() {
        let text = "one two one two";
        let back = query("one", Direction::Backward);
        assert_eq!(find(text, &back, 8), found(0, false));
        assert_eq!(find(text, &back, 9), found(8, false));
        assert_eq!(find(text, &back, 0), found(8, true));
    }

    /// Running off the end wraps once and says that it did.
    #[test]
    fn a_search_wraps_once_around_the_buffer() {
        let text = "alpha beta";
        assert_eq!(
            find(text, &query("alpha", Direction::Forward), 6),
            found(0, true)
        );
        assert_eq!(
            find(text, &query("beta", Direction::Backward), 6),
            found(6, true)
        );
        assert_eq!(find(text, &query("gamma", Direction::Forward), 0), None);
        assert_eq!(find(text, &query("", Direction::Forward), 0), None);
    }

    /// A point inside a multi-byte character is a point on a boundary as
    /// far as the search is concerned; slicing there would panic.
    #[test]
    fn a_search_from_inside_a_character_does_not_split_it() {
        let text = "→ turn → turn";
        assert_eq!(
            find(text, &query("turn", Direction::Forward), 1),
            found(4, false)
        );
        assert_eq!(
            find(text, &query("turn", Direction::Backward), 10),
            found(4, false)
        );
    }

    /// A search waiting on one agent's transcript is not answered by
    /// another agent's arriving first.
    #[test]
    fn a_waiting_search_belongs_to_the_agent_it_waits_for() {
        let agent = |counter| {
            AgentId::from_counter(counter, &rho_ui_proto::AgentIdDomain(0))
                .expect("a counter in the domain is an id")
        };
        let (mine, theirs) = (agent(1), agent(2));
        let mut search = Search::default();
        search.wait_for(Pending {
            agent: mine,
            query: query("needle", Direction::Forward),
        });
        assert_eq!(search.take_waiting_for(theirs), None);
        assert_eq!(
            search.take_waiting_for(mine),
            Some(query("needle", Direction::Forward))
        );
        assert_eq!(search.take_waiting_for(mine), None);
    }
}

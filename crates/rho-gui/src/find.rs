//! Find: one prompt over every node's full path.
//!
//! The reader knows what a thing is called and roughly where it sits, not
//! which surface it lives on, so there is one prompt rather than one per
//! kind: agents, pages, topics and Slack conversations all arrive as a
//! path (`nixos › poco on linux`) and `enter` opens whichever surface that
//! path names.
//!
//! Matching is fzf-style: a subsequence, scored so that matches on word
//! and path-segment starts win. That is what makes `nixpoco` find
//! `nixos › poco on linux` while a query that only lands mid-word ranks
//! below it.
//!
//! `find_candidates` is the single seam onto the tree. Slice 2 swaps what
//! it yields from a path string to a `NodeId` without the prompt or the
//! scorer noticing.

use gpui::{App, Context, Window};
use rho_core::AgentId;

use crate::minibuffer::Candidate;
use crate::registry::HostId;
use crate::workspace::Workspace;

/// A match is worth this much before bonuses.
const MATCH: i32 = 16;
/// Skipping characters costs, so a tight match beats a scattered one.
const GAP_START: i32 = -3;
const GAP_EXTENSION: i32 = -1;
/// First character of a word (after a space, a dash, an underscore).
const BONUS_BOUNDARY: i32 = 8;
/// First character of a path segment, worth more than a word start: the
/// segment is how the reader remembers where a thing lives.
const BONUS_SEGMENT: i32 = 12;
const BONUS_CAMEL: i32 = 6;
/// Adjacent matches, which is what makes a typed prefix beat initials.
const BONUS_CONSECUTIVE: i32 = 8;
/// The query's first character weighs double, so `p` prefers the path that
/// starts with it.
const BONUS_FIRST_MULTIPLIER: i32 = 2;

/// Characters that end a path segment.
const SEPARATORS: [char; 3] = ['›', '/', '>'];
/// Characters that end a word inside a segment.
const DELIMITERS: [char; 6] = ['-', '_', '.', ':', '#', '@'];

/// What the finder opens when a path is chosen.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum FindTarget {
    Agent(AgentId),
    Page(rho_browser::PageId),
    /// A heading: opens its first agent, or a draft under it, exactly as
    /// `enter` on the dashboard row does.
    Topic {
        host: HostId,
        node_id: rho_desk::NodeId,
    },
    Slack(rho_slack::session::Source),
}

/// One findable node: the path the reader matches against, what opening it
/// means, and how recently it was used.
pub(crate) struct FindCandidate {
    pub path: String,
    pub kind: &'static str,
    pub target: FindTarget,
    /// Unix milliseconds of the last use, for ranking equal matches. Zero
    /// where nothing records a use.
    pub recency: i64,
}

/// The bonus a match at `index` earns from what precedes it.
fn bonus_at(chars: &[char], index: usize) -> i32 {
    let Some(previous) = index.checked_sub(1).map(|index| chars[index]) else {
        // The start of the path is the start of its first segment.
        return BONUS_SEGMENT;
    };
    if previous.is_whitespace() {
        // A separator carries through the space beside it: in `a › poco`
        // the `p` starts a segment, not merely a word.
        let before = chars[..index - 1]
            .iter()
            .rev()
            .find(|character| !character.is_whitespace());
        return match before {
            Some(character) if SEPARATORS.contains(character) => BONUS_SEGMENT,
            _ => BONUS_BOUNDARY,
        };
    }
    if SEPARATORS.contains(&previous) {
        return BONUS_SEGMENT;
    }
    if DELIMITERS.contains(&previous) {
        return BONUS_BOUNDARY;
    }
    if previous.is_lowercase() && chars[index].is_uppercase() {
        return BONUS_CAMEL;
    }
    0
}

/// Scores `query` against `path`, or `None` when the query is not a
/// subsequence of it. An empty query matches everything at zero.
pub(crate) fn score(path: &str, query: &str) -> Option<i32> {
    // Whitespace in the query is the reader's own spacing, not part of
    // what they are looking for: `nix poco` and `nixpoco` are one query.
    let query = query
        .chars()
        .filter(|character| !character.is_whitespace())
        .map(lower)
        .collect::<Vec<_>>();
    if query.is_empty() {
        return Some(0);
    }
    let chars = path.chars().collect::<Vec<_>>();
    let folded = chars.iter().copied().map(lower).collect::<Vec<_>>();
    let bonuses = (0..chars.len())
        .map(|index| bonus_at(&chars, index))
        .collect::<Vec<_>>();

    // `previous[index]` is the best score of an alignment of the query so
    // far whose last character matched at `index`.
    let mut previous: Option<Vec<Option<i32>>> = None;
    for (position, needle) in query.iter().enumerate() {
        let mut row = vec![None; chars.len()];
        for index in 0..chars.len() {
            if folded[index] != *needle {
                continue;
            }
            let multiplier = if position == 0 {
                BONUS_FIRST_MULTIPLIER
            } else {
                1
            };
            let here = MATCH + bonuses[index] * multiplier;
            row[index] = match &previous {
                None => Some(here),
                Some(previous) => previous[..index]
                    .iter()
                    .enumerate()
                    .filter_map(|(earlier, score)| {
                        let score = (*score)?;
                        let gap = (index - earlier - 1) as i32;
                        Some(if gap == 0 {
                            score + here + BONUS_CONSECUTIVE
                        } else {
                            score + here + GAP_START + GAP_EXTENSION * (gap - 1)
                        })
                    })
                    .max(),
            };
        }
        if row.iter().all(Option::is_none) {
            return None;
        }
        previous = Some(row);
    }
    previous?.into_iter().flatten().max()
}

fn lower(character: char) -> char {
    character.to_lowercase().next().unwrap_or(character)
}

/// Orders `(path, recency)` pairs against a query, best match first and
/// most recently used among equals, returning indices into the input.
/// Paths the query does not match are dropped.
pub(crate) fn rank(candidates: &[(String, i64)], query: &str) -> Vec<usize> {
    let mut scored = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, (path, recency))| Some((index, score(path, query)?, *recency)))
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| right.2.cmp(&left.2))
            .then_with(|| candidates[left.0].0.cmp(&candidates[right.0].0))
    });
    scored.into_iter().map(|(index, _, _)| index).collect()
}

/// Slack conversations and threads as findable paths. A thread hangs
/// under the conversation it is in, which is how the reader names it.
fn slack_candidates(
    rows: Vec<rho_slack::model::ConversationRow>,
    threads: Vec<(rho_slack::types::ThreadKey, rho_slack::model::ThreadCard)>,
) -> Vec<FindCandidate> {
    let millis = |ts: &rho_slack::types::Ts| (ts.epoch_seconds() * 1000.0) as i64;
    let mut candidates = Vec::new();
    for row in rows {
        candidates.push(FindCandidate {
            path: format!("slack › {}", row.label),
            kind: "conversation",
            recency: row.latest.as_ref().map_or(0, millis),
            target: FindTarget::Slack(rho_slack::session::Source::Conversation(row.id)),
        });
    }
    for (key, card) in threads {
        candidates.push(FindCandidate {
            path: format!("slack › {} › {}", card.conversation, card.summary),
            kind: "thread",
            recency: millis(&card.verdict_key),
            target: FindTarget::Slack(rho_slack::session::Source::Thread(key)),
        });
    }
    candidates
}

impl Workspace {
    /// Every node the finder can reach, as its full path and what opening
    /// it means. The one seam onto the tree: slice 2 changes what a target
    /// carries, not the prompt.
    pub(crate) fn find_candidates(&self, cx: &App) -> Vec<FindCandidate> {
        let mut candidates = self.dashboard.find_candidates(&self.registry, cx);
        candidates.extend(self.slack_find_candidates(cx));
        candidates
    }

    /// Slack's side of the tree: one path per conversation, and one per
    /// thread the client is tracking.
    fn slack_find_candidates(&self, cx: &App) -> Vec<FindCandidate> {
        let Some(session) = &self.slack else {
            return Vec::new();
        };
        let session = session.read(cx);
        let model = session.model();
        let now = crate::inbox::now_ms();
        let threads = model
            .tracked()
            .into_iter()
            .filter_map(|key| {
                let card = model.card(&key, now)?;
                Some((key, card))
            })
            .collect::<Vec<_>>();
        slack_candidates(session.rows(), threads)
    }

    /// The finder itself: type a path, `enter` opens it.
    pub(crate) fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let complete = std::rc::Rc::new(|workspace: &Workspace, input: &str, cx: &App| {
            let candidates = workspace.find_candidates(cx);
            let paths = candidates
                .iter()
                .map(|candidate| (candidate.path.clone(), candidate.recency))
                .collect::<Vec<_>>();
            rank(&paths, input)
                .into_iter()
                .filter_map(|index| candidates.get(index))
                .take(FIND_LIMIT)
                .map(|candidate| Candidate {
                    value: candidate.path.clone(),
                    description: candidate.kind.to_owned(),
                })
                .collect()
        });
        let on_submit = std::rc::Rc::new(
            |workspace: &mut Workspace,
             input: String,
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                workspace.find_open(&input, window, cx);
            },
        );
        self.open_prompt("find:", complete, on_submit, window, cx);
        if let Some(minibuffer) = &mut self.minibuffer {
            // A path has spaces in it, so completion replaces the whole
            // input rather than the last word.
            minibuffer.set_complete_whole_input();
        }
    }

    /// Opens the target the chosen path names, the ordinary way each
    /// surface is opened from the dashboard.
    fn find_open(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let path = path.trim();
        if path.is_empty() {
            return;
        }
        let candidates = self.find_candidates(cx);
        // The reader may have submitted a query rather than completing a
        // row, so the best-ranked match is what they asked for.
        let paths = candidates
            .iter()
            .map(|candidate| (candidate.path.clone(), candidate.recency))
            .collect::<Vec<_>>();
        let exact = candidates
            .iter()
            .position(|candidate| candidate.path == path);
        let Some(index) = exact.or_else(|| rank(&paths, path).first().copied()) else {
            self.notice_on(
                None,
                &format!("nothing matching `{path}`"),
                crate::style::StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        match candidates[index].target.clone() {
            FindTarget::Agent(agent_id) => self.open_agent(agent_id, window, cx),
            FindTarget::Page(page_id) => self.open_browser_page(page_id, window, cx),
            FindTarget::Topic { host, node_id } => {
                match self.dashboard.first_tree_agent_for_topic((host, node_id)) {
                    Some(agent_id) => self.open_agent(agent_id, window, cx),
                    None => {
                        self.dashboard
                            .open_new_tree_draft((host, node_id), window, cx);
                        self.dashboard_focus_draft(window, cx);
                    }
                }
            }
            FindTarget::Slack(source) => self.open_slack_source(source, window, cx),
        }
    }
}

/// The prompt shows a window of candidates; ranking past that is work the
/// reader never sees.
const FIND_LIMIT: usize = 50;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_of_initials_finds_the_path_it_names() {
        assert!(
            score("nixos › poco on linux", "nixpoco").is_some(),
            "`nixpoco` must be a subsequence of the path"
        );
        let paths = [
            ("nixos › poco on linux".to_owned(), 0),
            ("nix › personal notes › cocoa".to_owned(), 0),
            ("poems › nothing to index".to_owned(), 0),
        ];
        assert_eq!(
            rank(&paths, "nixpoco").first().copied(),
            Some(0),
            "the path whose segments the query spells must rank first"
        );
    }

    #[test]
    fn a_segment_start_beats_the_same_letters_mid_word() {
        let inside = score("alpha › apocope", "poco").expect("matches inside the word");
        let start = score("alpha › poco", "poco").expect("matches at the segment start");
        assert!(
            start > inside,
            "segment start {start} must beat mid-word {inside}"
        );
    }

    #[test]
    fn a_word_start_beats_a_letter_inside_a_word() {
        let inside = score("recent", "rn").expect("matches inside the word");
        let start = score("release notes", "rn").expect("matches at the word start");
        assert!(
            start > inside,
            "word start {start} must beat mid-word {inside}"
        );
    }

    #[test]
    fn a_query_that_is_not_a_subsequence_does_not_match() {
        assert_eq!(score("nixos › poco on linux", "zzz"), None);
        assert_eq!(
            score("nixos › poco", "ocon"),
            None,
            "order matters: the letters must appear in the query's order"
        );
    }

    #[test]
    fn matching_ignores_case_and_the_readers_own_spacing() {
        assert!(score("Release Notes", "rn").is_some());
        assert_eq!(
            score("nixos › poco on linux", "nix poco"),
            score("nixos › poco on linux", "nixpoco"),
            "a space in the query is spacing, not a character to match"
        );
    }

    #[test]
    fn equal_matches_are_ordered_by_recency_of_use() {
        let paths = [
            ("desk › notes".to_owned(), 10),
            ("desk › notes".to_owned(), 900),
        ];
        assert_eq!(
            rank(&paths, "notes"),
            vec![1, 0],
            "the more recently used of two equal matches comes first"
        );
    }

    #[test]
    fn slack_conversations_and_threads_are_paths_like_any_other_node() {
        use rho_slack::model::{ConversationRow, ThreadCard, Waiting};
        use rho_slack::types::{ChannelId, ThreadKey, Ts};

        let key = ThreadKey {
            workspace: rho_slack::config::WorkspaceName("acme".to_owned()),
            channel: ChannelId::from("C1"),
            thread_ts: Ts::from("100.000000"),
        };
        let candidates = slack_candidates(
            vec![ConversationRow {
                id: ChannelId::from("C1"),
                label: "#design".to_owned(),
                unread: false,
                mention_count: 0,
                latest: Some(Ts::from("120.000000")),
            }],
            vec![(
                key.clone(),
                ThreadCard {
                    key: key.clone(),
                    conversation: "#design".to_owned(),
                    summary: "release date".to_owned(),
                    waiting: Waiting::OnYou,
                    wait_days: 0.0,
                    verdict_key: Ts::from("140.000000"),
                },
            )],
        );
        let paths = candidates
            .iter()
            .map(|candidate| (candidate.path.as_str(), candidate.kind))
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                ("slack › #design", "conversation"),
                ("slack › #design › release date", "thread"),
            ]
        );
        assert_eq!(
            candidates[1].target,
            FindTarget::Slack(rho_slack::session::Source::Thread(key)),
            "the thread's path must open the thread, not its channel"
        );
        assert!(
            score(&candidates[1].path, "desrel").is_some(),
            "a thread is findable by its channel and its summary at once"
        );
    }
}

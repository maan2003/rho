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
//! A query is its words, not its letters. Each word is matched on its own
//! and every one of them must land somewhere, but they need not land on
//! the same name: `rho agent foo` finds the agent `foo` filed under
//! `rho/agent`, with `rho` and `agent` landing on the label path and `foo`
//! on the name. One subsequence over the whole query could never do that,
//! because the space in the query has no space in the name to land on.
//! A word that is a label path entire — `rho/agent` — is the reader
//! naming a filing rather than spelling a thing, so everything filed
//! there is what they asked for, agents first and newest first.
//!
//! `find_candidates` is the single seam onto the tree. Slice 2 swaps what
//! it yields from a path string to a `NodeId` without the prompt or the
//! scorer noticing.

use gpui::{App, Context, Window};
use rho_agents::HostId;
use rho_core::AgentId;

use crate::minibuffer::Candidate;
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

/// A word that is exactly a label path is the reader naming the filing,
/// not spelling letters that happen to be in it. It outweighs anything an
/// incidental subsequence can earn, so `rho/agent` lists what is filed
/// under `rho/agent` rather than everything containing those letters.
const BONUS_LABEL_PATH: i32 = 4096;

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
        node_id: rho_desk::cells::Id,
    },
    Slack(rho_slack::session::Source),
}

/// One findable node: the path the reader matches against, what opening it
/// means, and how recently it was used.
pub(crate) struct FindCandidate {
    pub path: String,
    pub kind: &'static str,
    pub target: FindTarget,
    /// The same thing named by each label it carries, `rho/agent › name`.
    /// The reader remembers the filing as often as the place, so a label
    /// path finds a thing exactly as its parent path does.
    pub labels: Vec<LabelName>,
    /// Names the query matches but the row never shows: an agent's tag and
    /// the last thing the user said to it. The reader looks for what they
    /// remember, which is rarely the title something ended up with.
    pub aka: Vec<String>,
    /// Unix milliseconds of the last use, for ranking equal matches. Zero
    /// where nothing records a use.
    pub recency: i64,
}

/// A thing named by one of its labels: the filing path on its own, and the
/// name that path makes. The path is kept apart from the name because a
/// query that is the path entire means something the letters do not.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LabelName {
    pub path: String,
    pub name: String,
}

impl LabelName {
    pub(crate) fn new(path: &str, title: &str) -> Self {
        Self {
            path: path.to_owned(),
            name: format!("{path} › {title}"),
        }
    }
}

/// One name a query is matched against, and the label path it belongs to
/// when it is a label's.
#[derive(Clone, Debug)]
pub(crate) struct FindName {
    text: String,
    label_path: Option<String>,
}

impl FindName {
    fn plain(text: String) -> Self {
        Self {
            text,
            label_path: None,
        }
    }
}

impl FindCandidate {
    #[cfg(test)]
    pub(crate) fn names_for_test(&self) -> Vec<String> {
        self.names().into_iter().map(|name| name.text).collect()
    }

    /// Every name the query is matched against, the shown path first.
    fn names(&self) -> Vec<FindName> {
        let mut names = vec![FindName::plain(self.path.clone())];
        names.extend(self.labels.iter().map(|label| FindName {
            text: label.name.clone(),
            label_path: Some(label.path.clone()),
        }));
        names.extend(self.aka.iter().cloned().map(FindName::plain));
        names
    }
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

// How much work one keystroke in the finder actually did.
//
// A keystroke has two halves and both can grow with the desk, so both are
// counted, separately, because they grow for different reasons and a sum
// hides the smaller one: building a candidate — the node itself and each
// ancestor its breadcrumb walks to the root — and scoring one, where a
// step is a visit to a cell of the alignment. Summed, a scan introduced
// into the build is a third of a total the scoring dominates and passes a
// ratio that should have caught it; that is not a hypothetical, it is what
// this counter did before it was split.
//
// This is the machine's own work: a busy machine does not change it.
//
// Thread-local because the suite runs tests concurrently in one process: a
// shared counter reads as one test's work plus its neighbours', which is a
// number that cannot be wrong in any way you can see.
#[cfg(test)]
thread_local! {
    static WALK_STEPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static SCORE_STEPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// One node visited, or one ancestor of it, while the candidate set is
/// built.
#[cfg(test)]
pub(crate) fn charge_walk(steps: usize) {
    WALK_STEPS.with(|counted| counted.set(counted.get() + steps as u64));
}

#[cfg(not(test))]
pub(crate) fn charge_walk(_steps: usize) {}

#[cfg(test)]
fn charge_score(steps: usize) {
    SCORE_STEPS.with(|counted| counted.set(counted.get() + steps as u64));
}

#[cfg(not(test))]
fn charge_score(_steps: usize) {}

/// What the finder has done since this was last asked: the candidate set
/// walked, and the matcher run.
#[cfg(test)]
pub(crate) fn take_find_steps() -> (u64, u64) {
    (
        WALK_STEPS.with(|counted| counted.replace(0)),
        SCORE_STEPS.with(|counted| counted.replace(0)),
    )
}

/// Scores `query` against `path`, or `None` when the query is not a
/// subsequence of it. An empty query matches everything at zero.
pub(crate) fn score(path: &str, query: &str) -> Option<i32> {
    // A term never carries whitespace by the time it is here — the query
    // was split on it — but a caller with one word and a stray space is
    // asking the same question, so it is dropped rather than failed.
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
        // One row is one pass over the path, whether or not any character
        // of it matches.
        charge_score(chars.len());
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
                // A match after the first looks back over every earlier
                // alignment: the part that is not linear in the path, and
                // the part a cheaper scorer would remove.
                Some(previous) => {
                    charge_score(index);
                    previous[..index]
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
                        .max()
                }
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
    let names = candidates
        .iter()
        .map(|(path, recency)| (vec![path.clone()], *recency))
        .collect::<Vec<_>>();
    rank_names(&names, query)
}

/// What one word of the query is worth against one name, with the filing
/// counted: a word that is exactly this name's label path is the reader
/// naming the filing, and that is worth more than the letters.
fn score_name(name: &FindName, term: &str) -> Option<i32> {
    let score = score(&name.text, term)?;
    let named_the_filing = name
        .label_path
        .as_deref()
        .is_some_and(|path| path.eq_ignore_ascii_case(term));
    Some(if named_the_filing {
        score + BONUS_LABEL_PATH
    } else {
        score
    })
}

/// What a whole query is worth against everything one thing is called.
///
/// Every word must land, but they need not land on the same name: the
/// filing can answer one word and the title another, which is how a reader
/// who remembers `rho/agent` and `foo` finds the thing that is both. Each
/// word takes the best name it can find and the words are summed, so a
/// thing that answers two words beats a thing that answers one twice.
pub(crate) fn score_terms(names: &[FindName], query: &str) -> Option<i32> {
    let mut total = 0;
    let mut any = false;
    for term in query.split_whitespace() {
        any = true;
        total += names
            .iter()
            .filter_map(|name| score_name(name, term))
            .max()?;
    }
    // An empty query matches everything at zero, as one word of nothing did.
    if !any {
        return Some(0);
    }
    Some(total)
}

/// `rank` where a thing has more than one name: the best-scoring name is
/// the thing's score for each word of the query, so a label path competes
/// with the parent path rather than replacing it. Ties break on the first
/// name, which is the path the row shows.
pub(crate) fn rank_names(candidates: &[(Vec<String>, i64)], query: &str) -> Vec<usize> {
    let named = candidates
        .iter()
        .map(|(names, recency)| {
            (
                names
                    .iter()
                    .cloned()
                    .map(FindName::plain)
                    .collect::<Vec<_>>(),
                *recency,
            )
        })
        .collect::<Vec<_>>();
    rank_find_names(&named, query)
}

/// The ranking itself, over names that know their filing.
pub(crate) fn rank_find_names(candidates: &[(Vec<FindName>, i64)], query: &str) -> Vec<usize> {
    let mut scored = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, (names, recency))| {
            let best = score_terms(names, query)?;
            Some((index, best, *recency))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| right.2.cmp(&left.2))
            .then_with(|| {
                candidates[left.0]
                    .0
                    .first()
                    .map(|name| &name.text)
                    .cmp(&candidates[right.0].0.first().map(|name| &name.text))
            })
    });
    scored.into_iter().map(|(index, _, _)| index).collect()
}

/// Slack conversations and threads as findable paths. A thread hangs
/// under the conversation it is in, which is how the reader names it.
pub(crate) fn slack_candidates(
    rows: Vec<rho_slack::model::ConversationRow>,
    threads: Vec<(
        rho_slack::types::ThreadKey,
        rho_slack::model::UnitCard,
        String,
    )>,
) -> Vec<FindCandidate> {
    let millis = |ts: &rho_slack::types::Ts| (ts.epoch_seconds() * 1000.0) as i64;
    let mut candidates = Vec::new();
    for row in rows {
        candidates.push(FindCandidate {
            path: format!("slack › {}", row.label),
            kind: "conversation",
            recency: row.latest.as_ref().map_or(0, millis),
            target: FindTarget::Slack(rho_slack::session::Source::Conversation(row.id)),
            labels: Vec::new(),
            aka: Vec::new(),
        });
    }
    for (key, card, title) in threads {
        candidates.push(FindCandidate {
            path: format!("slack › {} › {title}", card.conversation),
            kind: "thread",
            recency: millis(&card.newest),
            target: FindTarget::Slack(rho_slack::session::Source::Thread(key)),
            labels: Vec::new(),
            aka: Vec::new(),
        });
    }
    candidates
}

impl Workspace {
    /// Every node the finder can reach, as its full path and what opening
    /// it means. The one seam onto the tree: slice 2 changes what a target
    /// carries, not the prompt.
    pub(crate) fn find_candidates(&self, cx: &App) -> Vec<FindCandidate> {
        let mut candidates =
            crate::candidates::find_candidates(&self.desk_cells, &self.registry, cx);
        let mut slack = self.slack_find_candidates(cx);
        // A Slack room is findable because Slack says it exists rather than
        // because the tree holds a row for it, so its labels are joined on
        // here instead of coming down with the node.
        if let Some(host) = self.hosts.owner() {
            let paths = self
                .desk_cells
                .label_paths(host)
                .into_iter()
                .collect::<std::collections::HashMap<_, _>>();
            let workspace_name = self
                .slack
                .session()
                .map(|session| session.read(cx).model().workspace().clone());
            for candidate in &mut slack {
                let FindTarget::Slack(source) = &candidate.target else {
                    continue;
                };
                let Some(name) = &workspace_name else {
                    continue;
                };
                let unit = crate::slack::unit_of_source(name, source);
                let Some(facts) = self
                    .desk_cells
                    .facts(host, &rho_desk::cells::Id::Slack(unit))
                else {
                    continue;
                };
                let title = candidate.path.clone();
                candidate.labels = facts
                    .labels
                    .iter()
                    .filter_map(|label| paths.get(label))
                    .map(|path| LabelName::new(path, &title))
                    .collect();
            }
        }
        candidates.extend(slack);
        candidates
    }

    /// What the finder's `row`th match for `query` opens. Two agents can
    /// share a name, and so a path: which of them the reader highlighted
    /// is the row, never the text.
    pub(crate) fn find_target_at(&self, query: &str, row: usize) -> Option<FindTarget> {
        self.find_snapshot
            .as_ref()?
            .ranked(query)
            .into_iter()
            .nth(row)
            .map(|candidate| candidate.target.clone())
    }

    /// Slack's side of the tree: one path per conversation, and one per
    /// thread the client is tracking.
    fn slack_find_candidates(&self, cx: &App) -> Vec<FindCandidate> {
        let Some(session) = self.slack.session() else {
            return Vec::new();
        };
        let session = session.read(cx);
        let model = session.model();
        let now = chrono::Utc::now().timestamp_millis();
        // Only followed threads are paths of their own here: a conversation
        // is already one row above, and listing its unit again would put the
        // same room in the finder twice.
        let threads = model
            .tracked()
            .into_iter()
            .filter_map(|unit| {
                let root = unit.thread.clone()?;
                let card = model.card(&unit, now)?;
                let title = session.unit_summary(&unit);
                Some((model.key(&unit.channel, &root), card, title))
            })
            .collect::<Vec<_>>();
        slack_candidates(session.rows(), threads)
    }

    /// What one keystroke in the finder costs: the candidates rebuilt and
    /// ranked, which is what the completion closure below does on every
    /// character typed.
    #[cfg(test)]
    pub(crate) fn find_rows_for_test(&self, input: &str, cx: &App) -> Vec<Candidate> {
        self.find_rows_over_for_test(Vec::new(), input, cx)
    }

    /// What the picker's open costs: the whole candidate set and the names
    /// it will be ranked by, which is the frame the reader pays for.
    #[cfg(test)]
    pub(crate) fn find_snapshot_for_test(
        &self,
        slack: Vec<FindCandidate>,
        cx: &App,
    ) -> FindSnapshot {
        let mut candidates = self.find_candidates(cx);
        candidates.extend(slack);
        FindSnapshot::of(candidates)
    }

    /// What a keystroke costs: the ranking, over a set already in hand.
    #[cfg(test)]
    pub(crate) fn find_rows_in_for_test(snapshot: &FindSnapshot, input: &str) -> Vec<Candidate> {
        snapshot.rows(input)
    }

    /// The same keystroke with `slack` standing in for what a connected
    /// session would have contributed.
    ///
    /// A Slack workspace's rooms are a large part of what the finder ranks,
    /// and standing up a real session — a client, a mirror, a socket — to
    /// count them would measure the session rather than the finder. So the
    /// rows are handed in and everything after them is the real path:
    /// `find_candidates` builds the desk's half exactly as the completion
    /// closure does, the two halves are concatenated the same way, and the
    /// ranking is the ranking.
    #[cfg(test)]
    pub(crate) fn find_rows_over_for_test(
        &self,
        slack: Vec<FindCandidate>,
        input: &str,
        cx: &App,
    ) -> Vec<Candidate> {
        let mut candidates = self.find_candidates(cx);
        candidates.extend(slack);
        FindSnapshot::of(candidates).rows(input)
    }

    /// The finder itself: type a path, `enter` opens it.
    pub(crate) fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Everything there is to find, taken here and held by the prompt's
        // own closures, so it lives exactly as long as the prompt does and
        // no keystroke rebuilds it. The reader cannot file a note or start
        // an agent while the finder is up, so a snapshot is not stale: what
        // it holds is what there was when they asked.
        let snapshot = std::rc::Rc::new(FindSnapshot::of(self.find_candidates(cx)));
        let held = snapshot.clone();
        self.find_snapshot = Some(snapshot);
        let complete =
            std::rc::Rc::new(move |_: &Workspace, input: &str, _: &App| held.rows(input));
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
        let snapshot = self.find_snapshot.take();
        // The row the reader highlighted, when they chose one: rows can
        // share a path, so the text alone would always open the first.
        if let Some(target) = self.pending_find_target.take() {
            self.open_find_target(target, window, cx);
            return;
        }
        let path = path.trim();
        if path.is_empty() {
            return;
        }
        // The reader may have submitted a query rather than completing a
        // row, so the best-ranked match is what they asked for — ranked
        // over the same set the rows they were looking at came from, not
        // over a set built again underneath them.
        let Some(target) = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.best_for(path))
            .cloned()
        else {
            self.notice_on(
                None,
                &format!("nothing matching `{path}`"),
                rho_window::style::StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        self.open_find_target(target, window, cx);
    }

    /// What the row the reader chose names, opened the ordinary way each
    /// surface is opened from the dashboard.
    fn open_find_target(
        &mut self,
        target: FindTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            FindTarget::Agent(agent_id) => self.open_agent(agent_id, window, cx),
            FindTarget::Page(page_id) => self.open_browser_page(page_id, window, cx),
            // A node opens its own surface, and a note's surface is the
            // note: the staffed-heading shortcut belonged to the desk,
            // where a heading had nowhere else to go.
            FindTarget::Topic { host, node_id } => {
                self.open_note(host, node_id, window, cx);
            }
            FindTarget::Slack(source) => self.open_slack_source(source, window, cx),
        }
    }
}

/// The finder's rows for a query, best first. This is the list the prompt
/// shows, so the row a reader highlighted is this list's nth.
/// What the finder has to choose from, taken once when the picker opens.
///
/// A keystroke ranks and draws, and nothing else. Building the candidate
/// set is a pass over the desk and over every Slack unit rho tracks, and
/// doing it per character made every character cost the workspace; it is
/// done once, here, and each keystroke reads what is already in hand.
///
/// The names go with it. Nothing a candidate is matched by can change while
/// the prompt is open, so the string each query is scored against is made
/// once rather than cloned out of the candidate on every character.
pub(crate) struct FindSnapshot {
    candidates: Vec<FindCandidate>,
    names: Vec<(Vec<FindName>, i64)>,
}

impl FindSnapshot {
    fn of(candidates: Vec<FindCandidate>) -> Self {
        let names = candidates
            .iter()
            .map(|candidate| (candidate.names(), candidate.recency))
            .collect();
        Self { candidates, names }
    }

    /// The best matches for `query`, best first, in the order the prompt
    /// shows them.
    fn ranked(&self, query: &str) -> Vec<&FindCandidate> {
        self.order(query)
            .into_iter()
            .map(|index| &self.candidates[index])
            .collect()
    }

    /// The order the prompt shows, as indices.
    ///
    /// A query that is a label path and nothing else is a different
    /// question from a query of letters: the reader named a filing, so the
    /// answer is what is filed there, and among things filed together an
    /// agent is what they came for and the newest one first. The scorer
    /// already puts that set on top; this says how the set itself reads.
    fn order(&self, query: &str) -> Vec<usize> {
        let mut order = rank_find_names(&self.names, query);
        if let Some(path) = self.bare_label_path(query) {
            let under = |index: &usize| self.filed_under(*index, &path);
            let mut filed = order
                .iter()
                .copied()
                .filter(|index| under(index))
                .collect::<Vec<_>>();
            filed.sort_by(|left, right| {
                self.is_agent(*right)
                    .cmp(&self.is_agent(*left))
                    .then_with(|| {
                        self.candidates[*right]
                            .recency
                            .cmp(&self.candidates[*left].recency)
                    })
                    .then_with(|| {
                        self.candidates[*left]
                            .path
                            .cmp(&self.candidates[*right].path)
                    })
            });
            let rest = order.into_iter().filter(|index| !under(index));
            filed.extend(rest);
            order = filed;
        }
        order.truncate(FIND_LIMIT);
        order
    }

    /// The label path a query names entire, when it names one at all.
    fn bare_label_path(&self, query: &str) -> Option<String> {
        let mut terms = query.split_whitespace();
        let term = terms.next()?;
        if terms.next().is_some() {
            return None;
        }
        self.candidates
            .iter()
            .flat_map(|candidate| candidate.labels.iter())
            .find(|label| label.path.eq_ignore_ascii_case(term))
            .map(|label| label.path.clone())
    }

    fn filed_under(&self, index: usize, path: &str) -> bool {
        self.candidates[index]
            .labels
            .iter()
            .any(|label| label.path.eq_ignore_ascii_case(path))
    }

    fn is_agent(&self, index: usize) -> bool {
        matches!(self.candidates[index].target, FindTarget::Agent(_))
    }

    /// The label path a row matched on, when a label answered the query
    /// better than the place the thing sits. The reader asked by filing,
    /// so the row says which filing it came back for.
    fn matched_label(&self, index: usize, query: &str) -> Option<String> {
        let candidate = &self.candidates[index];
        let here = score_terms(&[FindName::plain(candidate.path.clone())], query);
        let mut best: Option<(i32, &LabelName)> = None;
        for label in &candidate.labels {
            let name = FindName {
                text: label.name.clone(),
                label_path: Some(label.path.clone()),
            };
            if let Some(score) = score_terms(&[name], query)
                && best.as_ref().is_none_or(|(held, _)| score > *held)
            {
                best = Some((score, label));
            }
        }
        let (score, label) = best?;
        (here.is_none_or(|here| score > here)).then(|| label.path.clone())
    }

    /// The rows the prompt draws for `query`.
    fn rows(&self, query: &str) -> Vec<Candidate> {
        self.order(query)
            .into_iter()
            .map(|index| {
                let candidate = &self.candidates[index];
                let description = match self.matched_label(index, query) {
                    Some(path) => format!("{} · {path}", candidate.kind),
                    None => candidate.kind.to_owned(),
                };
                Candidate {
                    value: candidate.path.clone(),
                    description,
                }
            })
            .collect()
    }

    /// What a submitted query names, when the reader typed rather than
    /// chose a row: an exact path if there is one, else the best match.
    fn best_for(&self, path: &str) -> Option<&FindTarget> {
        let exact = self
            .candidates
            .iter()
            .position(|candidate| candidate.path == path);
        let index = exact.or_else(|| self.order(path).first().copied())?;
        Some(&self.candidates[index].target)
    }
}

/// The prompt shows a window of candidates; ranking past that is work the
/// reader never sees.
const FIND_LIMIT: usize = 50;

/// The old whole-of-it call, kept for the tests that check ranking itself.
#[cfg(test)]
fn ranked_find_candidates(candidates: Vec<FindCandidate>, query: &str) -> Vec<FindCandidate> {
    let snapshot = FindSnapshot::of(candidates);
    let order = snapshot.order(query);
    let mut candidates = snapshot
        .candidates
        .into_iter()
        .map(Some)
        .collect::<Vec<_>>();
    order
        .into_iter()
        .take(FIND_LIMIT)
        .filter_map(|index| candidates[index].take())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_row(path: &str, id: u64) -> FindCandidate {
        FindCandidate {
            path: path.to_owned(),
            kind: "agent",
            target: FindTarget::Agent(
                AgentId::from_counter(id, &rho_ui_proto::AgentIdDomain(0)).unwrap(),
            ),
            labels: Vec::new(),
            aka: Vec::new(),
            recency: 0,
        }
    }

    /// An agent filed under a label, named by the place it sits and by
    /// the filing it carries.
    fn filed_agent(path: &str, id: u64, label: &str, recency: i64) -> FindCandidate {
        let title = path.rsplit(" › ").next().unwrap_or(path);
        FindCandidate {
            labels: vec![LabelName::new(label, title)],
            recency,
            ..agent_row(path, id)
        }
    }

    /// The reader types what they remember: the filing, then the name. The
    /// two are not one subsequence — there is no space in `rho/agent › foo`
    /// for the space in `rho agent foo` to land on — so a query is its
    /// words, each landing wherever it can among the thing's names.
    #[test]
    fn a_query_of_words_lands_them_on_whichever_name_answers() {
        let candidates = vec![
            filed_agent("rig › foo", 1, "rho/agent", 10),
            filed_agent("rig › bar", 2, "rho/agent", 20),
        ];
        let rows = ranked_find_candidates(candidates, "rho agent foo");
        let paths = rows.iter().map(|row| row.path.as_str()).collect::<Vec<_>>();
        assert_eq!(
            paths.first().copied(),
            Some("rig › foo"),
            "the filing answered two words and the name the third, got {paths:?}"
        );
        assert_eq!(
            rows.len(),
            1,
            "`foo` lands on nothing `bar` is called, so `bar` is not a match"
        );
    }

    /// A label path typed alone is the reader naming a filing rather than
    /// spelling letters. Everything filed there is the answer, agents
    /// first and the newest agent before the older one, and a thing that
    /// merely contains those letters is not in front of them.
    #[test]
    fn a_bare_label_path_lists_what_is_filed_under_it() {
        let candidates = vec![
            FindCandidate {
                recency: 99,
                ..agent_row("notes › rho/agenting", 3)
            },
            filed_agent("rig › older", 1, "rho/agent", 10),
            filed_agent("rig › newer", 2, "rho/agent", 20),
            FindCandidate {
                kind: "topic",
                labels: vec![LabelName::new("rho/agent", "the topic")],
                target: FindTarget::Topic {
                    host: HostId::default(),
                    node_id: rho_desk::cells::Id::Note(rho_desk::cells::Uuid([7; 16])),
                },
                recency: 40,
                ..agent_row("rig › the topic", 4)
            },
        ];
        let rows = ranked_find_candidates(candidates, "rho/agent");
        let paths = rows.iter().map(|row| row.path.as_str()).collect::<Vec<_>>();
        assert_eq!(
            &paths[..3],
            &["rig › newer", "rig › older", "rig › the topic"],
            "what is filed there comes first, agents by recency, got {paths:?}"
        );
        assert_eq!(
            paths.get(3).copied(),
            Some("notes › rho/agenting"),
            "a thing that only spells the letters ranks below the filing"
        );
    }

    /// The row says which filing it came back for, because the reader
    /// asked by the filing and the path the row shows is somewhere else.
    #[test]
    fn a_row_found_by_its_label_shows_that_label() {
        let snapshot = FindSnapshot::of(vec![filed_agent("rig › foo", 1, "rho/agent", 10)]);
        let rows = snapshot.rows("rho agent foo");
        assert_eq!(
            rows.first().map(|row| row.description.as_str()),
            Some("agent · rho/agent"),
            "the label path the query matched on is not on the row, got {rows:?}"
        );
        let plain = snapshot.rows("foo");
        assert_eq!(
            plain.first().map(|row| row.description.as_str()),
            Some("agent"),
            "a query the place answered says nothing about a filing"
        );
    }

    /// Two agents can be called the same thing, so the rows the prompt
    /// shows can carry the same path. The row is what tells them apart:
    /// picking the second must open the second, not the first again.
    #[test]
    fn rows_that_share_a_path_are_told_apart_by_their_row() {
        let rows = ranked_find_candidates(
            vec![
                agent_row("rig › flaky test", 1),
                agent_row("rig › flaky test", 2),
            ],
            "flaky",
        );
        assert_eq!(rows.len(), 2, "both rows are shown");
        assert_eq!(rows[0].target, agent_row("", 1).target);
        assert_eq!(rows[1].target, agent_row("", 2).target);
    }

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
        use rho_slack::model::{ConversationRow, Unit, UnitCard, Waiting};
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
                unread_count: 0,
                muted: false,
                latest: Some(Ts::from("120.000000")),
            }],
            vec![(
                key.clone(),
                UnitCard {
                    unit: Unit::thread(&key.channel, &key.thread_ts),
                    title: "release date".to_owned(),
                    conversation: "#design".to_owned(),
                    attention: None,
                    waiting: Waiting::OnYou,
                    wait_days: 0.0,
                    first_seen_ms: 0,
                    newest: Ts::from("140.000000"),
                    newest_from_other: None,
                    others_replied: false,
                },
                "release date".to_owned(),
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

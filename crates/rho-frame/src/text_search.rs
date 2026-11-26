//! Search snapshots for `Places`. Sources own their documents; a keystroke
//! hands the worker only shared tree roots and resolves only displayed hits.
use std::cmp::Reverse;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use memchr::memmem::Finder;
use rho_agent_types::{EndpointId, MessageId};
use rho_dealer::Address;
use sum_tree::TreeMap;

use crate::matching::{best_hit, text_marks};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextKind {
    Chat,
    Note,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextScope {
    All,
    Text(Option<TextKind>),
}

pub struct TextQuery<'a> {
    pub(crate) scope: TextScope,
    pub(crate) text: &'a str,
    pub(crate) terms: Vec<Finder<'a>>,
}

impl<'a> TextQuery<'a> {
    /// The caller supplies trimmed, lowercased input.
    pub fn parse(input: &'a str) -> Self {
        let body = input.strip_prefix('/');
        let text = body.unwrap_or(input).trim_start();
        let (scope, text) = if let Some(words) = text.strip_prefix("chat:") {
            (TextScope::Text(Some(TextKind::Chat)), words)
        } else if let Some(words) = text.strip_prefix("note:") {
            (TextScope::Text(Some(TextKind::Note)), words)
        } else if body.is_some() {
            (TextScope::Text(None), text)
        } else {
            (TextScope::All, input)
        };
        let text = text.trim();
        Self {
            scope,
            text,
            terms: text.split_whitespace().map(Finder::new).collect(),
        }
    }

    pub(crate) fn wants_text(&self) -> bool {
        !self.terms.is_empty()
    }

    fn includes(&self, kind: TextKind) -> bool {
        self.wants_text()
            && match self.scope {
                TextScope::All | TextScope::Text(None) => true,
                TextScope::Text(Some(only)) => only == kind,
            }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextTarget {
    Draft(Address),
    Message {
        host: EndpointId,
        message: MessageId,
    },
    Note(Address),
}

#[derive(Debug, PartialEq, Eq)]
struct Document {
    text: String,
    folded: String,
    pairs: [u64; 16],
    time: u64,
}

impl Document {
    fn new(text: String, time: u64) -> Self {
        let folded = if text.is_ascii() {
            text.to_ascii_lowercase()
        } else {
            text.to_lowercase()
        };
        let pairs = byte_pairs(folded.as_bytes());
        Self {
            text,
            folded,
            pairs,
            time,
        }
    }

    fn find(&self, pattern: &Pattern<'_>) -> Option<(u32, usize)> {
        // An adjacent byte pair absent anywhere in the document makes a
        // match impossible. This is only a reject filter (pairs can collide
        // or occur elsewhere); the same-line match below is authoritative.
        if pattern
            .pairs
            .iter()
            .any(|&(slot, mask)| self.pairs[slot] & mask != mask)
        {
            return None;
        }
        self.folded.lines().enumerate().find_map(|(row, line)| {
            let mut score = 0;
            for term in &pattern.terms {
                score += best_hit(line, term)?.0 as usize;
            }
            Some((u32::try_from(row).ok()?, score))
        })
    }
}

fn byte_pairs(bytes: &[u8]) -> [u64; 16] {
    let mut pairs = [0; 16];
    for pair in bytes.windows(2) {
        let bit = ((pair[0] as usize & 31) << 5) | (pair[1] as usize & 31);
        pairs[bit / 64] |= 1 << (bit % 64);
    }
    pairs
}

type Candidates<K> = Vec<(K, Arc<Document>)>;

struct Previous<K> {
    terms: Vec<String>,
    candidates: Arc<Candidates<K>>,
}

impl<K> Previous<K> {
    fn narrows(&self, pattern: &Pattern<'_>) -> bool {
        let terms = pattern.terms.iter().map(Finder::needle);
        // A new match also matched the previous query only if each old
        // term is a substring of some new term. Splitting a word, changing
        // its spelling, or backspacing does not necessarily narrow.
        !terms.clone().eq(self.terms.iter().map(String::as_bytes))
            && self.terms.iter().all(|old| {
                let finder = Finder::new(old);
                terms.clone().any(|new| finder.find(new).is_some())
            })
    }
}

struct Pattern<'a> {
    // Test the most selective-looking term first. Ties retain query order.
    terms: Vec<Finder<'a>>,
    pairs: Vec<(usize, u64)>,
}

impl<'a> Pattern<'a> {
    fn new(query: &TextQuery<'a>) -> Self {
        let mut terms = query.terms.clone();
        terms.sort_by_key(|term| Reverse(term.needle().len()));
        let mut pairs = [0; 16];
        for term in &terms {
            for (target, required) in pairs.iter_mut().zip(byte_pairs(term.needle())) {
                *target |= required;
            }
        }
        let pairs = pairs
            .into_iter()
            .enumerate()
            .filter(|&(_, mask)| mask != 0)
            .collect();
        Self { terms, pairs }
    }
}

/// A copy-on-write projection of the current visible words. Updating the
/// owner replaces one tree path; an in-flight search keeps its old snapshot
/// and cannot publish a narrowing cache for the new version.
#[derive(Clone)]
pub struct TextIndex<K: Clone + Ord> {
    docs: TreeMap<K, Arc<Document>>,
    previous: Arc<Mutex<Option<Previous<K>>>>,
    generation: Arc<()>,
}

impl<K: Clone + Ord> Default for TextIndex<K> {
    fn default() -> Self {
        Self {
            docs: TreeMap::default(),
            previous: Arc::default(),
            generation: Arc::default(),
        }
    }
}

impl<K: Clone + Ord> PartialEq for TextIndex<K> {
    fn eq(&self, other: &Self) -> bool {
        self.docs == other.docs
    }
}

impl<K: Clone + Ord> Eq for TextIndex<K> {}

impl<K: Clone + Ord> std::fmt::Debug for TextIndex<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Search bodies can include private notes.
        f.debug_struct("TextIndex").finish_non_exhaustive()
    }
}

impl<K: Clone + Ord> TextIndex<K> {
    pub fn update(&mut self, key: K, text: String, time: u64) {
        if self
            .docs
            .get(&key)
            .is_some_and(|doc| doc.time == time && doc.text == text)
        {
            return;
        }
        self.docs.insert(key, Arc::new(Document::new(text, time)));
        self.invalidate();
    }

    fn invalidate(&mut self) {
        self.previous = Arc::default();
        self.generation = Arc::default();
    }

    pub fn remove(&mut self, key: &K) {
        if self.docs.remove(key).is_some() {
            self.invalidate();
        }
    }

    pub fn contains(&self, key: &K) -> bool {
        self.docs.contains_key(key)
    }

    pub fn body(&self, key: &K) -> Option<&str> {
        Some(&self.docs.get(key)?.text)
    }

    fn find(
        &self,
        pattern: &Pattern<'_>,
        cancelled: &AtomicBool,
        limit: usize,
    ) -> Vec<IndexHit<K>> {
        let previous = self
            .previous
            .lock()
            .unwrap()
            .as_ref()
            .filter(|last| last.narrows(pattern))
            .map(|last| last.candidates.clone());
        let mut rows = Vec::new();
        let mut candidates = Vec::new();
        let mut test = |key: &K, doc: &Arc<Document>| {
            if cancelled.load(Ordering::Relaxed) {
                return false;
            }
            if let Some((row, score)) = doc.find(pattern) {
                candidates.push((key.clone(), doc.clone()));
                let ordinal = candidates.len() - 1;
                rows.push((score, Reverse(doc.time), ordinal, row));
            }
            true
        };
        match &previous {
            Some(last) => last.iter().all(|(key, doc)| test(key, doc)),
            None => self.docs.iter().all(|(key, doc)| test(key, doc)),
        };
        if cancelled.load(Ordering::Relaxed) {
            return Vec::new();
        }

        keep_top(&mut rows, limit, |row| *row);
        let hits = rows
            .into_iter()
            .map(|(score, _, ordinal, row)| {
                let (key, doc) = &candidates[ordinal];
                IndexHit {
                    key: key.clone(),
                    doc: doc.clone(),
                    row,
                    score,
                }
            })
            .collect();
        *self.previous.lock().unwrap() = Some(Previous {
            terms: pattern
                .terms
                .iter()
                .map(|term| {
                    std::str::from_utf8(term.needle())
                        .expect("terms are UTF-8")
                        .to_owned()
                })
                .collect(),
            candidates: Arc::new(candidates),
        });
        hits
    }
}

struct IndexHit<K> {
    key: K,
    doc: Arc<Document>,
    row: u32,
    score: usize,
}

pub struct TextHit {
    pub target: TextTarget,
    pub row: u32,
    pub(crate) marks: Vec<Range<usize>>,
    score: usize,
    doc: Arc<Document>,
    generation: Arc<()>,
}

impl TextHit {
    pub fn body(&self) -> &str {
        &self.doc.text
    }

    pub fn time(&self) -> u64 {
        self.doc.time
    }

    pub fn line(&self) -> &str {
        self.doc
            .text
            .lines()
            .nth(self.row as usize)
            .expect("case folding keeps the line breaks")
            .trim()
    }

    /// The owner may have edited or hidden this document while the worker
    /// was reading its snapshot. Do not resolve a stale line in that case.
    pub fn is_from<K: Clone + Ord>(&self, index: &TextIndex<K>, key: &K) -> bool {
        Arc::ptr_eq(&self.generation, &index.generation)
            || index
                .docs
                .get(key)
                .is_some_and(|doc| Arc::ptr_eq(doc, &self.doc))
    }
}

type Source = Box<dyn FnOnce(&Pattern<'_>, &AtomicBool, usize) -> Vec<TextHit> + Send>;

/// A batch of source snapshots for one input. It contains no app entities,
/// views, or live ledger marks.
pub struct TextSearch {
    input: String,
    sources: Vec<Source>,
    limit: usize,
}

impl TextSearch {
    pub fn new(input: String, limit: usize) -> Self {
        Self {
            input,
            sources: Vec::new(),
            limit,
        }
    }

    pub fn includes(&self, kind: TextKind) -> bool {
        TextQuery::parse(&self.input).includes(kind)
    }

    pub fn add<K: Clone + Ord + Send + Sync + 'static>(
        &mut self,
        index: TextIndex<K>,
        target: impl Fn(K) -> TextTarget + Send + 'static,
    ) {
        if index.docs.is_empty() {
            return;
        }
        self.sources
            .push(Box::new(move |pattern, cancelled, limit| {
                index
                    .find(pattern, cancelled, limit)
                    .into_iter()
                    .map(|hit| TextHit {
                        target: target(hit.key),
                        row: hit.row,
                        marks: Vec::new(),
                        doc: hit.doc,
                        score: hit.score,
                        generation: index.generation.clone(),
                    })
                    .collect()
            }));
    }

    pub fn add_draft<K: Clone + Ord>(&mut self, index: &TextIndex<K>, key: &K, address: Address) {
        let Some(doc) = index.docs.get(key).cloned() else {
            return;
        };
        let generation = index.generation.clone();
        self.sources.push(Box::new(move |pattern, _, _| {
            doc.find(pattern)
                .map(|(row, score)| TextHit {
                    target: TextTarget::Draft(address),
                    row,
                    marks: Vec::new(),
                    score,
                    doc,
                    generation,
                })
                .into_iter()
                .collect()
        }));
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.limit == 0 || self.sources.is_empty()
    }

    pub fn find(self, cancelled: &AtomicBool) -> Vec<TextHit> {
        let query = TextQuery::parse(&self.input);
        let pattern = Pattern::new(&query);
        let mut rows = Vec::new();
        for source in self.sources {
            if cancelled.load(Ordering::Relaxed) {
                return Vec::new();
            }
            let start = rows.len();
            rows.extend(
                source(&pattern, cancelled, self.limit)
                    .into_iter()
                    .enumerate()
                    .map(|(offset, hit)| (start + offset, hit)),
            );
        }
        // Drafts first, then all stored excerpts together. Source order
        // and mail/note identity keep tied results deterministic.
        keep_top(&mut rows, self.limit, |(ordinal, hit)| {
            (
                !matches!(hit.target, TextTarget::Draft(_)),
                hit.score,
                Reverse(hit.time()),
                *ordinal,
            )
        });
        rows.into_iter()
            .map(|(_, mut hit)| {
                hit.marks = text_marks(hit.line(), &query);
                hit
            })
            .collect()
    }
}

fn keep_top<T, K: Ord>(rows: &mut Vec<T>, limit: usize, key: impl Fn(&T) -> K) {
    if rows.len() > limit {
        rows.select_nth_unstable_by_key(limit, &key);
        rows.truncate(limit);
    }
    rows.sort_unstable_by_key(key);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(index: &TextIndex<u64>, input: &str, limit: usize) -> Vec<TextHit> {
        let mut search = TextSearch::new(input.into(), limit);
        search.add(index.clone(), |id| {
            TextTarget::Note(Address::new("note", id))
        });
        search.find(&AtomicBool::new(false))
    }

    #[test]
    fn scopes_disjointly_select_text_sources() {
        for (input, expected) in [
            ("clock", [true, true]),
            // `>` once meant commands; commands are never in `Places`.
            ("> clock", [true, true]),
            ("/ clock", [true, true]),
            ("chat: clock", [true, false]),
            ("/ chat: clock", [true, false]),
            ("mail: clock", [true, true]),
            ("note: clock", [false, true]),
            ("/ note: clock", [false, true]),
            ("/", [false, false]),
            ("note:", [false, false]),
        ] {
            let query = TextQuery::parse(input);
            assert_eq!(
                [
                    query.includes(TextKind::Chat),
                    query.includes(TextKind::Note)
                ],
                expected,
                "{input}"
            );
        }
    }

    #[test]
    fn narrowing_uses_full_candidates_and_rechecks_the_first_matching_line() {
        let mut index = TextIndex::default();
        index.update(1, "clock\nclock behavior".into(), 3);
        index.update(2, "clock reboot".into(), 1);
        index.update(3, "clock behavior\nbehavior but only here".into(), 2);
        index.update(4, "clock\nbehavior".into(), 4);
        let broad = run(&index, "clock", 1);
        assert_eq!(broad[0].line(), "clock");
        let narrow = run(&index, "clock beh", 2);
        assert_eq!(
            narrow
                .iter()
                .map(|hit| (hit.time(), hit.row, hit.line()))
                .collect::<Vec<_>>(),
            [(3, 1, "clock behavior"), (2, 0, "clock behavior")]
        );
        assert_eq!(
            index
                .previous
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .candidates
                .len(),
            2
        );
        // A split is not a narrowing: a term can now also match before a space.
        index.update(5, "a cl oc k".into(), 5);
        assert!(run(&index, "clock", 10).iter().all(|hit| hit.time() != 5));
        assert!(run(&index, "cl oc k", 10).iter().any(|hit| hit.time() == 5));
    }

    #[test]
    fn revision_and_cancel_cannot_leave_a_false_narrowing_cache() {
        let mut index = TextIndex::default();
        index.update(1, "clock paused".into(), 9);
        index.update(2, "worker logs\n\n  ΟΣ clock".into(), 3);
        let old = run(&index, "clock", 2);
        let old_snapshot = index.clone();
        index.update(1, "clock now running".into(), 11);
        index.remove(&2);
        index.update(3, "CLOCK resumed".into(), 12);

        assert!(old[0].is_from(&old_snapshot, &1));
        assert!(!old[0].is_from(&index, &1));
        assert!(!old[1].is_from(&index, &2));
        let old_greek = run(&old_snapshot, "ος clock", 2);
        assert_eq!(old_greek[0].line(), "ΟΣ clock");
        assert_eq!(old_greek[0].marks, [0..4, 5..10]);

        let cancelled = AtomicBool::new(true);
        let query = TextQuery::parse("clock r");
        assert!(index.find(&Pattern::new(&query), &cancelled, 2).is_empty());
        assert!(index.previous.lock().unwrap().is_none());
        let hits = run(&index, "clock r", 2);
        assert_eq!(
            hits.iter().map(TextHit::line).collect::<Vec<_>>(),
            ["CLOCK resumed", "clock now running"]
        );
    }

    #[test]
    fn draft_precedes_a_newer_stored_whole_word_and_stored_hits_interleave_by_time() {
        let mut mail = TextIndex::default();
        let mut note = TextIndex::default();
        let mut draft = TextIndex::default();
        mail.update(1, "clock".into(), 30);
        mail.update(2, "clock".into(), 10);
        note.update(1, "clock".into(), 20);
        draft.update(1, "a stopwatch".into(), 0);
        let mut search = TextSearch::new("watch".into(), 3);
        search.add_draft(&draft, &1, Address::new("thread", "open"));
        assert!(matches!(
            search.find(&AtomicBool::new(false))[0].target,
            TextTarget::Draft(_)
        ));
        draft.update(1, "a clockwork".into(), 0);
        let mut search = TextSearch::new("clock".into(), 3);
        search.add_draft(&draft, &1, Address::new("thread", "open"));
        search.add(mail, |id| TextTarget::Message {
            host: EndpointId([1; 32]),
            message: MessageId(rho_agent_types::ChatSeq(id)),
        });
        search.add(note, |id| TextTarget::Note(Address::new("note", id)));
        let hits = search.find(&AtomicBool::new(false));
        assert_eq!(
            hits.iter().map(TextHit::time).collect::<Vec<_>>(),
            [0, 30, 20]
        );
    }
}

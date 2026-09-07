//! Where the reader is, and where the reader was.
//!
//! A surface is a buffer with the point in it, history is a stack, and back
//! returns the point to where it was (`RHO-WINDOW-DESIGN.md`). This is the
//! machine that holds one context's surfaces in that order. It names no
//! source type: the key is whatever identity the crate above it uses, and
//! the surface is whatever that crate keeps a place in.
//!
//! Back restores the point by *keeping* the surface, not by replaying a
//! line and a column: whatever the caller stores in an entry — an editor,
//! a view, a model — is handed back as it was, still holding its own
//! selections, scroll and folds. Nothing here remembers a position, which
//! is why nothing here can be wrong about one.
//!
//! What `S` must be. Leaving a surface drops this machine's copy of it, so
//! `S` has to be a *handle* to a view the caller keeps elsewhere, not the
//! view itself. In `rho-gui` it is `Surface`, whose every variant is a
//! refcounted `Entity`, and the context's own buffer list holds a clone;
//! dropping an entry drops a handle and nothing else, which is why leaving
//! a buffer here never kills it, exactly as in Emacs. A caller that stored
//! the only copy of a view in an entry would lose it on the way back, and
//! that is a bug in the caller, not a behaviour this machine offers.
//!
//! What it costs. A push is one `Vec::push` and one map write. Going back
//! pops, skipping entries that are no longer the newest for their key;
//! each such entry is skipped at most once in the life of the stack, so
//! back is O(1) amortised. Forgetting the thing behind a surface is one map
//! removal, whatever the stack holds. Nothing walks the stack at draw time,
//! because the caller draws `current()` and never reads the rest.
//!
//! The dedupe is the reason for the staleness rule. Showing a surface that
//! is already somewhere in the stack should not make the reader walk back
//! through it twice, and the way to get that without a scan is to leave the
//! older entry where it is and let the map say which entry is the real one.

use std::collections::HashMap;
use std::hash::Hash;

/// How many entries are tolerated per live key before the stack is rebuilt.
/// Compaction is O(entries), and reaching it needs at least as many stale
/// entries as live ones, each made by a push or a forget that paid for it.
const STALE_RATIO: usize = 2;

/// The smallest stack worth compacting. Below this the walk costs less than
/// deciding whether to do it.
const COMPACT_FLOOR: usize = 16;

struct Entry<K, S> {
    key: K,
    surface: S,
}

/// One context's viewport and the stack behind it.
///
/// The current surface is not in the stack: it is where the reader is, and
/// the stack is where the reader was.
pub struct History<K, S> {
    key: K,
    surface: S,
    entries: Vec<Entry<K, S>>,
    /// The index of the newest entry for each key. An entry whose index is
    /// not the one here is stale — a duplicate the reader has since passed,
    /// or a surface whose thing has gone — and back steps over it.
    newest: HashMap<K, usize>,
    /// How many times the stack has been rebuilt. Kept because the cost
    /// claim is about how rarely this happens, and a claim about cost that
    /// cannot be read is a claim nobody can check.
    compactions: usize,
}

impl<K: Clone + Eq + Hash, S> History<K, S> {
    /// A context begins on a surface. There is nowhere to go back to yet.
    pub fn new(key: K, surface: S) -> Self {
        Self {
            key,
            surface,
            entries: Vec::new(),
            newest: HashMap::new(),
            compactions: 0,
        }
    }

    pub fn current(&self) -> &S {
        &self.surface
    }

    pub fn current_mut(&mut self) -> &mut S {
        &mut self.surface
    }

    pub fn current_key(&self) -> &K {
        &self.key
    }

    /// Show a surface: the one on the glass goes on the stack.
    ///
    /// Showing what is already showing replaces it and pushes nothing —
    /// otherwise back would return the reader to where they already are.
    pub fn show(&mut self, key: K, surface: S) {
        if key == self.key {
            self.surface = surface;
            return;
        }
        // Where the reader is going is not also somewhere behind them: any
        // older entry for it goes stale here, which is the dedupe, done in
        // one map write instead of a scan.
        self.newest.remove(&key);
        let key = std::mem::replace(&mut self.key, key);
        let surface = std::mem::replace(&mut self.surface, surface);
        self.newest.insert(key.clone(), self.entries.len());
        self.entries.push(Entry { key, surface });
        self.compact_if_stale();
    }

    /// Back one: the nearest entry that is still the newest for its key and
    /// whose thing has not been forgotten. `None` when there is nowhere to
    /// go, and then the caller decides where a reader with no history goes.
    ///
    /// The surface being left is dropped rather than pushed. That is what
    /// makes this a stack: back returns the point to where it was, and does
    /// not add a place to come back to.
    pub fn back(&mut self) -> Option<&S> {
        while let Some(entry) = self.entries.pop() {
            if self.newest.get(&entry.key) != Some(&self.entries.len()) {
                continue;
            }
            self.newest.remove(&entry.key);
            self.key = entry.key;
            self.surface = entry.surface;
            return Some(&self.surface);
        }
        None
    }

    /// The thing behind `key` is gone: no entry for it is reachable again.
    /// One map removal, whatever the stack holds — the entries themselves
    /// are stepped over the next time back passes them.
    ///
    /// This says nothing about the current surface. A reader looking at a
    /// surface whose thing has just died is a question for the caller, which
    /// is the only one that knows where to send them.
    pub fn forget(&mut self, key: &K) {
        self.newest.remove(key);
    }

    /// Whether back can still reach `key`.
    pub fn holds(&self, key: &K) -> bool {
        self.newest.contains_key(key)
    }

    /// How many surfaces back can still reach. This walks the map, not the
    /// stack, and exists for tests and the overview — never for a frame.
    pub fn reachable(&self) -> usize {
        self.newest.len()
    }

    /// The keys back would visit, nearest first. For tests and for anything
    /// that lists history to a reader; a frame does not call it.
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.entries
            .iter()
            .enumerate()
            .rev()
            .filter(|(index, entry)| self.newest.get(&entry.key) == Some(index))
            .map(|(_, entry)| &entry.key)
    }

    /// How many times the stack has been rebuilt, for the cost note.
    pub fn compactions(&self) -> usize {
        self.compactions
    }

    /// How many entries the stack holds, stale ones included.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    fn compact_if_stale(&mut self) {
        if self.entries.len() < COMPACT_FLOOR
            || self.entries.len() <= self.newest.len().saturating_mul(STALE_RATIO)
        {
            return;
        }
        let mut kept = Vec::with_capacity(self.newest.len());
        for (index, entry) in std::mem::take(&mut self.entries).into_iter().enumerate() {
            if self.newest.get(&entry.key) == Some(&index) {
                kept.push(entry);
            }
        }
        self.newest.clear();
        for (index, entry) in kept.iter().enumerate() {
            self.newest.insert(entry.key.clone(), index);
        }
        self.entries = kept;
        self.compactions += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> History<&'static str, u32> {
        History::new("home", 0)
    }

    #[test]
    fn back_returns_the_surface_that_was_left() {
        let mut history = history();
        history.show("a", 1);
        history.show("b", 2);
        assert_eq!(history.back().copied(), Some(1));
        assert_eq!(*history.current_key(), "a");
        assert_eq!(history.back().copied(), Some(0));
        assert_eq!(history.back(), None, "home was the first surface");
    }

    /// The surface handed back is the one that was put in, not a rebuilt
    /// copy of it: this is what makes the point, the scroll and the folds
    /// survive, since the caller's view is what an entry holds.
    #[test]
    fn back_hands_back_what_was_stored() {
        let mut history = History::new("home", String::from("home as it was"));
        history.show("a", String::from("a as it was"));
        assert_eq!(history.back().map(String::as_str), Some("home as it was"));
    }

    /// Showing what is already showing is not a place to come back to.
    #[test]
    fn showing_the_current_surface_pushes_nothing() {
        let mut history = history();
        history.show("a", 1);
        history.show("a", 2);
        assert_eq!(*history.current(), 2);
        assert_eq!(history.back().copied(), Some(0));
    }

    /// A surface visited twice is passed once on the way back. Without the
    /// staleness rule this is where a scan-and-remove would be needed.
    #[test]
    fn a_repeated_surface_is_walked_back_through_once() {
        let mut history = history();
        history.show("a", 1);
        history.show("b", 2);
        history.show("a", 3);
        history.show("c", 4);
        assert_eq!(history.back().copied(), Some(3), "a, the newest one");
        assert_eq!(history.back().copied(), Some(2), "b");
        assert_eq!(
            history.back().copied(),
            Some(0),
            "home: the older a is a duplicate the reader has passed"
        );
    }

    #[test]
    fn a_forgotten_surface_is_never_shown_again() {
        let mut history = history();
        history.show("a", 1);
        history.show("b", 2);
        history.show("c", 3);
        history.forget(&"b");
        assert_eq!(history.back().copied(), Some(1), "b is gone, so a");
        assert_eq!(history.back().copied(), Some(0), "home");
        assert!(!history.holds(&"b"));
    }

    /// Every entry for a dead key goes, not just the newest, and one call
    /// does it however many times the reader had been there.
    #[test]
    fn forgetting_removes_every_entry_for_that_key() {
        let mut history = history();
        for round in 0..5 {
            history.show("dead", round);
            history.show("alive", 100 + round);
        }
        history.forget(&"dead");
        while history.back().is_some() {
            assert_ne!(*history.current_key(), "dead");
        }
    }

    /// The cost claim, at the size the note quotes: a thousand pushes touch
    /// a thousand entries between them, and walking the whole stack back
    /// steps over each stale entry once.
    #[test]
    fn a_thousand_pushes_and_a_full_walk_back_stay_linear() {
        let mut history = History::new(0usize, 0usize);
        for index in 1..=1_000 {
            history.show(index, index);
        }
        assert_eq!(history.entry_count(), 1_000);
        assert_eq!(history.reachable(), 1_000);
        assert_eq!(history.compactions(), 0, "nothing was stale, so no rebuild");
        let mut steps = 0;
        while history.back().is_some() {
            steps += 1;
        }
        assert_eq!(steps, 1_000);
    }

    /// Duplicates are what make a stack stale, and compaction is what keeps
    /// a stale stack from growing without bound. Two keys shown a thousand
    /// times each leave a stack the size of what is reachable, rebuilt a
    /// handful of times rather than on every push.
    #[test]
    fn a_stack_of_duplicates_compacts_rarely_and_stays_small() {
        let mut history = History::new("home", 0);
        for round in 0..1_000 {
            history.show("a", round);
            history.show("b", round);
        }
        // Home, and whichever of a and b the reader is not standing on:
        // the current surface is not in the stack.
        assert_eq!(history.reachable(), 2);
        assert!(
            history.entry_count() <= COMPACT_FLOOR,
            "the stack kept {} entries for three surfaces",
            history.entry_count()
        );
        // Each rebuild throws away at least half a floor's worth of stale
        // entries, so the pushes that made them are what pay for it.
        assert!(
            history.compactions() <= 2_000 / (COMPACT_FLOOR / 2),
            "rebuilt {} times in 2000 pushes",
            history.compactions()
        );
    }

    /// The numbers the design note quotes, at the size it quotes them.
    /// Entries touched is read off the stack itself: a push adds exactly
    /// one entry, a back removes exactly one, and a forget moves none.
    #[test]
    fn the_cost_numbers_in_the_note_hold_at_a_thousand_entries() {
        let mut history = History::new(0usize, 0usize);
        for index in 1..=1_000 {
            history.show(index, index);
        }
        assert_eq!(history.entry_count(), 1_000);

        // Push: one entry.
        let (entries, compactions) = (history.entry_count(), history.compactions());
        history.show(1_001, 1_001);
        assert_eq!(history.entry_count(), entries + 1);
        assert_eq!(history.compactions(), compactions, "no rebuild on a push");

        // Forget: no entry at all, only the map.
        let (entries, reachable) = (history.entry_count(), history.reachable());
        history.forget(&500);
        assert_eq!(history.entry_count(), entries, "forget walked the stack");
        assert_eq!(history.reachable(), reachable - 1);

        // Back: one entry, plus the forgotten one stepped over when it is
        // reached, and each stale entry is stepped over at most once.
        let entries = history.entry_count();
        history.back();
        assert_eq!(history.entry_count(), entries - 1);
        let mut steps = 0;
        while history.back().is_some() {
            steps += 1;
        }
        assert_eq!(steps, 999, "1001 pushed, 1 walked back, 1 forgotten");
    }

    /// The other half of the note: how often the rebuild runs. Two surfaces
    /// alternating is the worst case for staleness, and even there it is
    /// once per eight pushes, not once per push.
    #[test]
    fn compaction_runs_once_per_eight_pushes_at_worst() {
        let mut history = History::new("home", 0);
        let pushes = 2_000;
        for round in 0..pushes / 2 {
            history.show("a", round);
            history.show("b", round);
        }
        assert!(
            history.compactions() * (COMPACT_FLOOR / 2) <= pushes,
            "{} rebuilds in {pushes} pushes",
            history.compactions()
        );
    }

    /// Compaction must not change what back does — it drops what was
    /// already unreachable and nothing else.
    #[test]
    fn compaction_keeps_the_order_back_would_have_walked() {
        let mut history = History::new("home", 0);
        for round in 0..50 {
            history.show("a", round);
            history.show("b", round);
        }
        history.show("last", 999);
        let expected = history.keys().copied().collect::<Vec<_>>();
        assert_eq!(expected, vec!["b", "a", "home"]);
        let mut walked = Vec::new();
        while history.back().is_some() {
            walked.push(*history.current_key());
        }
        assert_eq!(walked, expected);
    }
}

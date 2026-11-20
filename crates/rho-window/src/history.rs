//! Where the reader is, where the reader was, and where they were going.
//!
//! The golden rule is the user's and it is TikTok's. Up goes back through
//! history. Down goes *forward* through history when there is anything
//! forward, and only when the reader is at the newest entry does down deal —
//! open the next thing that asks for attention and append it. There is no
//! other next or previous. This is the machine that holds that list; the
//! caller does the dealing, because only the caller knows what asks.
//!
//! One list, not one per context. Back walks into the context the reader
//! came from, because the reader came from there and a way back that stops
//! at a boundary they never noticed crossing is not a way back.
//! (Superseded: an earlier ruling made history per context and this machine
//! a stack. The user overruled it.)
//!
//! Back restores the point by *keeping* the surface, not by replaying a line
//! and a column: whatever the caller stores in an entry — an editor, a view,
//! a model — is handed back as it was, still holding its own selections,
//! scroll and folds. Nothing here remembers a position, which is why nothing
//! here can be wrong about one.
//!
//! What `S` must be. Leaving a surface keeps this machine's copy of it now
//! (the entry stays in the list, behind or ahead of the cursor), but a
//! forgotten or deduplicated entry is dropped, so `S` still has to be a
//! *handle* to a view the caller keeps elsewhere, not the view itself. In
//! `rho-gui` it is `Surface`, whose every variant is a refcounted `Entity`
//! and whose context's own buffer list holds a clone. A caller that stored
//! the only copy of a view in an entry would lose it, and that is a bug in
//! the caller, not a behaviour this machine offers.
//!
//! What it costs. The list is doubly linked through a slab, so an append is
//! one slot write, up is one link hop, down is one link hop, and forgetting
//! a key is one map lookup and one unlink — all O(1), none of them touching
//! the rest of the list. Nothing is ever skipped, because nothing dead is
//! ever left in the list to skip: a key that is forgotten or superseded is
//! unlinked where it stands. The one exception is the entry the reader is
//! looking at, which cannot be unlinked while they are on it and so is
//! unlinked the moment they step off.
//!
//! And no forward step is ever lost by the machine on its own. Opening
//! something new while the cursor is in the middle appends at the end and
//! moves the cursor there; the entry the reader opened it from goes to the
//! end first, just behind the new one, so back returns to the screen the
//! reader was on, not to whatever happened to be ahead of it. What was
//! ahead of the reader stays in the list, behind them, reachable by going
//! back. It does not truncate.

use std::collections::HashMap;
use std::hash::Hash;

struct Entry<K, S> {
    key: K,
    surface: S,
    /// Toward the oldest.
    prev: Option<usize>,
    /// Toward the newest.
    next: Option<usize>,
    /// Append order. Monotone along the list and never reordered, so
    /// comparing two entries' `order` says which side of the cursor an
    /// entry is on without walking to find out.
    order: u64,
}

/// The reader's history: one list across every context, with a cursor.
pub struct History<K, S> {
    slots: Vec<Option<Entry<K, S>>>,
    free: Vec<usize>,
    /// The newest entry, which is where a new open is appended.
    newest: usize,
    /// Where the reader is. Always a live slot: the list is never empty.
    cursor: usize,
    /// The live entry for each key. A key appears at most once in the list.
    live: HashMap<K, usize>,
    /// The entry under the cursor has been forgotten and goes as soon as the
    /// reader steps off it. They are still looking at it, so it cannot go
    /// now, and where a reader on a dead surface belongs is the caller's
    /// question, not this machine's.
    cursor_forgotten: bool,
    next_order: u64,
    length: usize,
    /// How many live entries are behind the cursor: how many times back can
    /// be pressed. Kept rather than counted, because counting is a walk.
    behind: usize,
}

impl<K: Clone + Eq + Hash, S> History<K, S> {
    /// History begins on a surface. There is nowhere to go, either way.
    pub fn new(key: K, surface: S) -> Self {
        let mut live = HashMap::new();
        live.insert(key.clone(), 0);
        Self {
            slots: vec![Some(Entry {
                key,
                surface,
                prev: None,
                next: None,
                order: 0,
            })],
            free: Vec::new(),
            newest: 0,
            cursor: 0,
            live,
            cursor_forgotten: false,
            next_order: 1,
            length: 1,
            behind: 0,
        }
    }

    fn entry(&self, slot: usize) -> &Entry<K, S> {
        self.slots[slot].as_ref().expect("a linked slot is filled")
    }

    fn entry_mut(&mut self, slot: usize) -> &mut Entry<K, S> {
        self.slots[slot].as_mut().expect("a linked slot is filled")
    }

    pub fn current(&self) -> &S {
        &self.entry(self.cursor).surface
    }

    pub fn current_mut(&mut self) -> &mut S {
        let cursor = self.cursor;
        &mut self.entry_mut(cursor).surface
    }

    pub fn current_key(&self) -> &K {
        &self.entry(self.cursor).key
    }

    /// Open a surface: it becomes the newest entry and the cursor is on it.
    ///
    /// Showing what is already showing replaces it and appends nothing —
    /// otherwise back would return the reader to where they already are.
    ///
    /// A key that is already in the list is removed from where it was: the
    /// reader should not walk back through the same surface twice. With the
    /// cursor in the middle, what was ahead of the reader stays in the list
    /// behind them; nothing is truncated.
    pub fn show(&mut self, key: K, surface: S) {
        if key == *self.current_key() {
            let cursor = self.cursor;
            self.entry_mut(cursor).surface = surface;
            self.cursor_forgotten = false;
            self.live.insert(key, cursor);
            return;
        }
        if let Some(slot) = self.live.remove(&key) {
            // Not the cursor: the cursor's key is the current key and this
            // one is not it.
            self.unlink(slot);
        }
        // The new entry is linked before the one being left is unlinked, so
        // the list is never momentarily empty — the reader is always
        // somewhere, which is the invariant every step relies on.
        let leaving = self.cursor;
        let leaving_is_dead = self.cursor_forgotten;
        self.cursor_forgotten = false;
        if !leaving_is_dead {
            // Back from what opens now is where the reader opened it from,
            // so the entry being left goes to the end first. What was ahead
            // of it stays where it was, now behind the reader.
            self.move_to_newest(leaving);
        }
        let order = self.next_order;
        self.next_order += 1;
        let newest = self.newest;
        let slot = self.alloc(Entry {
            key: key.clone(),
            surface,
            prev: Some(newest),
            next: None,
            order,
        });
        self.entry_mut(newest).next = Some(slot);
        self.newest = slot;
        self.live.insert(key, slot);
        self.length += 1;
        self.cursor = slot;
        self.behind = self.length - 1;
        if leaving_is_dead {
            // `unlink` takes the step off `behind` itself: the entry being
            // left is older than the one just appended.
            self.unlink(leaving);
        }
    }

    /// Up: one step back through history. `None` when the reader is at the
    /// oldest entry, and then the caller decides whether that means
    /// anything.
    pub fn back(&mut self) -> Option<&S> {
        let previous = self.entry(self.cursor).prev?;
        self.step_to(previous);
        // One fewer entry behind either way: if the entry being left was
        // dead it has gone from behind the new cursor, and if it was not it
        // is now ahead of it.
        self.behind -= 1;
        Some(self.current())
    }

    /// Down: one step forward through history. `None` means the reader is at
    /// the newest entry — which is the caller's cue to deal, and the only
    /// place dealing happens.
    pub fn forward(&mut self) -> Option<&S> {
        let next = self.entry(self.cursor).next?;
        // The entry left is behind the reader now. If it was dead,
        // `step_to` unlinks it and `unlink` takes that step off again, so
        // it is counted first: the count never goes below zero on the way.
        self.behind += 1;
        self.step_to(next);
        Some(self.current())
    }

    /// Whether down would deal rather than step.
    pub fn at_newest(&self) -> bool {
        self.entry(self.cursor).next.is_none()
    }

    /// The thing behind `key` is gone: the entry leaves the list, so no walk
    /// in either direction ever reaches it again. One map lookup and one
    /// unlink, whatever the list holds.
    ///
    /// This says nothing about the current surface. A reader looking at a
    /// surface whose thing has just died is a question for the caller, which
    /// is the only one that knows where to send them; the entry stays under
    /// them until they move.
    pub fn forget(&mut self, key: &K) {
        let Some(slot) = self.live.remove(key) else {
            return;
        };
        if slot == self.cursor {
            self.cursor_forgotten = true;
            return;
        }
        self.unlink(slot);
    }

    /// Whether a walk can still reach `key`, the current surface included.
    pub fn holds(&self, key: &K) -> bool {
        self.live.contains_key(key)
    }

    /// How many steps back the reader can take.
    pub fn behind(&self) -> usize {
        self.behind
    }

    /// How many steps forward the reader can take before down deals.
    pub fn ahead(&self) -> usize {
        self.length - self.behind - 1
    }

    /// How many entries the list holds, the current one included.
    pub fn len(&self) -> usize {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// The keys back would visit, nearest first. For tests and for anything
    /// that lists history to a reader; a frame does not call it.
    pub fn keys_back(&self) -> impl Iterator<Item = &K> {
        std::iter::successors(self.entry(self.cursor).prev, |slot| self.entry(*slot).prev)
            .map(|slot| &self.entry(slot).key)
    }

    /// The keys forward would visit, nearest first.
    pub fn keys_forward(&self) -> impl Iterator<Item = &K> {
        std::iter::successors(self.entry(self.cursor).next, |slot| self.entry(*slot).next)
            .map(|slot| &self.entry(slot).key)
    }

    /// Move the cursor to a neighbour, taking the entry it was on out of the
    /// list if that entry has been forgotten.
    /// Move the cursor to a neighbour, taking the entry it was on out of
    /// the list if that entry has been forgotten. Says whether it did.
    fn step_to(&mut self, slot: usize) -> bool {
        let leaving = self.cursor;
        let dead = self.cursor_forgotten;
        self.cursor_forgotten = false;
        self.cursor = slot;
        if dead {
            // The cursor has already moved, so the list still holds the
            // neighbour it moved to and this cannot empty it.
            self.unlink(leaving);
        }
        dead
    }

    /// Take `slot` out of the list. An entry older than the cursor was one
    /// of the steps back the reader had, so `behind` shrinks with it; the
    /// cursor's own entry is accounted for by whoever moved the cursor.
    fn unlink(&mut self, slot: usize) {
        let pivot = self.cursor;
        let (prev, next, order) = {
            let entry = self.entry(slot);
            (entry.prev, entry.next, entry.order)
        };
        if let Some(prev) = prev {
            self.entry_mut(prev).next = next;
        }
        match next {
            Some(next) => self.entry_mut(next).prev = prev,
            None => self.newest = prev.expect("the list is never empty"),
        }
        self.slots[slot] = None;
        self.free.push(slot);
        self.length -= 1;
        if slot != pivot && order < self.entry(pivot).order {
            self.behind -= 1;
        }
    }

    /// Take `slot` from where it stands and link it as the newest entry.
    /// The list keeps its length; only the order of `slot` moves.
    fn move_to_newest(&mut self, slot: usize) {
        if slot == self.newest {
            return;
        }
        let (prev, next) = {
            let entry = self.entry(slot);
            (entry.prev, entry.next)
        };
        let next = next.expect("an entry that is not the newest has one after it");
        if let Some(prev) = prev {
            self.entry_mut(prev).next = Some(next);
        }
        self.entry_mut(next).prev = prev;
        let newest = self.newest;
        self.entry_mut(newest).next = Some(slot);
        let order = self.next_order;
        self.next_order += 1;
        let entry = self.entry_mut(slot);
        entry.prev = Some(newest);
        entry.next = None;
        entry.order = order;
        self.newest = slot;
    }

    fn alloc(&mut self, entry: Entry<K, S>) -> usize {
        match self.free.pop() {
            Some(slot) => {
                self.slots[slot] = Some(entry);
                slot
            }
            None => {
                self.slots.push(Some(entry));
                self.slots.len() - 1
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> History<&'static str, u32> {
        History::new("home", 0)
    }

    fn back_keys(history: &History<&'static str, u32>) -> Vec<&'static str> {
        history.keys_back().copied().collect()
    }

    fn forward_keys(history: &History<&'static str, u32>) -> Vec<&'static str> {
        history.keys_forward().copied().collect()
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

    /// The golden rule, in one test: up then down is where you started, and
    /// down at the newest entry is where the caller deals.
    #[test]
    fn up_then_down_is_where_the_reader_started() {
        let mut history = history();
        history.show("a", 1);
        history.show("b", 2);
        history.show("c", 3);
        assert!(history.at_newest());
        assert_eq!(history.back().copied(), Some(2));
        assert_eq!(history.back().copied(), Some(1));
        assert!(!history.at_newest(), "there is somewhere forward now");
        assert_eq!(history.forward().copied(), Some(2));
        assert_eq!(history.forward().copied(), Some(3));
        assert!(history.at_newest());
        assert_eq!(
            history.forward(),
            None,
            "at the newest entry down is the caller's to deal"
        );
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
    fn showing_the_current_surface_appends_nothing() {
        let mut history = history();
        history.show("a", 1);
        history.show("a", 9);
        assert_eq!(*history.current(), 9, "the newer surface is the one shown");
        assert_eq!(history.len(), 2);
        assert_eq!(history.back().copied(), Some(0));
        assert_eq!(history.back(), None);
    }

    /// A surface opened again is not two places to walk back through: it
    /// leaves where it was and becomes the newest.
    #[test]
    fn opening_a_surface_again_moves_it_rather_than_repeating_it() {
        let mut history = history();
        history.show("a", 1);
        history.show("b", 2);
        history.show("a", 3);
        assert_eq!(back_keys(&history), ["b", "home"]);
        assert_eq!(history.len(), 3);
        assert_eq!(history.back().copied(), Some(2));
        assert_eq!(history.back().copied(), Some(0));
        assert_eq!(history.back(), None);
    }

    /// A new open with the cursor in the middle appends at the end and
    /// takes the cursor there, with the entry it was opened from just
    /// behind it: back is the screen before. Nothing is truncated, so what
    /// was ahead is still reachable, by going back past it.
    #[test]
    fn opening_with_the_cursor_in_the_middle_appends_and_never_truncates() {
        let mut history = history();
        history.show("a", 1);
        history.show("b", 2);
        history.show("c", 3);
        history.back();
        history.back();
        assert_eq!(*history.current_key(), "a");
        assert_eq!(forward_keys(&history), ["b", "c"]);

        history.show("d", 4);
        assert_eq!(*history.current_key(), "d");
        assert!(history.at_newest());
        assert_eq!(
            back_keys(&history),
            ["a", "c", "b", "home"],
            "back goes to a, where d was opened from; b and c were ahead and are now behind"
        );
        assert_eq!(history.len(), 5);
        assert_eq!((history.behind(), history.ahead()), (4, 0));
        assert_eq!(history.back().copied(), Some(1));
        assert_eq!(forward_keys(&history), ["d"]);
    }

    /// Two private remarks from one thread: the reader opens a note from
    /// the thread, goes back, opens a second note. Back from the second is
    /// the thread, not the first note.
    #[test]
    fn back_from_a_second_open_is_where_it_was_opened_from() {
        let mut history = history();
        history.show("thread", 1);
        history.show("note one", 2);
        history.back();
        history.show("note two", 3);
        assert_eq!(history.back().copied(), Some(1));
        assert_eq!(*history.current_key(), "thread");
        assert_eq!(history.back().copied(), Some(2));
        assert_eq!(history.back().copied(), Some(0));
        assert_eq!(history.back(), None);
    }

    #[test]
    fn forgetting_takes_an_entry_out_of_both_walks() {
        let mut history = history();
        history.show("a", 1);
        history.show("b", 2);
        history.show("c", 3);
        history.forget(&"b");
        assert!(!history.holds(&"b"));
        assert_eq!(back_keys(&history), ["a", "home"]);
        assert_eq!(history.back().copied(), Some(1));
        assert_eq!(forward_keys(&history), ["c"]);
        assert_eq!(history.forward().copied(), Some(3));
        assert_eq!(history.forward(), None);
    }

    /// Forgetting what the reader is looking at cannot take it out from
    /// under them, so it goes when they step off — and the walk on the far
    /// side is not disturbed by the wait.
    #[test]
    fn forgetting_the_current_surface_removes_it_when_the_reader_leaves() {
        let mut history = history();
        history.show("a", 1);
        history.show("b", 2);
        history.forget(&"b");
        assert_eq!(*history.current_key(), "b", "the reader has not moved");
        assert_eq!(history.len(), 3);
        assert_eq!(history.back().copied(), Some(1));
        assert_eq!(history.len(), 2, "b left as the reader stepped off it");
        assert_eq!(
            history.forward(),
            None,
            "and down deals rather than returning to a dead surface"
        );
        assert_eq!(history.back().copied(), Some(0));
    }

    /// Down off a forgotten entry at the oldest end: the entry goes and
    /// the reader gains no step back. The count once dipped below zero
    /// here, which is what closing a surface the reader had walked back to
    /// the start of history did.
    #[test]
    fn forward_off_a_forgotten_oldest_entry_gains_no_step_back() {
        let mut history = history();
        history.show("page", 1);
        history.show("home", 0);
        history.back();
        assert_eq!((history.behind(), history.ahead()), (0, 1));
        history.forget(&"page");
        assert_eq!(history.forward().copied(), Some(0));
        assert_eq!(
            (history.behind(), history.ahead(), history.len()),
            (0, 0, 1)
        );
        assert_eq!(history.back(), None);

        // In the middle, down off a forgotten entry keeps the steps back
        // the reader had, and no more.
        let mut history = History::new("home", 0);
        history.show("a", 1);
        history.show("b", 2);
        history.show("c", 3);
        history.back();
        history.forget(&"b");
        assert_eq!(history.forward().copied(), Some(3));
        assert_eq!((history.behind(), history.ahead()), (2, 0));
        assert_eq!(back_keys(&history), ["a", "home"]);
    }

    #[test]
    fn the_counts_say_how_far_each_way_goes() {
        let mut history = history();
        assert_eq!((history.behind(), history.ahead()), (0, 0));
        history.show("a", 1);
        history.show("b", 2);
        assert_eq!((history.behind(), history.ahead()), (2, 0));
        history.back();
        assert_eq!((history.behind(), history.ahead()), (1, 1));
        history.back();
        assert_eq!((history.behind(), history.ahead()), (0, 2));
        history.forward();
        assert_eq!((history.behind(), history.ahead()), (1, 1));
    }

    /// The counts survive an entry leaving from either side of the cursor,
    /// which is the part a walk-free count can get wrong.
    #[test]
    fn the_counts_survive_a_forget_on_either_side() {
        let mut history = history();
        history.show("a", 1);
        history.show("b", 2);
        history.show("c", 3);
        history.back();
        assert_eq!((history.behind(), history.ahead()), (2, 1));
        history.forget(&"home");
        assert_eq!(
            (history.behind(), history.ahead()),
            (1, 1),
            "one fewer step back"
        );
        history.forget(&"c");
        assert_eq!(
            (history.behind(), history.ahead()),
            (1, 0),
            "and nothing forward"
        );
        assert_eq!(history.back().copied(), Some(1));
        assert_eq!(history.back(), None);
    }

    /// A surface opened again from behind the cursor takes its place at the
    /// end, and the counts follow it.
    #[test]
    fn reopening_from_behind_the_cursor_keeps_the_counts_honest() {
        let mut history = history();
        history.show("a", 1);
        history.show("b", 2);
        history.back();
        assert_eq!((history.behind(), history.ahead()), (1, 1));
        history.show("home", 7);
        assert_eq!(*history.current(), 7);
        assert_eq!(back_keys(&history), ["a", "b"]);
        assert_eq!((history.behind(), history.ahead()), (2, 0));
    }

    /// The cost claim, read back rather than asserted: a thousand opens and
    /// a thousand steps each way leave a list with exactly the entries that
    /// were opened, and every walk is one hop.
    #[test]
    fn the_list_holds_one_entry_per_live_key() {
        let mut history = History::new(0usize, 0usize);
        for key in 1..=1_000 {
            history.show(key, key);
        }
        assert_eq!(history.len(), 1_001);
        assert_eq!(history.behind(), 1_000);
        for _ in 0..1_000 {
            assert!(history.back().is_some());
        }
        assert_eq!(history.back(), None);
        assert_eq!((history.behind(), history.ahead()), (0, 1_000));
        for _ in 0..1_000 {
            assert!(history.forward().is_some());
        }
        assert_eq!(history.forward(), None);
    }

    /// Opening the same two surfaces over and over is the case a stack
    /// would grow without bound on. The list holds two entries and a home,
    /// because a reopen moves an entry rather than adding one.
    #[test]
    fn alternating_opens_do_not_grow_the_list() {
        let mut history = history();
        for step in 0..2_000 {
            history.show(if step % 2 == 0 { "a" } else { "b" }, step);
        }
        assert_eq!(history.len(), 3);
        assert_eq!(back_keys(&history), ["a", "home"]);
    }

    /// Slots are reused, so a long-lived history does not grow its slab
    /// with every open and forget.
    #[test]
    fn forgetting_returns_a_slot_to_be_used_again() {
        let mut history = history();
        for step in 0..1_000 {
            history.show("scratch", step);
            history.forget(&"scratch");
        }
        assert!(
            history.slots.len() <= 4,
            "slab grew to {} slots",
            history.slots.len()
        );
    }
}

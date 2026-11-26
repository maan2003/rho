//! The places the reader has been, most recently visited first
//! (`window.Screen`): navigation is pure MRU. The front entry is the
//! screen in view; opening a place moves it to the front, closing one
//! hands the front to the place visited before it. There is no back and
//! forward: `Places` lists this order, `tab` flips to its second entry
//! from Desk, and `q` falls back along it.
//!
//! An entry keeps whatever the caller stores with it, so a screen visited
//! again comes back as it was left, scroll and point included: the frame
//! keeps the surface itself and nothing here remembers a position.

use std::time::{SystemTime, UNIX_EPOCH};

struct Entry<K, S> {
    key: K,
    value: S,
    /// Wall-clock time of the last visit, when known: a place restored
    /// from an old record may not say.
    visited_ms: Option<u64>,
}

/// Places by recency of visit. Never empty: the reader is always
/// somewhere.
pub struct Recent<K, S> {
    /// Newest first; `entries[0]` is in view.
    entries: Vec<Entry<K, S>>,
}

fn now_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
}

impl<K: Clone + Eq, S> Recent<K, S> {
    /// The reader starts on `key`.
    pub fn new(key: K, value: S) -> Self {
        Self {
            entries: vec![Entry {
                key,
                value,
                visited_ms: now_ms(),
            }],
        }
    }

    fn position(&self, key: &K) -> Option<usize> {
        self.entries.iter().position(|entry| &entry.key == key)
    }

    pub fn current_key(&self) -> &K {
        &self.entries[0].key
    }

    pub fn current(&self) -> &S {
        &self.entries[0].value
    }

    /// Visit `key`: it comes to the front with `value`, wherever it was.
    pub fn show(&mut self, key: K, value: S) {
        if let Some(at) = self.position(&key) {
            self.entries.remove(at);
        }
        self.entries.insert(
            0,
            Entry {
                key,
                value,
                visited_ms: now_ms(),
            },
        );
    }

    /// Visit `key` again as it was left, with what was stored for it.
    /// `false` when it is not here.
    pub fn bring(&mut self, key: &K) -> bool {
        let Some(at) = self.position(key) else {
            return false;
        };
        let mut entry = self.entries.remove(at);
        entry.visited_ms = now_ms();
        self.entries.insert(0, entry);
        true
    }

    /// `key` was visited just now, but the reader is back where they
    /// were: it goes right behind the screen in view. A place made
    /// elsewhere, such as an idea sent from its own screen, ages from
    /// here like any other.
    pub fn remember(&mut self, key: K, value: S) {
        if *self.current_key() == key {
            return;
        }
        if let Some(at) = self.position(&key) {
            self.entries.remove(at);
        }
        self.entries.insert(
            1,
            Entry {
                key,
                value,
                visited_ms: now_ms(),
            },
        );
    }

    /// `new` takes `old`'s place in the order, as the same place under a
    /// new name: a usage chart switching its window, or a queued idea
    /// becoming its thread. `false` when `old` is not here.
    pub fn replace(&mut self, old: &K, new: K, value: S) -> bool {
        if self.position(old).is_none() {
            return false;
        }
        if old != &new
            && let Some(elsewhere) = self.position(&new)
        {
            self.entries.remove(elsewhere);
        }
        let at = self.position(old).expect("still here");
        let entry = &mut self.entries[at];
        entry.key = new;
        entry.value = value;
        if at == 0 {
            entry.visited_ms = now_ms();
        }
        true
    }

    /// `key` is gone from the order. When it was in view, the place
    /// visited before it is now; the last place left stays, since the
    /// reader is always somewhere. Says whether it was removed.
    pub fn remove(&mut self, key: &K) -> bool {
        match self.position(key) {
            Some(at) if self.entries.len() > 1 => {
                self.entries.remove(at);
                if at == 0 {
                    self.entries[0].visited_ms = now_ms();
                }
                true
            }
            _ => false,
        }
    }

    /// Every place but the one in view, most recent first.
    pub fn previous(&self) -> impl Iterator<Item = &K> {
        self.entries[1..].iter().map(|entry| &entry.key)
    }

    /// Every place, the one in view first.
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.entries.iter().map(|entry| &entry.key)
    }

    /// When `key` was last visited, as far as anyone knows.
    pub fn visited_ms(&self, key: &K) -> Option<u64> {
        self.entries[self.position(key)?].visited_ms
    }

    /// Rebuilds the order from a record: `places` newest first, each
    /// with when it was visited. `None` when there are none.
    pub fn restore(places: impl IntoIterator<Item = (K, S, Option<u64>)>) -> Option<Self> {
        let mut entries: Vec<Entry<K, S>> = Vec::new();
        for (key, value, visited_ms) in places {
            if entries.iter().any(|entry| entry.key == key) {
                continue;
            }
            entries.push(Entry {
                key,
                value,
                visited_ms,
            });
        }
        (!entries.is_empty()).then_some(Self { entries })
    }

    /// Every place with when it was visited, newest first: what
    /// [`Self::restore`] reads back.
    pub fn record(&self) -> impl Iterator<Item = (&K, Option<u64>)> {
        self.entries
            .iter()
            .map(|entry| (&entry.key, entry.visited_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(recent: &Recent<&'static str, u32>) -> Vec<&'static str> {
        recent.keys().copied().collect()
    }

    #[test]
    fn a_visit_brings_a_place_to_the_front_without_repeating_it() {
        let mut recent = Recent::new("desk", 0);
        recent.show("a", 1);
        recent.show("b", 2);
        recent.show("a", 3);
        assert_eq!(order(&recent), ["a", "b", "desk"]);
        assert_eq!(*recent.current(), 3, "the newer value is kept");
        assert_eq!(
            recent.previous().copied().collect::<Vec<_>>(),
            ["b", "desk"]
        );
    }

    /// Flipping between two places never grows the order: there is no
    /// stack to pile up.
    #[test]
    fn alternating_visits_keep_two_entries() {
        let mut recent = Recent::new("desk", 0);
        for step in 0..1_000 {
            recent.show(if step % 2 == 0 { "term" } else { "page" }, step);
        }
        assert_eq!(order(&recent), ["page", "term", "desk"]);
    }

    /// Closing what is in view hands the front to the place visited
    /// before it, which is not necessarily the one opened before it.
    #[test]
    fn closing_returns_to_the_previous_visit() {
        let mut recent = Recent::new("desk", 0);
        recent.show("a", 1);
        recent.show("b", 2);
        recent.show("a", 3);
        recent.show("c", 4);
        assert!(recent.remove(&"c"));
        assert_eq!(*recent.current_key(), "a");
        assert!(recent.remove(&"b"), "a place behind can close too");
        assert_eq!(order(&recent), ["a", "desk"]);
        assert!(recent.remove(&"a"));
        assert!(!recent.remove(&"desk"), "the last place stays");
        assert_eq!(order(&recent), ["desk"]);
    }

    /// Coming back keeps what was stored with the place, unlike a fresh
    /// visit that says anew how it was reached.
    #[test]
    fn bringing_a_place_back_keeps_what_was_stored() {
        let mut recent = Recent::new("desk", 0);
        recent.show("transcript", 7);
        recent.show("page", 2);
        assert!(recent.bring(&"transcript"));
        assert_eq!(order(&recent), ["transcript", "page", "desk"]);
        assert_eq!(*recent.current(), 7);
        assert!(!recent.bring(&"gone"));
        assert_eq!(order(&recent), ["transcript", "page", "desk"]);
    }

    #[test]
    fn a_remembered_place_waits_right_behind_the_screen_in_view() {
        let mut recent = Recent::new("desk", 0);
        recent.show("thread", 1);
        recent.remember("idea", 2);
        assert_eq!(order(&recent), ["thread", "idea", "desk"]);
        // It ages like any other place.
        recent.show("page", 3);
        recent.show("term", 4);
        assert_eq!(order(&recent), ["term", "page", "thread", "idea", "desk"]);
        recent.remember("thread", 5);
        assert_eq!(order(&recent), ["term", "thread", "page", "idea", "desk"]);
        recent.remember("term", 6);
        assert_eq!(
            *recent.current(),
            4,
            "the screen in view is not remembered behind itself"
        );
    }

    #[test]
    fn a_replaced_place_keeps_its_rank() {
        let mut recent = Recent::new("desk", 0);
        recent.show("idea", 1);
        recent.show("page", 2);
        assert!(recent.replace(&"idea", "thread", 3));
        assert_eq!(order(&recent), ["page", "thread", "desk"]);
        // Switching to a name already elsewhere takes it from there.
        recent.show("usage 7d", 4);
        recent.show("usage 30d", 5);
        assert!(recent.replace(&"usage 30d", "usage 7d", 6));
        assert_eq!(order(&recent), ["usage 7d", "page", "thread", "desk"]);
        assert!(!recent.replace(&"gone", "x", 7));
    }

    #[test]
    fn a_record_reads_back_the_same_order() {
        let mut recent = Recent::new("desk", 0);
        recent.show("a", 1);
        recent.show("b", 2);
        let record: Vec<_> = recent.record().map(|(key, at)| (*key, 0, at)).collect();
        let restored = Recent::restore(record).unwrap();
        assert_eq!(order(&restored), ["b", "a", "desk"]);
        assert_eq!(restored.visited_ms(&"a"), recent.visited_ms(&"a"));
        assert!(Recent::<&str, u32>::restore([]).is_none());
        let doubled = Recent::restore([("a", 0, None), ("a", 1, None), ("b", 2, None)]).unwrap();
        assert_eq!(order(&doubled), ["a", "b"]);
    }
}

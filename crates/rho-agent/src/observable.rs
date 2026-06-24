//! A shared value with a consistent snapshot + change subscription.
//!
//! This is the only genuinely tricky concurrency in the agent, so it lives on
//! its own. It generalizes over what changes: `T` is the current state a reader
//! snapshots, `Event` is the change broadcast to followers. The conversation
//! history is an `Observable<Vec<ItemBlock>, ItemBlock>` (snapshot the vec,
//! broadcast each appended block); the agent status is an
//! `Observable<Status, Status>` (snapshot the value, broadcast the new value).
//!
//! A follower needs the current state *and* every later change, with nothing
//! lost in between and nothing delivered twice. One invariant guarantees that:
//!
//! * the writer mutates the state **and** broadcasts the event while holding
//!   the write lock;
//! * a subscriber clones the snapshot **and** opens its receiver while holding
//!   the read lock.
//!
//! The write lock is exclusive, so a mutation can never interleave a
//! subscriber's snapshot+subscribe. Any change ordered before the subscriber
//! took the read lock is already in the snapshot and — having been broadcast
//! before `subscribe()` — is not redelivered; any change after is delivered on
//! the receiver and absent from the snapshot. Reads are concurrent (`RwLock`),
//! so building a provider request never blocks a UI snapshot or vice versa.

use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use tokio::sync::broadcast;

/// Change events buffered per subscriber before a slow follower is told to
/// resync via `broadcast::error::RecvError::Lagged`.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// A shared, mutable `T` that broadcasts an `Event` on every change.
///
/// Clones share the same underlying value (so a handle and the owning loop see
/// the same state).
pub(crate) struct Observable<T, Event> {
    value: Arc<RwLock<T>>,
    events: broadcast::Sender<Event>,
}

impl<T, Event: Clone> Observable<T, Event> {
    pub(crate) fn new(initial: T) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            value: Arc::new(RwLock::new(initial)),
            events,
        }
    }

    /// Mutate the value and broadcast the change `f` describes. The mutation
    /// and the broadcast happen under the same write lock, so a subscriber
    /// can never observe one without the other. A send error just means
    /// there are no live subscribers.
    pub(crate) fn update(&self, f: impl FnOnce(&mut T) -> Event) {
        let mut value = self.write();
        let event = f(&mut value);
        let _ = self.events.send(event);
    }

    /// Borrow the current value without cloning, e.g. to build a request.
    pub(crate) fn read(&self) -> RwLockReadGuard<'_, T> {
        self.value.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, T> {
        self.value.write().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: Clone, Event: Clone> Observable<T, Event> {
    /// A cloned point-in-time copy of the value.
    pub(crate) fn snapshot(&self) -> T {
        self.read().clone()
    }

    /// The current value plus a receiver for every later change, taken
    /// atomically under the read lock so the follower misses nothing and
    /// double-counts nothing. See the module docs for why this is correct.
    pub(crate) fn subscribe(&self) -> (T, broadcast::Receiver<Event>) {
        let value = self.read();
        let receiver = self.events.subscribe();
        (value.clone(), receiver)
    }
}

impl<T: Clone> Observable<T, T> {
    /// Replace the value, broadcasting the new value as the change. The
    /// "value is also the event" shape used for a latest-wins status.
    pub(crate) fn set(&self, value: T) {
        self.update(|slot| {
            *slot = value.clone();
            value
        });
    }
}

impl<T, Event> Clone for Observable<T, Event> {
    fn clone(&self) -> Self {
        Self {
            value: Arc::clone(&self.value),
            events: self.events.clone(),
        }
    }
}

impl<T, Event> std::fmt::Debug for Observable<T, Event> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Observable").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// History-shaped: append to a vec, broadcast the appended element.
    fn push(log: &Observable<Vec<i32>, i32>, n: i32) {
        log.update(|items| {
            items.push(n);
            n
        });
    }

    #[tokio::test]
    async fn subscribe_returns_snapshot_then_streams_later_changes() {
        let log = Observable::<Vec<i32>, i32>::new(vec![1, 2]);

        let (snapshot, mut rx) = log.subscribe();
        push(&log, 3);
        push(&log, 4);

        assert_eq!(snapshot, vec![1, 2]);
        assert_eq!(rx.recv().await.unwrap(), 3);
        assert_eq!(rx.recv().await.unwrap(), 4);
    }

    #[test]
    fn change_before_subscribe_is_in_the_snapshot_not_the_stream() {
        let log = Observable::<Vec<i32>, i32>::new(Vec::new());
        push(&log, 1);

        let (snapshot, mut rx) = log.subscribe();

        // The pre-subscribe change is in the snapshot...
        assert_eq!(snapshot, vec![1]);
        // ...and is NOT redelivered on the stream (no double-apply).
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));

        // A post-subscribe change IS delivered on the stream.
        push(&log, 2);
        assert_eq!(rx.try_recv().unwrap(), 2);
    }

    #[test]
    fn snapshot_and_read_reflect_changes() {
        let log = Observable::<Vec<String>, String>::new(Vec::new());
        assert!(log.read().is_empty());

        push_str(&log, "a");
        push_str(&log, "b");

        assert_eq!(log.snapshot(), vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(log.read().len(), 2);
    }

    /// Status-shaped: replace the value, broadcast the new value. Same type.
    #[test]
    fn works_as_a_single_replaced_value() {
        let status = Observable::<i32, i32>::new(0);

        let (snapshot, mut rx) = status.subscribe();
        status.update(|value| {
            *value = 7;
            *value
        });

        assert_eq!(snapshot, 0);
        assert_eq!(status.snapshot(), 7);
        assert_eq!(rx.try_recv().unwrap(), 7);
    }

    #[test]
    fn clones_share_the_same_value() {
        let log = Observable::<Vec<i32>, i32>::new(vec![1]);
        let clone = log.clone();

        push(&clone, 2);

        assert_eq!(log.snapshot(), vec![1, 2]);
    }

    fn push_str(log: &Observable<Vec<String>, String>, s: &str) {
        log.update(|items| {
            items.push(s.to_owned());
            s.to_owned()
        });
    }
}

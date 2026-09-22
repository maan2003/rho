//! A virtual clock and timer registrations for deterministic tests.

use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
	task::Poll,
};

use super::{Instant, Timer, Timers};

/// A virtual clock for deterministic timer tests.
///
/// Clones share the clock. Advance time explicitly, then poll session or origin
/// drivers yourself to observe elapsed deadlines.
pub struct Test {
	shared: Arc<Mutex<Shared>>,
}

struct Shared {
	now: Instant,
	timers: HashMap<u64, Entry>,
	next_id: u64,
}

struct Entry {
	at: Option<Instant>,
	waiters: kio::WaiterList,
}

impl Test {
	/// A fresh runtime whose virtual clock starts at the real current instant.
	pub fn new() -> Self {
		Self {
			shared: Arc::new(Mutex::new(Shared {
				now: Instant::now(),
				timers: HashMap::new(),
				next_id: 0,
			})),
		}
	}

	/// Move the virtual clock forward, waking every timer that elapses.
	pub fn advance(&self, duration: std::time::Duration) {
		let mut woken = Vec::new();
		{
			let mut shared = self.shared.lock().unwrap();
			shared.now += duration;
			let now = shared.now;
			for entry in shared.timers.values_mut() {
				if entry.at.is_some_and(|at| at <= now) {
					woken.push(entry.waiters.take());
				}
			}
		}
		// Wake outside the lock: a woken task's first move may be to arm a timer.
		for mut waiters in woken {
			waiters.wake();
		}
	}

	/// Jump the virtual clock to the earliest armed timer and fire it.
	///
	/// Returns `false` (moving nothing) when no timer is armed in the future.
	pub fn advance_to_timer(&self) -> bool {
		let next = {
			let shared = self.shared.lock().unwrap();
			let now = shared.now;
			shared
				.timers
				.values()
				.filter_map(|entry| entry.at)
				.filter(|at| *at > now)
				.min()
		};
		match next {
			Some(at) => {
				// Route through `advance` so the wake happens outside the lock.
				let now = self.shared.lock().unwrap().now;
				self.advance(at - now);
				true
			}
			None => false,
		}
	}
}

impl Timers for Test {
	type Timer = TestTimer;

	fn timer(&self) -> Self::Timer {
		let id = {
			let mut shared = self.shared.lock().unwrap();
			let id = shared.next_id;
			shared.next_id += 1;
			shared.timers.insert(
				id,
				Entry {
					at: None,
					waiters: kio::WaiterList::new(),
				},
			);
			id
		};
		TestTimer {
			shared: self.shared.clone(),
			id,
		}
	}

	fn now(&self) -> Instant {
		self.shared.lock().unwrap().now
	}
}

impl Clone for Test {
	fn clone(&self) -> Self {
		Self {
			shared: self.shared.clone(),
		}
	}
}

impl Default for Test {
	fn default() -> Self {
		Self::new()
	}
}

impl std::fmt::Debug for Test {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let shared = self.shared.lock().unwrap();
		f.debug_struct("Test")
			.field("now", &shared.now)
			.field("timers", &shared.timers.len())
			.finish()
	}
}

/// The [`Timer`] handed out by [`Test`]: elapsed exactly when its armed instant
/// is at or before the shared virtual clock.
pub struct TestTimer {
	shared: Arc<Mutex<Shared>>,
	id: u64,
}

impl Timer for TestTimer {
	fn set(&mut self, at: Option<Instant>) {
		let mut shared = self.shared.lock().unwrap();
		if let Some(entry) = shared.timers.get_mut(&self.id) {
			entry.at = at;
		}
	}

	fn poll(&mut self, waiter: &kio::Waiter) -> Poll<()> {
		let mut shared = self.shared.lock().unwrap();
		let now = shared.now;
		let Some(entry) = shared.timers.get_mut(&self.id) else {
			return Poll::Pending;
		};
		match entry.at {
			Some(at) if at <= now => Poll::Ready(()),
			_ => {
				waiter.register(&mut entry.waiters);
				Poll::Pending
			}
		}
	}
}

impl Drop for TestTimer {
	fn drop(&mut self) {
		self.shared.lock().unwrap().timers.remove(&self.id);
	}
}

#[cfg(all(test, not(loom)))]
mod tests {
	use std::time::Duration;

	use super::*;
	use crate::runtime::Deadline;

	fn poll_once(rt: &Test, deadline: &mut Deadline<Test>) -> Poll<()> {
		let _ = rt;
		let waiter = kio::Waiter::noop();
		deadline.poll(&waiter)
	}

	#[test]
	fn fires_only_when_advanced_past() {
		let rt: Test = Test::new();
		let mut deadline = Deadline::after(&rt, Duration::from_secs(5));

		assert!(poll_once(&rt, &mut deadline).is_pending());
		rt.advance(Duration::from_secs(4));
		assert!(poll_once(&rt, &mut deadline).is_pending());
		rt.advance(Duration::from_secs(1));
		assert!(poll_once(&rt, &mut deadline).is_ready());
		// Fused: still ready on a re-poll.
		assert!(poll_once(&rt, &mut deadline).is_ready());
	}

	#[test]
	fn rearm_after_advance_lands_in_the_future() {
		// The reason Timers::now exists: a deadline armed relative to "now"
		// after a big advance must not sit in the virtual past.
		let rt: Test = Test::new();
		rt.advance(Duration::from_secs(3600));

		let mut deadline = Deadline::after(&rt, Duration::from_secs(1));
		assert!(poll_once(&rt, &mut deadline).is_pending());
		rt.advance(Duration::from_secs(1));
		assert!(poll_once(&rt, &mut deadline).is_ready());
	}

	#[test]
	fn disarmed_never_fires() {
		let rt: Test = Test::new();
		let mut deadline = Deadline::new(&rt);
		assert!(poll_once(&rt, &mut deadline).is_pending());
		rt.advance(Duration::from_secs(3600));
		assert!(poll_once(&rt, &mut deadline).is_pending());
	}

	#[test]
	fn disarming_a_live_countdown_stops_it() {
		let rt: Test = Test::new();
		let mut deadline = Deadline::after(&rt, Duration::from_secs(1));
		deadline.set(None);
		rt.advance(Duration::from_secs(10));
		assert!(poll_once(&rt, &mut deadline).is_pending());

		// And re-arming fires again.
		let at = rt.now() + Duration::from_secs(3);
		deadline.set(Some(at));
		rt.advance(Duration::from_secs(3));
		assert!(poll_once(&rt, &mut deadline).is_ready());
	}

	#[test]
	fn advance_wakes_a_parked_waiter() {
		let rt: Test = Test::new();
		let mut deadline = Deadline::after(&rt, Duration::from_secs(1));

		let woken = Arc::new(std::sync::atomic::AtomicBool::new(false));
		struct Flag(Arc<std::sync::atomic::AtomicBool>);
		impl std::task::Wake for Flag {
			fn wake(self: Arc<Self>) {
				self.0.store(true, std::sync::atomic::Ordering::SeqCst);
			}
		}
		let waker = std::task::Waker::from(Arc::new(Flag(woken.clone())));
		let waiter = kio::Waiter::new(waker);

		assert!(deadline.poll(&waiter).is_pending());
		rt.advance(Duration::from_secs(1));
		assert!(
			woken.load(std::sync::atomic::Ordering::SeqCst),
			"the timer wake was lost"
		);
		assert!(deadline.poll(&waiter).is_ready());
	}

	#[test]
	fn advance_to_timer_jumps_to_the_earliest() {
		let rt: Test = Test::new();
		let start = rt.now();
		let mut near = Deadline::after(&rt, Duration::from_secs(2));
		let mut far = Deadline::after(&rt, Duration::from_secs(9));

		assert!(rt.advance_to_timer());
		assert_eq!(rt.now(), start + Duration::from_secs(2));
		assert!(poll_once(&rt, &mut near).is_ready());
		assert!(poll_once(&rt, &mut far).is_pending());

		assert!(rt.advance_to_timer());
		assert_eq!(rt.now(), start + Duration::from_secs(9));
		assert!(poll_once(&rt, &mut far).is_ready());

		// Nothing armed in the future: the clock stays put.
		assert!(!rt.advance_to_timer());
		assert_eq!(rt.now(), start + Duration::from_secs(9));
	}

	#[test]
	fn dropped_timers_release_their_slot() {
		let rt: Test = Test::new();
		let deadline = Deadline::after(&rt, Duration::from_secs(1));
		drop(deadline);
		assert!(!rt.advance_to_timer(), "a dropped timer still counted as armed");
	}
}

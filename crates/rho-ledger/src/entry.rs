//! Entries and the clock that orders them.

use senax_encoder::{Decode, Encode};

use crate::protocol::DeviceId;

/// When an entry was written, as a hybrid logical clock: wall time, a
/// counter for writes within one millisecond, and the device, so two
/// devices never tie. A device's clock never runs behind anything it has
/// read, so a write made after reading another always wins over it, even
/// when the reader's wall clock is behind the writer's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
pub struct Stamp {
    pub millis: u64,
    pub counter: u32,
    pub device: DeviceId,
}

impl Stamp {
    /// The next stamp after `last` at wall time `now`.
    pub(crate) fn next(last: (u64, u32), now: u64, device: DeviceId) -> Self {
        let (millis, counter) = if now > last.0 {
            (now, 0)
        } else {
            (last.0, last.1 + 1)
        };
        Self {
            millis,
            counter,
            device,
        }
    }
}

/// One write: `value` at `key`, or `None` for a key taken away. The
/// newest stamp wins, whoever wrote it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Entry {
    pub key: Vec<u8>,
    pub value: Option<Vec<u8>>,
    pub stamp: Stamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: DeviceId = DeviceId([1; 16]);

    #[test]
    fn the_clock_never_runs_behind_what_it_has_seen() {
        let first = Stamp::next((0, 0), 100, DEVICE);
        assert_eq!((first.millis, first.counter), (100, 0));
        // The wall clock went back: the stamp still moves forward.
        let second = Stamp::next((first.millis, first.counter), 50, DEVICE);
        assert_eq!((second.millis, second.counter), (100, 1));
        assert!(second > first);
    }

    #[test]
    fn two_devices_at_the_same_moment_do_not_tie() {
        let mine = Stamp::next((0, 0), 7, DeviceId([1; 16]));
        let theirs = Stamp::next((0, 0), 7, DeviceId([2; 16]));
        assert_ne!(mine.cmp(&theirs), std::cmp::Ordering::Equal);
    }
}

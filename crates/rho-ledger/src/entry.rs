//! Legacy merged-row shape, retained for one-time migration reads.
use senax_encoder::{Decode, Encode};

use crate::protocol::DeviceId;
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
pub struct Stamp {
    pub millis: u64,
    pub counter: u32,
    pub device: DeviceId,
}

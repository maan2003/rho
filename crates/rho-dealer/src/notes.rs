//! Notes, which sync on their own: a note is its text, and every save is
//! a whole new revision of it. The newest revision of each note is the
//! note.

use jiff::{Timestamp, Zoned};
use senax_encoder::{Decode, Encode};

use crate::facts::{Device, EntryId};

/// A note as one device saved it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct NoteRev {
    pub note: uuid::Uuid,
    pub device: Device,
    /// When and where it was saved; the newest wins.
    pub at: Zoned,
    pub created: Timestamp,
    /// The note's text; its first line is its title.
    pub body: String,
    pub deleted: bool,
}

impl NoteRev {
    pub fn id(&self) -> EntryId {
        EntryId {
            at: self.at.timestamp(),
            device: self.device,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        senax_encoder::encode(self).expect("encode a note").to_vec()
    }

    pub fn decode(mut bytes: &[u8]) -> Option<Self> {
        senax_encoder::decode(&mut bytes).ok()
    }
}

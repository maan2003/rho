/// Append only string, allowing more efficient cloning
use std::sync::{Arc, RwLock};

use bytes::{Buf, BytesMut};
use senax_encoder::{Decoder, Encoder, Packer, Result};

pub struct AppendString {
    shared: Arc<Shared>,
}

pub struct AStr {
    shared: Arc<Shared>,
    len: usize,
}

pub struct Shared {
    // lock free sharing is possible with segmentation and unsafe, but skipping that.
    data: RwLock<String>,
}

impl AStr {
    pub fn with_str<R>(&self, f: impl FnOnce(&str) -> R) -> R {
        f(&self.shared.data.read().expect("poison").as_str()[..self.len])
    }

    /// How two snapshots relate. When they share a buffer the shorter is always
    /// a prefix of the longer (append-only), so the caller can recover the
    /// appended suffix from the two [`len`](Self::len)s. Equal-length snapshots
    /// report [`Diff::LeftIsPrefix`] (each is trivially a prefix of the other).
    pub fn diff(&self, other: &Self) -> Diff {
        if !Arc::ptr_eq(&self.shared, &other.shared) {
            Diff::Unrelated
        } else if self.len <= other.len {
            Diff::LeftIsPrefix
        } else {
            Diff::RightIsPrefix
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

pub enum Diff {
    Unrelated,
    LeftIsPrefix,
    RightIsPrefix,
}

impl Clone for AStr {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            len: self.len,
        }
    }
}

impl PartialEq for AStr {
    fn eq(&self, other: &Self) -> bool {
        self.with_str(|a| other.with_str(|b| a == b))
    }
}

impl Eq for AStr {}

impl std::fmt::Display for AStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.with_str(|s| f.write_str(s))
    }
}

impl std::fmt::Debug for AStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.with_str(|s| std::fmt::Debug::fmt(s, f))
    }
}

impl Encoder for AStr {
    fn encode(&self, writer: &mut BytesMut) -> Result<()> {
        self.with_str(|value| value.encode(writer))
    }

    fn is_default(&self) -> bool {
        self.is_empty()
    }
}

impl Packer for AStr {
    fn pack(&self, writer: &mut BytesMut) -> Result<()> {
        self.with_str(|value| value.pack(writer))
    }
}

impl Decoder for AStr {
    fn decode(reader: &mut impl Buf) -> Result<Self> {
        Ok(String::decode(reader)?.into())
    }
}

impl From<String> for AStr {
    fn from(value: String) -> Self {
        AppendString::from(value).snapshot()
    }
}

impl From<&str> for AStr {
    fn from(value: &str) -> Self {
        AStr::from(value.to_owned())
    }
}

impl From<String> for AppendString {
    fn from(value: String) -> Self {
        Self {
            shared: Arc::new(Shared {
                data: RwLock::new(value),
            }),
        }
    }
}

impl Default for AppendString {
    fn default() -> Self {
        Self::new()
    }
}

impl AppendString {
    pub fn new() -> Self {
        Self::from(String::new())
    }

    pub fn push_str(&mut self, string: &str) {
        self.shared.data.write().expect("poison").push_str(string);
    }

    pub fn snapshot(&self) -> AStr {
        let len = self.shared.data.read().expect("poison").len();
        AStr {
            shared: self.shared.clone(),
            len,
        }
    }
}

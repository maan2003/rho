//! Retained software video frames, independent of codecs and transports.
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// Immutable full-resolution 8-bit Y, U and V planes. Implementations must keep
/// the decoder's allocation alive and prevent its reuse while this object lives.
pub trait Yuv444Data: Send + Sync {
    /// Visible frame size.
    fn size(&self) -> (u32, u32);
    /// Plane bytes including row padding, but not necessarily final-row padding.
    fn plane(&self, index: usize) -> &[u8];
    /// Distance in bytes between rows.
    fn stride(&self, index: usize) -> u32;
}

/// A full-range BT.601 YUV444 frame with sRGB-encoded display channels.
/// Clones retain the same storage and upload identity; no pixels are copied.
#[derive(Clone)]
pub struct VideoFrame {
    id: u64,
    data: Arc<dyn Yuv444Data>,
}
impl VideoFrame {
    /// Validate all planes before making the frame available to the renderer.
    pub fn new(data: Arc<dyn Yuv444Data>) -> anyhow::Result<Self> {
        let (width, height) = data.size();
        anyhow::ensure!(width > 0 && height > 0, "empty video frame");
        for plane in 0..3 {
            let stride = data.stride(plane);
            let length = (height as usize - 1)
                .checked_mul(stride as usize)
                .and_then(|v| v.checked_add(width as usize));
            anyhow::ensure!(
                stride >= width && length.is_some_and(|len| len <= data.plane(plane).len()),
                "invalid video plane"
            );
        }
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Ok(Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            data,
        })
    }
    /// Stable identity for this immutable frame.
    pub fn id(&self) -> u64 {
        self.id
    }
    /// Visible frame size.
    pub fn size(&self) -> (u32, u32) {
        self.data.size()
    }
    /// Decoder-owned plane data.
    pub fn plane(&self, index: usize) -> &[u8] {
        self.data.plane(index)
    }
    /// Decoder plane stride.
    pub fn stride(&self, index: usize) -> u32 {
        self.data.stride(index)
    }
}
impl std::fmt::Debug for VideoFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoFrame")
            .field("id", &self.id)
            .field("size", &self.size())
            .finish()
    }
}
impl PartialEq for VideoFrame {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for VideoFrame {}

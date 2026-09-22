//! Shared MoQ framing and optional VP9 codec, without compositor dependencies.
pub const MAX_PACKET: usize = 16 * 1024 * 1024;
#[cfg(any(feature = "encoder", feature = "decoder"))]
pub mod codec;
pub mod media;

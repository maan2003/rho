//! Local agent-desktop protocol, independent of the compositor and media
//! implementation.
//!
//! A connection starts with Hello. Requests and responses are newline-delimited
//! JSON, limited to MAX_HEADER bytes including the newline. A Frame header is
//! immediately followed by exactly width * height * 4 bytes of packed,
//! top-to-bottom BGRA8. Requests are sequential: finish reading one response
//! before sending the next. Socket access grants screenshot access; this
//! protocol supplies no authentication. Annotations are client-owned screenshot
//! attachments, not desktop protocol state.

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 4;
pub const MAX_HEADER: u64 = 65536;
pub const MAX_DIMENSION: u32 = 4096;
pub const SOCKET_ENV: &str = "RHO_DESKTOP_SOCKET";

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello { version: u32 },
    Capture { output: String },
    Input { input: Input },
    Status,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Hello {
        version: u32,
        outputs: Vec<Output>,
        media_socket: String,
    },
    Done,
    Status {
        streaming: bool,
        composed: u64,
        encoded: u64,
    },
    Frame {
        width: u32,
        height: u32,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Output {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

/// Validates dimensions before a client allocates a frame buffer.
pub fn frame_len(width: u32, height: u32) -> Option<usize> {
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return None;
    }
    Some(width as usize * height as usize * 4)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wire_format_is_not_a_compositor_type() {
        let request = Request::Capture {
            output: "headless-1".into(),
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"type":"capture","output":"headless-1"}"#
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"type":"hello","version":4}"#).unwrap(),
            Request::Hello { version: VERSION }
        );
    }
    #[test]
    fn progress_round_trips_without_conflating_pipeline_stages() {
        let input = Input::Feedback(Feedback {
            received: Some(FrameId {
                group: 9,
                timestamp_us: 81_000,
            }),
            decoded: Some(FrameId {
                group: 8,
                timestamp_us: 72_000,
            }),
            presented: Some(FrameId {
                group: 8,
                timestamp_us: 63_000,
            }),
            decode_us: 19_000,
            lag_us: 47_000,
            recover: true,
        });
        assert_eq!(
            serde_json::from_slice::<Input>(&serde_json::to_vec(&input).unwrap()).unwrap(),
            input
        );
        #[cfg(feature = "senax")]
        {
            let bytes = senax_encoder::pack(&input).unwrap();
            assert_eq!(
                senax_encoder::unpack::<Input>(&mut &bytes[..]).unwrap(),
                input
            );
        }
    }

    #[test]
    fn validates_both_dimensions_before_allocation() {
        assert_eq!(frame_len(65, 47), Some(12220));
        assert_eq!(frame_len(4096, 4096), Some(67108864));
        for (w, h) in [
            (0, 47),
            (65, 0),
            (4097, 47),
            (65, 4097),
            (u32::MAX, u32::MAX),
        ] {
            assert_eq!(frame_len(w, h), None);
        }
    }
}

/// Input coordinates are output pixels, not logical Wayland coordinates.
/// Physical keys are Linux evdev codes; release held keys/buttons on
/// disconnect.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    feature = "senax",
    derive(
        senax_encoder::Encode,
        senax_encoder::Decode,
        senax_encoder::Pack,
        senax_encoder::Unpack
    )
)]
pub enum Input {
    Move { x: u32, y: u32 },
    Button { button: u32, pressed: bool },
    Scroll { horizontal: f64, vertical: f64 },
    Key(String),
    Text(String),
    Physical { code: u32, pressed: bool },
    ReleaseAll,
    Quality { bitrate: u32, keyframe: bool },
    Feedback(Feedback),
}

/// Identity survives capture, transport, decode, and presentation. Timestamps
/// use the source's monotonic capture clock, not either machine's wall clock.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(
    feature = "senax",
    derive(
        senax_encoder::Encode,
        senax_encoder::Decode,
        senax_encoder::Pack,
        senax_encoder::Unpack
    )
)]
pub struct FrameId {
    pub group: u64,
    pub timestamp_us: u64,
}

/// Coalesced receiver progress. `presented` marks a GUI frame callback after
/// paint, not a hardware scanout fence. Durations are measured on the receiver.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    feature = "senax",
    derive(
        senax_encoder::Encode,
        senax_encoder::Decode,
        senax_encoder::Pack,
        senax_encoder::Unpack
    )
)]
pub struct Feedback {
    pub received: Option<FrameId>,
    pub decoded: Option<FrameId>,
    pub presented: Option<FrameId>,
    pub decode_us: u64,
    pub lag_us: u64,
    pub recover: bool,
}

/// Asynchronous viewer control errors. Media travels on separate MoQ streams.
#[derive(Clone, Debug)]
#[cfg_attr(
    feature = "senax",
    derive(
        senax_encoder::Encode,
        senax_encoder::Decode,
        senax_encoder::Pack,
        senax_encoder::Unpack
    )
)]
pub enum Packet {
    Error(String),
}

#[cfg(all(feature = "local", target_os = "linux"))]
pub mod local;

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

pub const VERSION: u32 = 3;
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
            serde_json::from_str::<Request>(r#"{"type":"hello","version":3}"#).unwrap(),
            Request::Hello { version: VERSION }
        );
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

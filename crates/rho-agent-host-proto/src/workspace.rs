//! Typed frames for a dedicated workspace file channel.

use camino::Utf8PathBuf;
use senax_encoder::{Decode, Encode, Pack, Unpack};

/// Largest file accepted by the workspace editor protocol.
pub const MAX_FILE_LEN: usize = 8 * 1024 * 1024;
/// File payload plus bounded request metadata and senax framing overhead.
pub const MAX_WORKSPACE_FRAME_LEN: usize = MAX_FILE_LEN + 16 * 1024;

/// Client-to-daemon frames after the channel handshake.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum WorkspaceClientFrame {
    Open {
        request_id: u64,
        path: Utf8PathBuf,
    },
    Reload {
        request_id: u64,
        path: Utf8PathBuf,
    },
    /// Save only if `revision` still identifies the current on-disk contents.
    /// An empty revision means the path was absent and must still be absent.
    Save {
        request_id: u64,
        path: Utf8PathBuf,
        revision: Vec<u8>,
        contents: Vec<u8>,
    },
    /// Create or replace a file regardless of its current revision.
    Overwrite {
        request_id: u64,
        path: Utf8PathBuf,
        contents: Vec<u8>,
    },
}

/// Result of opening or reloading a file.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum FileReadResult {
    File {
        contents: Vec<u8>,
        revision: Vec<u8>,
    },
    Deleted,
    Error(String),
}

/// Result of a checked or overwrite save.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum FileSaveResult {
    Saved {
        revision: Vec<u8>,
    },
    /// The file changed since the supplied revision. The current contents are
    /// returned so the client can present or compute a merge immediately.
    Conflict {
        contents: Vec<u8>,
        revision: Vec<u8>,
    },
    /// A checked-save target no longer exists.
    Deleted,
    Error(String),
}

/// Daemon-to-client frames after the channel handshake.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum WorkspaceServerFrame {
    Opened {
        request_id: u64,
        path: Utf8PathBuf,
        result: FileReadResult,
    },
    Reloaded {
        request_id: u64,
        path: Utf8PathBuf,
        result: FileReadResult,
    },
    Saved {
        request_id: u64,
        path: Utf8PathBuf,
        result: FileSaveResult,
    },
    /// Unsolicited, coalesced filesystem changes relative to the checkout.
    Changed {
        paths: Vec<Utf8PathBuf>,
        /// At least one event was dropped from the bounded watcher queue;
        /// clients must rescan the paths whose state they retain.
        rescan: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_frames_round_trip_arbitrary_bytes() {
        let frame = WorkspaceServerFrame::Opened {
            request_id: 7,
            path: Utf8PathBuf::from("src/main.rs"),
            result: FileReadResult::File {
                contents: b"\xef\xbb\xbfline\r\n\xff".to_vec(),
                revision: vec![9; 32],
            },
        };
        let bytes = senax_encoder::pack(&frame).unwrap();
        let mut bytes: &[u8] = &bytes;
        let decoded: WorkspaceServerFrame = senax_encoder::unpack(&mut bytes).unwrap();
        assert_eq!(decoded, frame);
    }

    #[tokio::test]
    async fn workspace_frame_limit_is_enforced_before_allocation() {
        use tokio::io::AsyncWriteExt as _;

        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer.write_u32_le(1024).await.unwrap();
        let error = crate::read_frame_limited::<_, WorkspaceClientFrame>(&mut reader, 32)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeds 32"));
    }

    #[tokio::test]
    async fn limited_workspace_frame_round_trips_without_option_wrapping() {
        let frame = WorkspaceServerFrame::Changed {
            paths: vec![Utf8PathBuf::from("src/main.rs")],
            rescan: true,
        };
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        crate::write_frame_limited(&mut writer, &frame, MAX_WORKSPACE_FRAME_LEN)
            .await
            .unwrap();
        let decoded: WorkspaceServerFrame =
            crate::read_frame_limited(&mut reader, MAX_WORKSPACE_FRAME_LEN)
                .await
                .unwrap();
        assert_eq!(decoded, frame);
    }
}

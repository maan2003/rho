//! Data types shared by workspace implementations and protocol clients.

use camino::Utf8PathBuf;
use senax_encoder::{Decode, Encode, Pack, Unpack};

/// Where an agent works, stored inline on the agent record. Self-contained:
/// there is no separate workspace table.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode, Pack, Unpack)]
pub enum WorkspaceInfo {
    /// A workspace in an agent-owned clone-store family.
    Checkout { repo: String, name: String },
    /// A workspace whose original VCS metadata is masked from
    /// child commands and replaced by a synthetic Git baseline.
    Sandbox { repo: String, name: String },
}

impl WorkspaceInfo {
    pub fn repo(&self) -> &str {
        match self {
            Self::Checkout { repo, .. } | Self::Sandbox { repo, .. } => repo,
        }
    }

    pub fn is_user_checkout(&self) -> bool {
        false
    }

    pub fn workspace_handle(&self) -> Option<String> {
        match self {
            Self::Checkout { name, .. } => Some(name.clone()),
            Self::Sandbox { .. } => None,
        }
    }

    pub fn is_sandbox(&self) -> bool {
        matches!(self, Self::Sandbox { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct WorkspaceDiffSnapshot {
    /// Exact jj operation from which the manifest was materialized.
    pub operation_id: String,
    /// Immutable working-copy commit the snapshot describes.
    pub commit_id: String,
    pub files: Vec<WorkspaceDiffFile>,
    /// At least one changed path was omitted after the implementation's file
    /// limit.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct WorkspaceDiffBaseContent {
    pub path: Utf8PathBuf,
    pub content: WorkspaceDiffContent,
    pub executable: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct WorkspaceDiffFile {
    /// Repository-relative path. A rename is represented losslessly as one
    /// deletion and one addition; copy presentation can be layered on later
    /// without changing file contents or edit semantics.
    pub path: Utf8PathBuf,
    pub status: WorkspaceDiffStatus,
    pub base: WorkspaceDiffContent,
    /// Descriptor for the snapshotted current side. Text comes from the live
    /// Zed Project buffer and is deliberately not duplicated on the wire.
    pub target: WorkspaceDiffTarget,
    pub base_executable: Option<bool>,
    pub target_executable: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum WorkspaceDiffStatus {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum WorkspaceDiffContent {
    /// File body is available from the immutable snapshot on demand.
    Deferred,
    Absent,
    Text(String),
    Binary {
        bytes: u64,
    },
    TooLarge {
        bytes_at_least: u64,
    },
    BudgetExhausted,
    Symlink(String),
    GitSubmodule(String),
    AccessDenied(String),
    OtherConflict(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum WorkspaceDiffTarget {
    Absent,
    Text { bytes: u64 },
    Binary { bytes: u64 },
    TooLarge { bytes_at_least: u64 },
    BudgetExhausted,
    Symlink(String),
    GitSubmodule(String),
    Conflict(String),
}

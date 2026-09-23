//! Data types shared by workspace implementations and protocol clients.

use camino::{Utf8Path, Utf8PathBuf};
use senax_encoder::{Decode, Encode, Pack, Unpack};

/// An agent's place: the workset it works in and its working directory
/// there as it sees it (`/src/<repo>/...`), how it sees the filesystem,
/// and what was cloned to make the workset when its creation cloned it.
/// Stored inline on the agent record; there is no workset table.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode, Pack, Unpack)]
pub struct Place {
    pub workset: String,
    pub cwd: Utf8PathBuf,
    #[senax(default)]
    pub mode: WorksetMode,
    #[senax(default)]
    pub origin: Option<Utf8PathBuf>,
}

/// Where a request names work: a place in a workset, or the user's own
/// checkout of a repository (a request only; no agent lives there).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode, Pack, Unpack)]
pub enum WorkspaceInfo {
    /// The user's own checkout: the repository path itself.
    UserCheckout {
        repo: Utf8PathBuf,
    },
    Workset(Place),
}

/// How an agent in a workset sees the filesystem.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode, Pack, Unpack)]
pub enum WorksetMode {
    /// A minimal generated root with the workset at `/src`.
    #[default]
    View,
    /// The host filesystem with the workset mounted over its `/src` stub.
    Exposed,
}

impl From<Place> for WorkspaceInfo {
    fn from(place: Place) -> Self {
        Self::Workset(place)
    }
}

impl WorkspaceInfo {
    /// The directory the agent works in: the repository root for the
    /// user's checkout, the working directory for a place.
    pub fn repo(&self) -> &Utf8Path {
        match self {
            Self::UserCheckout { repo } => repo,
            Self::Workset(place) => &place.cwd,
        }
    }

    pub fn place(&self) -> Option<&Place> {
        match self {
            Self::Workset(place) => Some(place),
            Self::UserCheckout { .. } => None,
        }
    }

    /// The workset this names, if a place.
    pub fn workset(&self) -> Option<&str> {
        self.place().map(|place| place.workset.as_str())
    }

    /// What was cloned to make the place's workset, when known; the
    /// repository itself for the user's checkout.
    pub fn origin(&self) -> Option<&Utf8Path> {
        match self {
            Self::Workset(place) => place.origin.as_deref(),
            Self::UserCheckout { repo } => Some(repo),
        }
    }

    pub fn is_user_checkout(&self) -> bool {
        matches!(self, Self::UserCheckout { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct WorkspaceDiffSnapshot {
    /// Exact VCS operation from which the manifest was materialized.
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

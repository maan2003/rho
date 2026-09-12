//! Data types shared by workspace implementations and protocol clients.

use camino::{Utf8Path, Utf8PathBuf};
use prefix_id::{PrefixId, PrefixIdDomain};
use senax_encoder::{Decode, Encode, Pack, Unpack};

pub type WorkspaceId = PrefixId<WorkspaceIdDomain>;

fn workspace_handle(id: WorkspaceId) -> String {
    format!("ws-{}", id.encoded())
}

/// Prefix-id family for repository-local managed-workspace ids.
///
/// Historical: the VCS of the time owned the per-repository seed and
/// counter. Rho persists the encoded id so old records still decode and
/// does not allocate production ids itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceIdDomain(pub u64);

impl PrefixIdDomain for WorkspaceIdDomain {
    const KIND: &'static str = "managed-workspace-id";

    fn machine_seed(&self) -> u64 {
        self.0
    }
}

/// Where an agent works, stored inline on the agent record. Self-contained:
/// there is no separate workspace table.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode, Pack, Unpack)]
pub enum WorkspaceInfo {
    /// The user's own checkout: the agent works directly at the repo path,
    /// no separate checkout and no namespace.
    UserCheckout { repo: Utf8PathBuf },
    /// A managed workspace of an earlier version, which persisted its own
    /// checkout path; Rho stores only the repository-local id. Historical:
    /// no longer created.
    Workspace {
        repo: Utf8PathBuf,
        #[senax(rename = "name")]
        id: WorkspaceId,
    },
    /// A managed workspace whose original VCS metadata is masked from
    /// child commands and replaced by a synthetic Git baseline. Historical:
    /// no longer created.
    Sandbox { repo: Utf8PathBuf, id: WorkspaceId },
    /// A place in a workset: the workset's id and the agent's working
    /// directory as it sees it (`/src/<repo>/...`). `origin` is what was
    /// cloned to make the workset, when this agent's creation cloned it.
    Workset {
        workset: String,
        cwd: Utf8PathBuf,
        #[senax(default)]
        mode: WorksetMode,
        #[senax(default)]
        origin: Option<Utf8PathBuf>,
    },
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

impl WorkspaceInfo {
    /// The directory the agent works in: a repository root for the
    /// historical variants, the working directory for a workset.
    pub fn repo(&self) -> &Utf8Path {
        match self {
            Self::UserCheckout { repo }
            | Self::Workspace { repo, .. }
            | Self::Sandbox { repo, .. } => repo,
            Self::Workset { cwd, .. } => cwd,
        }
    }

    /// The workset this agent works in.
    pub fn workset(&self) -> Option<&str> {
        match self {
            Self::Workset { workset, .. } => Some(workset),
            _ => None,
        }
    }

    /// What was cloned to start this agent's workset, when known.
    pub fn origin(&self) -> Option<&Utf8Path> {
        match self {
            Self::Workset { origin, .. } => origin.as_deref(),
            Self::UserCheckout { repo }
            | Self::Workspace { repo, .. }
            | Self::Sandbox { repo, .. } => Some(repo),
        }
    }

    pub fn is_user_checkout(&self) -> bool {
        matches!(self, Self::UserCheckout { .. })
    }

    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        match self {
            Self::UserCheckout { .. } | Self::Workset { .. } => None,
            Self::Workspace { id, .. } | Self::Sandbox { id, .. } => Some(*id),
        }
    }

    pub fn workspace_handle(&self) -> Option<String> {
        match self {
            Self::Workspace { id, .. } => Some(workspace_handle(*id)),
            Self::UserCheckout { .. } | Self::Sandbox { .. } | Self::Workset { .. } => None,
        }
    }

    pub fn is_sandbox(&self) -> bool {
        matches!(self, Self::Sandbox { .. })
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

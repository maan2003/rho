//! Who may use a rendezvous directory that the transport creates.

use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Access granted on a rendezvous directory created by this transport.
///
/// The mode is set explicitly after creation, so the process umask never
/// decides it. A directory that already exists is never re-moded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirectoryAccess {
    /// Only the creating user (`0700`).
    Owner,
    /// Every local user, with the sticky bit (`01777`), like `/tmp`.
    Shared,
}

impl DirectoryAccess {
    #[cfg(unix)]
    fn mode(self) -> u32 {
        match self {
            Self::Owner => 0o700,
            Self::Shared => 0o1777,
        }
    }

    /// Create `root` (and its parents) if missing; set the mode only when
    /// this call created `root` itself.
    pub(crate) async fn ensure(self, root: &Path) -> io::Result<()> {
        if let Some(parent) = root.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        match tokio::fs::create_dir(root).await {
            Ok(()) => self.apply(root).await,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
        }
    }

    #[cfg(unix)]
    async fn apply(self, root: &Path) -> io::Result<()> {
        tokio::fs::set_permissions(root, std::fs::Permissions::from_mode(self.mode())).await
    }

    #[cfg(not(unix))]
    async fn apply(self, _root: &Path) -> io::Result<()> {
        Ok(())
    }
}

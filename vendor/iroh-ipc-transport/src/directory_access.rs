//! The rendezvous directory is the user's alone.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// The mode of everything the transport creates: the directory, and the
/// sockets, records and leases in it. The directory keeps other users out
/// on its own; the files say the same, so a directory that is opened up
/// later does not open them. Set explicitly, so the umask never decides.
pub(crate) const PRIVATE: u32 = 0o600;

/// Create `root` (and its parents) if missing, mode `0700`, and only then:
/// a directory that already exists keeps the mode it has.
pub(crate) async fn ensure_private(root: &Path) -> io::Result<()> {
    if let Some(parent) = root.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    match tokio::fs::create_dir(root).await {
        Ok(()) => {
            tokio::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).await
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Makes a file the transport created private ([`PRIVATE`]).
pub(crate) async fn make_private(path: &Path) -> io::Result<()> {
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE)).await
}

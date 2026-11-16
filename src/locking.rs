//! Audited async boundary for portable, inherently synchronous file locks.
//!
//! Replace this boundary when Tokio exposes advisory locks. No socket probe is
//! involved: process death releases the OS lock even after an unclean shutdown.

use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tokio::fs::{File, OpenOptions};

#[derive(Debug)]
pub(crate) struct Lock {
    _file: File,
}

impl Lock {
    pub(crate) async fn open(path: &Path, create: bool) -> io::Result<Self> {
        let file = if create {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(path)
                .await
            {
                Ok(file) => {
                    #[cfg(unix)]
                    {
                        file.set_permissions(std::fs::Permissions::from_mode(0o666))
                            .await?;
                    }
                    file
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    OpenOptions::new().read(true).write(true).open(path).await?
                }
                Err(error) => return Err(error),
            }
        } else {
            OpenOptions::new().read(true).write(true).open(path).await?
        };
        let file = file.into_std().await;
        let file = tokio::task::spawn_blocking(move || {
            fs2::FileExt::try_lock_exclusive(&file).map_err(|error| {
                if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
                    io::Error::new(io::ErrorKind::WouldBlock, error)
                } else {
                    error
                }
            })?;
            Ok::<_, io::Error>(file)
        })
        .await
        .map_err(io::Error::other)??;
        Ok(Self {
            _file: File::from_std(file),
        })
    }
}

use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Component;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, ensure};
use camino::{Utf8Path, Utf8PathBuf};
use notify::{RecursiveMode, Watcher as _};
use rho_ui_proto::workspace::{FileReadResult, FileSaveResult, MAX_FILE_LEN};
use sha2::{Digest as _, Sha256};

const MAX_PATH_LEN: usize = 4096;
const CHANGE_QUEUE: usize = 256;
const MAX_CHANGED_PATHS: usize = 256;

pub(super) enum WatchChange {
    Path(Utf8PathBuf),
    Rescan,
}

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_BENEATH: u64 = 0x08;

pub(super) struct WorkspaceFiles {
    root: Utf8PathBuf,
    root_fd: OwnedFd,
}

impl WorkspaceFiles {
    pub(super) fn open(root: Utf8PathBuf) -> anyhow::Result<Self> {
        let root_fd: OwnedFd = File::open(&root)
            .with_context(|| format!("open workspace root {root}"))?
            .into();
        ensure!(root.is_dir(), "workspace root is not a directory");
        Ok(Self { root, root_fd })
    }

    pub(super) async fn read(self: &Arc<Self>, path: Utf8PathBuf) -> FileReadResult {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.read_sync(&path))
            .await
            .unwrap_or_else(|error| FileReadResult::Error(format!("file task failed: {error}")))
    }

    pub(super) async fn save(
        self: &Arc<Self>,
        path: Utf8PathBuf,
        expected: Option<Vec<u8>>,
        contents: Vec<u8>,
    ) -> FileSaveResult {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.save_sync(&path, expected.as_deref(), &contents))
            .await
            .unwrap_or_else(|error| FileSaveResult::Error(format!("file task failed: {error}")))
    }

    fn read_sync(&self, path: &Utf8Path) -> FileReadResult {
        match self.read_checked(path) {
            Ok(Some((contents, revision))) => FileReadResult::File { contents, revision },
            Ok(None) => FileReadResult::Deleted,
            Err(error) => FileReadResult::Error(format!("{error:#}")),
        }
    }

    fn save_sync(
        &self,
        path: &Utf8Path,
        expected: Option<&[u8]>,
        contents: &[u8],
    ) -> FileSaveResult {
        if let Err(error) = validate_path(path) {
            return FileSaveResult::Error(format!("{error:#}"));
        }
        if contents.len() > MAX_FILE_LEN {
            return FileSaveResult::Error(format!(
                "file length {} exceeds {MAX_FILE_LEN}",
                contents.len()
            ));
        }

        let current = match self.read_checked_details(path) {
            Ok(current) => current,
            Err(error) => return FileSaveResult::Error(format!("read {path}: {error:#}")),
        };
        if let Some(result) = checked_save_mismatch(expected, &current) {
            return result;
        }

        let parent = match self.open_parent(path) {
            Ok(parent) => parent,
            Err(error) => return FileSaveResult::Error(format!("open parent of {path}: {error}")),
        };
        let target_name = match path.file_name().and_then(|name| CString::new(name).ok()) {
            Some(name) => name,
            None => return FileSaveResult::Error(format!("invalid file name: {path}")),
        };
        let mode = current.as_ref().map(|details| details.mode);
        let mut temp = match TempFile::create(&parent, mode) {
            Ok(temp) => temp,
            Err(error) => return FileSaveResult::Error(format!("create temporary file: {error}")),
        };
        if let Err(error) = temp
            .file
            .write_all(contents)
            .and_then(|()| temp.file.sync_all())
        {
            return FileSaveResult::Error(format!("write temporary file for {path}: {error}"));
        }

        // Revalidate only after the complete replacement is durable. An
        // external change during preparation must not be overwritten.
        let current = match self.read_checked_details(path) {
            Ok(current) => current,
            Err(error) => return FileSaveResult::Error(format!("revalidate {path}: {error:#}")),
        };
        if let Some(result) = checked_save_mismatch(expected, &current) {
            return result;
        }
        if let Some(details) = &current
            && let Err(error) = temp
                .file
                .set_permissions(std::fs::Permissions::from_mode(details.mode))
                .and_then(|()| temp.file.sync_all())
        {
            return FileSaveResult::Error(format!("preserve mode for {path}: {error}"));
        }

        let flags = if expected.is_some_and(|revision| revision.is_empty()) {
            libc::RENAME_NOREPLACE
        } else {
            0
        };
        // SAFETY: both names are valid C strings relative to the same open
        // directory descriptor. The temporary name is uniquely owned here.
        let renamed = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                parent.as_raw_fd(),
                temp.name.as_ptr(),
                parent.as_raw_fd(),
                target_name.as_ptr(),
                flags,
            )
        };
        if renamed < 0 {
            let error = std::io::Error::last_os_error();
            if flags == libc::RENAME_NOREPLACE && error.raw_os_error() == Some(libc::EEXIST) {
                return match self.read_checked_details(path) {
                    Ok(Some(details)) => FileSaveResult::Conflict {
                        contents: details.contents,
                        revision: details.revision,
                    },
                    Ok(None) => FileSaveResult::Deleted,
                    Err(error) => FileSaveResult::Error(format!("recheck {path}: {error:#}")),
                };
            }
            return FileSaveResult::Error(format!("replace {path}: {error}"));
        }
        temp.committed = true;
        // Best-effort directory durability; a failure is still reported even
        // though the rename itself has completed.
        if unsafe { libc::fsync(parent.as_raw_fd()) } < 0 {
            return FileSaveResult::Error(format!(
                "sync parent of {path}: {}",
                std::io::Error::last_os_error()
            ));
        }

        let desired = revision(contents);
        match self.read_checked_details(path) {
            Ok(Some(details)) if details.revision == desired => {
                FileSaveResult::Saved { revision: desired }
            }
            Ok(Some(details)) => FileSaveResult::Conflict {
                contents: details.contents,
                revision: details.revision,
            },
            Ok(None) => FileSaveResult::Deleted,
            Err(error) => FileSaveResult::Error(format!("verify {path}: {error:#}")),
        }
    }

    fn open_parent(&self, path: &Utf8Path) -> std::io::Result<File> {
        let parent = path.parent().unwrap_or(Utf8Path::new(""));
        if parent.as_str().is_empty() {
            let fd = self.root_fd.try_clone()?;
            return Ok(File::from(fd));
        }
        match self.openat2(
            parent,
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY,
            0,
        ) {
            Ok(parent) => Ok(parent),
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                self.create_parent_dirs(parent)
            }
            Err(error) => Err(error),
        }
    }

    fn create_parent_dirs(&self, parent: &Utf8Path) -> std::io::Result<File> {
        let mut directory = File::from(self.root_fd.try_clone()?);
        for component in parent.as_std_path().components() {
            let Component::Normal(component) = component else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "parent path is not normalized",
                ));
            };
            let component = CString::new(component.as_encoded_bytes()).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL")
            })?;
            // SAFETY: directory is an owned directory fd and component is one
            // validated path component. EEXIST is handled by the open below.
            let created =
                unsafe { libc::mkdirat(directory.as_raw_fd(), component.as_ptr(), 0o777) };
            if created < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EEXIST) {
                    return Err(error);
                }
            }
            // SAFETY: openat is rooted in the previously opened directory;
            // O_NOFOLLOW prevents a component swap from escaping via symlink.
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    component.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: openat returned a new owned descriptor.
            directory = unsafe { File::from_raw_fd(fd) };
        }
        Ok(directory)
    }

    fn read_checked_details(&self, path: &Utf8Path) -> anyhow::Result<Option<FileDetails>> {
        validate_path(path)?;
        let mut file =
            match self.openat2(path, libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW, 0) {
                Ok(file) => file,
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
                Err(error) => return Err(error).with_context(|| format!("open {path}")),
            };
        let metadata = file.metadata()?;
        let mode = metadata.permissions().mode();
        let contents = read_bounded(&mut file).with_context(|| format!("read {path}"))?;
        let revision = revision(&contents);
        Ok(Some(FileDetails {
            contents,
            revision,
            mode,
        }))
    }

    fn read_checked(&self, path: &Utf8Path) -> anyhow::Result<Option<(Vec<u8>, Vec<u8>)>> {
        Ok(self
            .read_checked_details(path)?
            .map(|details| (details.contents, details.revision)))
    }

    fn openat2(&self, path: &Utf8Path, flags: i32, mode: u32) -> std::io::Result<File> {
        let path = CString::new(path.as_str())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has NUL"))?;
        let how = OpenHow {
            flags: flags as u64,
            mode: mode as u64,
            resolve: RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS,
        };
        // SAFETY: `path` and `how` remain valid for the syscall. On success
        // the returned descriptor is newly owned by this process.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                self.root_fd.as_raw_fd(),
                path.as_ptr(),
                &how,
                size_of::<OpenHow>(),
            )
        } as i32;
        if fd < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            // SAFETY: successful openat2 returned a new owned file descriptor.
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    pub(super) fn watcher(
        &self,
    ) -> anyhow::Result<(
        notify::RecommendedWatcher,
        tokio::sync::mpsc::Receiver<WatchChange>,
        Arc<AtomicBool>,
    )> {
        let root = self.root.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(CHANGE_QUEUE);
        let overflowed = Arc::new(AtomicBool::new(false));
        let callback_overflowed = overflowed.clone();
        let mut watcher =
            notify::recommended_watcher(move |result: Result<notify::Event, notify::Error>| {
                let Ok(event) = result else {
                    callback_overflowed.store(true, Ordering::Release);
                    let _ = tx.try_send(WatchChange::Rescan);
                    return;
                };
                if event.need_rescan() {
                    callback_overflowed.store(true, Ordering::Release);
                    let _ = tx.try_send(WatchChange::Rescan);
                }
                if watcher_event_requires_rescan(&event.kind) {
                    callback_overflowed.store(true, Ordering::Release);
                    let _ = tx.try_send(WatchChange::Rescan);
                }
                for path in event.paths {
                    let Ok(relative) = path.strip_prefix(root.as_std_path()) else {
                        continue;
                    };
                    let Ok(relative) = Utf8PathBuf::from_path_buf(relative.to_owned()) else {
                        continue;
                    };
                    if validate_path(&relative).is_ok()
                        && tx.try_send(WatchChange::Path(relative)).is_err()
                    {
                        callback_overflowed.store(true, Ordering::Release);
                    }
                }
            })?;
        watcher.watch(self.root.as_std_path(), RecursiveMode::Recursive)?;
        Ok((watcher, rx, overflowed))
    }
}

fn watcher_event_requires_rescan(kind: &notify::EventKind) -> bool {
    matches!(
        kind,
        notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
            | notify::EventKind::Remove(_)
    )
}

pub(super) fn drain_changes(
    first: WatchChange,
    rx: &mut tokio::sync::mpsc::Receiver<WatchChange>,
) -> (Vec<Utf8PathBuf>, bool) {
    let mut paths = BTreeSet::new();
    let mut rescan = false;
    match first {
        WatchChange::Path(path) => {
            paths.insert(path);
        }
        WatchChange::Rescan => rescan = true,
    }
    while paths.len() < MAX_CHANGED_PATHS {
        let Ok(change) = rx.try_recv() else { break };
        match change {
            WatchChange::Path(path) => {
                paths.insert(path);
            }
            WatchChange::Rescan => rescan = true,
        }
    }
    (paths.into_iter().collect(), rescan)
}

fn validate_path(path: &Utf8Path) -> anyhow::Result<()> {
    ensure!(!path.as_str().is_empty(), "workspace path is empty");
    ensure!(
        path.as_str().len() <= MAX_PATH_LEN,
        "workspace path is too long"
    );
    ensure!(!path.is_absolute(), "workspace path must be relative");
    let mut normalized = std::path::PathBuf::new();
    for component in path.as_std_path().components() {
        let Component::Normal(component) = component else {
            anyhow::bail!("workspace path contains traversal or a non-normal component");
        };
        normalized.push(component);
    }
    ensure!(
        normalized.as_os_str() == path.as_str(),
        "workspace path is not normalized"
    );
    Ok(())
}

fn read_bounded(file: &mut File) -> anyhow::Result<Vec<u8>> {
    ensure!(file.metadata()?.is_file(), "path is not a regular file");
    let mut contents = Vec::new();
    file.take((MAX_FILE_LEN + 1) as u64)
        .read_to_end(&mut contents)?;
    ensure!(
        contents.len() <= MAX_FILE_LEN,
        "file exceeds {MAX_FILE_LEN} bytes"
    );
    Ok(contents)
}

fn revision(contents: &[u8]) -> Vec<u8> {
    Sha256::digest(contents).to_vec()
}

struct FileDetails {
    contents: Vec<u8>,
    revision: Vec<u8>,
    mode: u32,
}

fn checked_save_mismatch(
    expected: Option<&[u8]>,
    current: &Option<FileDetails>,
) -> Option<FileSaveResult> {
    let expected = expected?;
    match current {
        None if expected.is_empty() => None,
        None => Some(FileSaveResult::Deleted),
        Some(details) if details.revision == expected => None,
        Some(details) => Some(FileSaveResult::Conflict {
            contents: details.contents.clone(),
            revision: details.revision.clone(),
        }),
    }
}

struct TempFile {
    parent_fd: i32,
    name: CString,
    file: File,
    committed: bool,
}

impl TempFile {
    fn create(parent: &File, mode: Option<u32>) -> std::io::Result<Self> {
        static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        for _ in 0..16 {
            let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let name = CString::new(format!(".rho-save-{}-{id}", std::process::id())).unwrap();
            // SAFETY: parent and name are valid, and O_EXCL gives this value
            // unique ownership of a newly created descriptor.
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW,
                    0o666,
                )
            };
            if fd < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EEXIST) {
                    continue;
                }
                return Err(error);
            }
            // SAFETY: openat returned a new owned descriptor.
            let file = unsafe { File::from_raw_fd(fd) };
            let temp = Self {
                parent_fd: parent.as_raw_fd(),
                name,
                file,
                committed: false,
            };
            if let Some(mode) = mode {
                temp.file
                    .set_permissions(std::fs::Permissions::from_mode(mode))?;
            }
            return Ok(temp);
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a temporary save file",
        ))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.committed {
            // SAFETY: parent_fd remains alive longer than this value, and name
            // identifies only the temporary file created above.
            unsafe {
                libc::unlinkat(self.parent_fd, self.name.as_ptr(), 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        let files =
            WorkspaceFiles::open(Utf8PathBuf::from_path_buf(root.path().to_owned()).unwrap())
                .unwrap();

        assert!(matches!(
            files.read_sync(Utf8Path::new("../secret")),
            FileReadResult::Error(_)
        ));
        assert!(matches!(
            files.read_sync(Utf8Path::new("nested/../secret")),
            FileReadResult::Error(_)
        ));
        assert!(matches!(
            files.read_sync(Utf8Path::new("nested/./file")),
            FileReadResult::Error(_)
        ));
        assert!(matches!(
            files.read_sync(Utf8Path::new("escape/secret")),
            FileReadResult::Error(_)
        ));
        assert_eq!(
            std::fs::read(outside.path().join("secret")).unwrap(),
            b"secret"
        );
    }

    #[test]
    fn checked_save_reports_conflict_and_deletion() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), b"one").unwrap();
        let files =
            WorkspaceFiles::open(Utf8PathBuf::from_path_buf(root.path().to_owned()).unwrap())
                .unwrap();
        let FileReadResult::File { revision, .. } = files.read_sync(Utf8Path::new("file")) else {
            panic!("expected file")
        };
        std::fs::write(root.path().join("file"), b"two").unwrap();
        assert!(matches!(
            files.save_sync(Utf8Path::new("file"), Some(&revision), b"mine"),
            FileSaveResult::Conflict { contents, .. } if contents == b"two"
        ));
        std::fs::remove_file(root.path().join("file")).unwrap();
        assert_eq!(
            files.save_sync(Utf8Path::new("file"), Some(&revision), b"mine"),
            FileSaveResult::Deleted
        );

        assert!(matches!(
            files.save_sync(Utf8Path::new("new"), Some(&[]), b"created"),
            FileSaveResult::Saved { .. }
        ));
        assert_eq!(std::fs::read(root.path().join("new")).unwrap(), b"created");
        assert!(matches!(
            files.save_sync(Utf8Path::new("nested/dir/new"), Some(&[]), b"nested"),
            FileSaveResult::Saved { .. }
        ));
        assert_eq!(
            std::fs::read(root.path().join("nested/dir/new")).unwrap(),
            b"nested"
        );
    }

    #[test]
    fn save_atomically_replaces_contents_and_preserves_mode() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
        let files =
            WorkspaceFiles::open(Utf8PathBuf::from_path_buf(root.path().to_owned()).unwrap())
                .unwrap();
        let FileReadResult::File { revision, .. } = files.read_sync(Utf8Path::new("file")) else {
            panic!("expected file")
        };
        assert!(matches!(
            files.save_sync(Utf8Path::new("file"), Some(&revision), b"new"),
            FileSaveResult::Saved { .. }
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o777, 0o751);
        assert!(std::fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".rho-save-")
        }));
    }

    #[test]
    fn watcher_rescan_signal_is_not_lost() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        tx.try_send(WatchChange::Path(Utf8PathBuf::from("a")))
            .unwrap();
        tx.try_send(WatchChange::Rescan).unwrap();
        tx.try_send(WatchChange::Path(Utf8PathBuf::from("b")))
            .unwrap();
        let first = rx.try_recv().unwrap();
        let (paths, rescan) = drain_changes(first, &mut rx);
        assert_eq!(paths, [Utf8PathBuf::from("a"), Utf8PathBuf::from("b")]);
        assert!(rescan);
    }

    #[test]
    fn watcher_rescans_for_renames_and_removals() {
        use notify::event::{ModifyKind, RemoveKind, RenameMode};

        assert!(watcher_event_requires_rescan(&notify::EventKind::Modify(
            ModifyKind::Name(RenameMode::Both)
        )));
        assert!(watcher_event_requires_rescan(&notify::EventKind::Remove(
            RemoveKind::Folder
        )));
        assert!(!watcher_event_requires_rescan(&notify::EventKind::Modify(
            ModifyKind::Data(notify::event::DataChange::Content)
        )));
    }
}

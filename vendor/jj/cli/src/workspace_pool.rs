// Copyright 2026 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A pool of reusable checkout directories ("slots") for workspaces.
//!
//! A slot is an anonymous numbered directory under `.jj/ws-pool/`. It has no
//! identity of its own: it is bound to a workspace by `jj workspace pool
//! attach` and freed again by `jj workspace pool detach`. A freed slot keeps
//! whatever ignored files (build caches etc.) its last occupant left behind,
//! so reusing a slot is cheap. New slots can be seeded by copying
//! (reflinking when possible) the ignored files of an existing workspace.
//!
//! The pool directory listing is the only registry. Each slot `N/` has a
//! sibling lock file `N.lock` guarding concurrent attach/prepare; binding is
//! recorded solely in the workspace store.

use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use jj_lib::file_util;
use jj_lib::file_util::IoResultExt as _;
use jj_lib::file_util::PathError;
use jj_lib::gitignore::GitIgnoreError;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::lock::FileLock;
use jj_lib::lock::FileLockError;
use jj_lib::repo_path::RepoPathBuf;

impl From<WorkspacePoolError> for crate::command_error::CommandError {
    fn from(err: WorkspacePoolError) -> Self {
        crate::command_error::user_error(err)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspacePoolError {
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    FileLock(#[from] FileLockError),
    #[error(transparent)]
    GitIgnore(#[from] GitIgnoreError),
}

/// A slot directory in the pool.
#[derive(Clone, Debug)]
pub struct Slot {
    pub index: usize,
    /// Path of the slot directory (repo-derived, not canonicalized).
    pub path: PathBuf,
}

/// A slot locked for exclusive use while a workspace is being attached to it
/// or the slot is being prepared. The lock is transient: long-term occupancy
/// is recorded in the workspace store.
pub struct ClaimedSlot {
    pub slot: Slot,
    _lock: FileLock,
}

pub struct WorkspacePool {
    base: PathBuf,
}

impl WorkspacePool {
    pub fn new(repo_path: &Path) -> Self {
        // The parent() call is needed to not write under `.jj/repo/`.
        let base = repo_path.parent().unwrap().join("ws-pool");
        Self { base }
    }

    /// The path the workspace store records for slot `index`, relative to
    /// the repo path (`.jj/repo`). Occupancy comparisons use these relative
    /// paths only: absolute paths are namespace-dependent (a mount namespace
    /// may present a slot at a different absolute location).
    pub fn store_path(index: usize) -> PathBuf {
        Path::new("..").join("ws-pool").join(index.to_string())
    }

    /// Parses a workspace-store path as a slot of this pool.
    pub fn slot_index_of_store_path(path: &Path) -> Option<usize> {
        use std::path::Component;
        let mut components = path.components();
        match (
            components.next(),
            components.next(),
            components.next(),
            components.next(),
        ) {
            (
                Some(Component::ParentDir),
                Some(Component::Normal(dir)),
                Some(Component::Normal(index)),
                None,
            ) if dir == "ws-pool" => index.to_str()?.parse().ok(),
            _ => None,
        }
    }

    pub fn slots(&self) -> Result<Vec<Slot>, WorkspacePoolError> {
        let entries = match fs::read_dir(&self.base) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
            Err(err) => {
                return Err(PathError {
                    path: self.base.clone(),
                    source: err,
                }
                .into());
            }
        };
        let mut slots = vec![];
        for entry in entries {
            let entry = entry.context(&self.base)?;
            let Some(index) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<usize>().ok())
            else {
                continue;
            };
            if !entry.file_type().context(entry.path())?.is_dir() {
                continue;
            }
            slots.push(Slot {
                index,
                path: entry.path(),
            });
        }
        slots.sort_by_key(|slot| slot.index);
        Ok(slots)
    }

    /// Creates a new empty slot directory.
    pub fn create_slot(&self) -> Result<Slot, WorkspacePoolError> {
        fs::create_dir_all(&self.base).context(&self.base)?;
        // create_dir is atomic, so concurrent creators simply move on to the
        // next index.
        for index in 0.. {
            let path = self.base.join(index.to_string());
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Slot { index, path }),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(PathError { path, source: err }.into());
                }
            }
        }
        unreachable!();
    }

    /// Tries to lock the given slot. Returns `None` if another process holds
    /// it.
    pub fn try_claim(&self, slot: &Slot) -> Result<Option<ClaimedSlot>, WorkspacePoolError> {
        let lock_path = self.base.join(format!("{}.lock", slot.index));
        Ok(FileLock::try_lock(lock_path)?.map(|lock| ClaimedSlot {
            slot: slot.clone(),
            _lock: lock,
        }))
    }

    /// Claims a slot that is not in `occupied` (paths currently recorded in
    /// the workspace store, relative to the repo path).
    pub fn claim_free_slot(
        &self,
        occupied: &[PathBuf],
    ) -> Result<Option<ClaimedSlot>, WorkspacePoolError> {
        for slot in self.slots()? {
            if occupied.contains(&Self::store_path(slot.index)) {
                continue;
            }
            if let Some(claimed) = self.try_claim(&slot)? {
                return Ok(Some(claimed));
            }
        }
        Ok(None)
    }
}

/// Statistics from seeding a slot.
#[derive(Debug, Default)]
pub struct SeedStats {
    pub files: u64,
    pub bytes: u64,
    pub reflinked: bool,
}

/// Copies the gitignored files of `donor_root` into `target_root`,
/// reflinking file contents when the filesystem supports it.
///
/// Only ignored files are copied: tracked files will be written by the
/// checkout, and copying untracked-but-not-ignored files would silently add
/// them to the attached workspace's change on the next snapshot.
pub fn seed_ignored_files(
    donor_root: &Path,
    target_root: &Path,
    base_ignores: Arc<GitIgnoreFile>,
) -> Result<SeedStats, WorkspacePoolError> {
    let mut stats = SeedStats::default();
    // Whether reflinking works is a property of the (source, target)
    // filesystem pair; probe once and fall back to copying for the rest.
    let mut use_reflink = true;
    seed_dir(
        donor_root,
        target_root,
        RepoPathBuf::root(),
        base_ignores,
        &mut use_reflink,
        &mut stats,
    )?;
    stats.reflinked = use_reflink;
    Ok(stats)
}

fn seed_dir(
    donor_dir: &Path,
    target_dir: &Path,
    prefix: RepoPathBuf,
    ignores: Arc<GitIgnoreFile>,
    use_reflink: &mut bool,
    stats: &mut SeedStats,
) -> Result<(), WorkspacePoolError> {
    let ignores = ignores.chain_with_file(&prefix, donor_dir.join(".gitignore"))?;
    let entries = fs::read_dir(donor_dir).context(donor_dir)?;
    for entry in entries {
        let entry = entry.context(donor_dir)?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if prefix.is_root() && (name_str == ".jj" || name_str == ".git") {
            continue;
        }
        let Ok(component) = jj_lib::repo_path::RepoPathComponent::new(name_str) else {
            continue;
        };
        let path = prefix.join(component);
        let file_type = entry.file_type().context(&entry.path())?;
        if file_type.is_dir() {
            if ignores.matches_dir(&path) {
                copy_tree(&entry.path(), &target_dir.join(&name), use_reflink, stats)?;
            } else {
                seed_dir(
                    &entry.path(),
                    &target_dir.join(&name),
                    path,
                    ignores.clone(),
                    use_reflink,
                    stats,
                )?;
            }
        } else if ignores.matches_file(&path) {
            fs::create_dir_all(target_dir).context(target_dir)?;
            copy_entry(
                &entry.path(),
                &target_dir.join(&name),
                file_type,
                use_reflink,
                stats,
            )?;
        }
    }
    Ok(())
}

/// Copies an entire (ignored) directory tree without consulting ignore files.
fn copy_tree(
    source: &Path,
    target: &Path,
    use_reflink: &mut bool,
    stats: &mut SeedStats,
) -> Result<(), WorkspacePoolError> {
    fs::create_dir_all(target).context(target)?;
    let entries = fs::read_dir(source).context(source)?;
    for entry in entries {
        let entry = entry.context(source)?;
        let file_type = entry.file_type().context(&entry.path())?;
        let target = target.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target, use_reflink, stats)?;
        } else {
            copy_entry(&entry.path(), &target, file_type, use_reflink, stats)?;
        }
    }
    Ok(())
}

fn copy_entry(
    source: &Path,
    target: &Path,
    file_type: fs::FileType,
    use_reflink: &mut bool,
    stats: &mut SeedStats,
) -> Result<(), WorkspacePoolError> {
    if file_type.is_symlink() {
        let link_target = fs::read_link(source).context(source)?;
        match file_util::symlink_file(&link_target, target) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => {
                return Err(PathError {
                    path: target.to_path_buf(),
                    source: err,
                }
                .into());
            }
        }
        stats.files += 1;
        return Ok(());
    }
    let bytes = if *use_reflink {
        match reflink_file(source, target) {
            Ok(bytes) => bytes,
            Err(_) => {
                *use_reflink = false;
                fs::copy(source, target).context(target)?
            }
        }
    } else {
        fs::copy(source, target).context(target)?
    };
    stats.files += 1;
    stats.bytes += bytes;
    Ok(())
}

#[cfg(target_os = "linux")]
fn reflink_file(source: &Path, target: &Path) -> io::Result<u64> {
    let src = fs::File::open(source)?;
    let metadata = src.metadata()?;
    let dst = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(target)?;
    rustix::fs::ioctl_ficlone(&dst, &src)?;
    dst.set_permissions(metadata.permissions())?;
    Ok(metadata.len())
}

#[cfg(not(target_os = "linux"))]
fn reflink_file(_source: &Path, _target: &Path) -> io::Result<u64> {
    Err(io::Error::other("reflink not supported on this platform"))
}

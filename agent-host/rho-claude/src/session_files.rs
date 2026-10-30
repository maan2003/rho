//! Removal of files whose paths identify one Claude session, not a project.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;

use anyhow::{Context as _, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use uuid::Uuid;

use crate::accounts::ClaudePaths;

/// Delete Claude Code's session-owned files for exactly `session_ids`.
///
/// Coverage (Claude Code's on-disk layout, inspected in 2.1.308):
/// - Every real project directory in [`ClaudePaths::projects`]: `<uuid>.jsonl`
///   and the whole `<uuid>/` tree (subagents, tool results). This includes old
///   project copies left by moving an agent.
/// - In the config home and each real account directory:
///   `file-history/<uuid>/`, `session-env/<uuid>/`, `tasks/<uuid>/`,
///   `uploads/<uuid>/`, `image-cache/<uuid>/`, `debug/<uuid>.txt`,
///   `dump-prompts/<uuid>.jsonl`, and `startup-perf/<uuid>.{txt,json}`.
///
/// Account `projects/` paths are deliberately not inspected: agents mount the
/// canonical projects tree there. Project directories and all shared state
/// remain, including credentials, settings, project memory, history.jsonl,
/// plans, shell snapshots, caches not listed above, and PID-keyed `sessions/`.
/// No content-based guessing or search outside these explicit paths occurs.
///
/// Directory symlinks in project/account listings are skipped. A symlink at
/// an explicit root or intermediate directory is an error; a matching leaf
/// symlink is unlinked, never followed (including inside a session subtree).
/// Callers must stop session writers before calling: this is not atomic or
/// synchronized against concurrent filesystem replacement.
///
/// Returns the sorted paths actually removed, one per transcript, auxiliary
/// file, or whole session subtree, not one per descendant. Missing paths are
/// a no-op. Other I/O errors propagate; earlier removals are not rolled back.
/// No environment variables or implicit user paths are consulted.
pub async fn delete_session_files(
    paths: &ClaudePaths,
    session_ids: &[Uuid],
) -> Result<Vec<Utf8PathBuf>> {
    if session_ids.is_empty() {
        return Ok(Vec::new());
    }
    let paths = paths.clone();
    let session_ids: BTreeSet<_> = session_ids.iter().copied().collect();
    tokio::task::spawn_blocking(move || delete_files(&paths, &session_ids))
        .await
        .context("join Claude session file deletion")?
}

fn delete_files(paths: &ClaudePaths, session_ids: &BTreeSet<Uuid>) -> Result<Vec<Utf8PathBuf>> {
    let mut deleted = Vec::new();
    for project in real_child_directories(&paths.projects())? {
        for id in session_ids {
            remove(&project.join(format!("{id}.jsonl")), &mut deleted)?;
            remove(&project.join(id.to_string()), &mut deleted)?;
        }
    }

    let mut homes = vec![paths.config_home().to_owned()];
    if real_directory(paths.accounts_root())? {
        for account in paths.list()? {
            homes.push(paths.account_dir(&account)?);
        }
    }
    for home in homes {
        if !real_directory(&home)? {
            continue;
        }
        for directory in [
            "file-history",
            "session-env",
            "tasks",
            "uploads",
            "image-cache",
        ] {
            let root = home.join(directory);
            if real_directory(&root)? {
                for id in session_ids {
                    remove(&root.join(id.to_string()), &mut deleted)?;
                }
            }
        }
        for (directory, extension) in [
            ("debug", "txt"),
            ("dump-prompts", "jsonl"),
            ("startup-perf", "txt"),
            ("startup-perf", "json"),
        ] {
            let root = home.join(directory);
            if real_directory(&root)? {
                for id in session_ids {
                    remove(&root.join(format!("{id}.{extension}")), &mut deleted)?;
                }
            }
        }
    }
    deleted.sort();
    Ok(deleted)
}

/// Check ancestors too: checking only `projects/` would still follow a
/// symlink config home (and checking only the home misses symlink parents).
fn real_directory(path: &Utf8Path) -> Result<bool> {
    for ancestor in path.ancestors() {
        if ancestor.as_str().is_empty() {
            continue;
        }
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("refuse Claude session deletion through symlink directory {ancestor}");
            }
            Ok(metadata) if !metadata.is_dir() => {
                bail!("expected Claude directory at {ancestor}");
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).with_context(|| format!("stat Claude directory {ancestor}"));
            }
        }
    }
    Ok(true)
}

fn real_child_directories(root: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    if !real_directory(root)? {
        return Ok(Vec::new());
    }
    let mut directories = Vec::new();
    for entry in fs::read_dir(root).with_context(|| format!("read Claude directory {root}"))? {
        let entry = entry.with_context(|| format!("read entry in Claude directory {root}"))?;
        if entry
            .file_type()
            .context("stat Claude directory entry")?
            .is_dir()
        {
            directories.push(
                Utf8PathBuf::from_path_buf(entry.path())
                    .map_err(|path| anyhow::anyhow!("non-UTF8 Claude directory: {path:?}"))?,
            );
        }
    }
    directories.sort();
    Ok(directories)
}

fn remove(path: &Utf8Path, deleted: &mut Vec<Utf8PathBuf>) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("stat Claude session path {path}"));
        }
    };
    let result = if metadata.is_dir() {
        // std's recursive remover does not follow symlinks within the tree.
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match result {
        Ok(()) => deleted.push(path.to_owned()),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("delete Claude session path {path}"));
        }
    }
    Ok(())
}

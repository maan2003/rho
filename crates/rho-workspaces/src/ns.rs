//! Mount-namespace plumbing: the daemon-wide user namespace, per-workspace
//! mount namespaces held in fds, and the per-repo alias mounts that keep jj's
//! `.jj/repo` pointers resolvable from every namespace.

use std::fs::File;
use std::os::fd::{AsFd as _, OwnedFd};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use rustix::fs::CWD;
use rustix::mount::{
    MountPropagationFlags, MoveMountFlags, OpenTreeFlags, mount_change, move_mount, open_tree,
};
use rustix::thread::{UnshareFlags, unshare_unsafe};

/// Moves the daemon into its own user + mount namespace, mapped to the
/// current user and holding `CAP_SYS_ADMIN` over all mounts made after this
/// point. Workspaces require it: agents whose real cwd contradicts the paths
/// baked into their context are worse than a dead daemon, so callers should
/// treat failure as fatal rather than degrade.
///
/// # Safety
///
/// The process must be single-threaded: `unshare(CLONE_NEWUSER)` fails on a
/// threaded process, but the deeper contract is that this rewires
/// process-wide state — every subsequent thread inherits the new namespaces,
/// and code that already captured pre-namespace state (open directory fds,
/// resolved paths, cached credentials) would silently disagree with code
/// running after. Call it once, at the top of `main`, before the tokio
/// runtime or any other thread exists.
pub unsafe fn init_daemon_namespace() -> anyhow::Result<()> {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    // SAFETY: single-threaded per this function's contract.
    unsafe { unshare_unsafe(UnshareFlags::NEWUSER | UnshareFlags::NEWNS) }
        .context("unshare user+mount namespace")?;
    std::fs::write("/proc/self/setgroups", "deny").context("deny setgroups")?;
    std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1")).context("write uid_map")?;
    std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1")).context("write gid_map")?;
    // Host mounts keep flowing in; nothing the daemon mounts leaks back out.
    mount_change(
        "/",
        MountPropagationFlags::DOWNSTREAM | MountPropagationFlags::REC,
    )
    .context("make mount tree a recursive slave")?;
    Ok(())
}

/// The escape hatch back to the origin repo: slot pointers reference
/// `<origin>/.jj/ws-parent/…`, which must resolve to the origin from *three*
/// namespaces. On the host and in the daemon's namespace it does via a
/// symlink in the origin's `.jj` pointing back at the repo itself (so no
/// mounts, and nothing to re-establish after a daemon restart). In an
/// agent's namespace the origin path is covered by the slot, so the same
/// path lands on the slot's plain `.jj/ws-parent` directory — where
/// [`create_workspace_ns`] binds the real origin.
pub const WS_PARENT: &str = "ws-parent";

pub struct ViewMount {
    pub repo: PathBuf,
    pub slot: PathBuf,
    pub metadata_masks: Option<(PathBuf, PathBuf)>,
}

/// Creates the mount namespace for one agent view: a copy of the daemon's
/// namespace with each entry's slot checkout mounted over its origin repo
/// path, and each origin bound back in at `.jj/ws-parent` for the slot's
/// repo pointers. Runs on a dedicated thread because the thread ends up
/// permanently inside the new namespace — the returned
/// `/proc/thread-self/ns/mnt` fd is what keeps it alive after the thread
/// exits.
pub fn create_view_ns(mounts: Vec<ViewMount>) -> anyhow::Result<OwnedFd> {
    std::thread::spawn(move || -> anyhow::Result<OwnedFd> {
        // SAFETY: NEWNS implies unsharing fs state for this thread only; the
        // thread exits immediately after and shares nothing else.
        unsafe { unshare_unsafe(UnshareFlags::NEWNS) }.context("unshare mount namespace")?;
        for mount in &mounts {
            cover_origin_with_slot(&mount.repo, &mount.slot, mount.metadata_masks.is_none())?;
            if let Some((jj_mask, git_mask)) = &mount.metadata_masks {
                rustix::mount::mount_bind(jj_mask, mount.repo.join(".jj"))
                    .context("mask sandbox .jj metadata")?;
                // Pure jj repositories have no colocated `.git` entry.
                if mount.repo.join(".git").exists() {
                    rustix::mount::mount_bind(git_mask, mount.repo.join(".git"))
                        .context("mask sandbox .git metadata")?;
                }
            }
        }
        let fd = File::open("/proc/thread-self/ns/mnt").context("open mount namespace fd")?;
        Ok(fd.into())
    })
    .join()
    .expect("view namespace thread panicked")
}

/// The per-entry mount dance, in whatever namespace this thread is in:
/// mount the slot checkout over the origin repo path and bind the origin
/// back in at the slot's `.jj/ws-parent`.
fn cover_origin_with_slot(repo: &Path, slot: &Path, bind_origin_back: bool) -> anyhow::Result<()> {
    // Clone both trees before the cover mount hides the origin.
    let clone = |path: &Path| {
        open_tree(
            CWD,
            path,
            OpenTreeFlags::OPEN_TREE_CLONE
                | OpenTreeFlags::AT_RECURSIVE
                | OpenTreeFlags::OPEN_TREE_CLOEXEC,
        )
    };
    let origin_tree = bind_origin_back
        .then(|| clone(repo).context("open repo tree"))
        .transpose()?;
    let slot_tree = clone(slot).context("open slot tree")?;
    move_mount(
        slot_tree.as_fd(),
        "",
        CWD,
        repo,
        MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
    )
    .context("mount slot over repo path")?;
    // The path now resolves inside the slot: its empty ws-parent dir.
    if let Some(origin_tree) = origin_tree {
        move_mount(
            origin_tree.as_fd(),
            "",
            CWD,
            repo.join(".jj").join(WS_PARENT),
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
        )
        .context("bind origin at ws-parent")?;
    }
    Ok(())
}

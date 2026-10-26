//! The agent filesystem view: a private mount namespace with the host root,
//! the workset mounted at `/src`, and the clone-store root mounted read-only.
//!
//! This is a layout, not a sandbox: everything runs as the invoking user,
//! and no security boundary is claimed.

use std::ffi::{CString, OsStr};
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::{fs, io};

use anyhow::{Context as _, ensure};

/// Where the workset directory appears to an agent.
pub const MOUNT_ROOT: &str = "/src";

/// What a namespace mounts: the workset directory at the visible root, and
/// the mirror store root and its keeper's socket at their host paths, so
/// the absolute paths clones record (alternates) and the environment
/// names hold inside.
#[derive(Clone, Debug)]
pub struct Mounts {
    /// The workset directory, mounted read-write at the visible root.
    pub src: PathBuf,
    /// The mirror store root, mounted read-only at its own host path.
    pub store_root: PathBuf,
    /// The keeper's socket, mounted at its own host path.
    pub store_socket: Option<PathBuf>,
}

/// The host view as the invoking user, with a namespace-local `/src` for the
/// workset and the store root made read-only. No host mount stub is needed.
/// Existing host paths and their submounts remain visible. New top-level host
/// entries created after construction are not reflected in this private root.
pub struct ExposedBuilder {
    mounts: Mounts,
}

impl ExposedBuilder {
    pub fn new(mounts: Mounts) -> anyhow::Result<Self> {
        validate_mounts(&mounts)?;
        Ok(Self { mounts })
    }

    /// Reconstructs the host root in the current private mount namespace,
    /// then mounts the workset at `/src`. Call only in a single-threaded
    /// process after unsharing user and mount namespaces. Changes root and cwd.
    pub fn build(&self) -> anyhow::Result<()> {
        // Like bubblewrap, use two pivots: first move the staging tmpfs out of
        // the host tree, then build the final root from the detached host root.
        // Otherwise a recursive bind of /tmp would include our own new root.
        // Mounting over the existing /tmp creates no host files or directories.
        let staging = fs::canonicalize("/tmp").context("resolve root staging directory")?;
        mount_fs(
            Some(OsStr::new("tmpfs")),
            &staging,
            Some("tmpfs"),
            libc::MS_NOSUID | libc::MS_NODEV,
            Some("mode=0755"),
        )
        .context("mount private root staging tmpfs")?;
        std::env::set_current_dir(&staging)?;
        fs::create_dir("newroot")?;
        fs::create_dir("oldroot")?;
        bind(Path::new("newroot"), Path::new("newroot"), false)?;
        pivot_root(&staging, Path::new("oldroot"))?;
        std::env::set_current_dir("/")?;

        for entry in fs::read_dir("/oldroot")? {
            let entry = entry?;
            if entry.file_name() == MOUNT_ROOT.trim_start_matches('/') {
                continue;
            }
            let target = Path::new("/newroot").join(entry.file_name());
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                std::os::unix::fs::symlink(fs::read_link(entry.path())?, &target)?;
            } else {
                if kind.is_dir() {
                    fs::create_dir(&target)?;
                } else {
                    fs::File::create(&target)?;
                }
                bind(&entry.path(), &target, false)?;
            }
        }
        fs::create_dir("/newroot/src")?;
        unmount(Path::new("/oldroot"))?;

        // Stacking the old staging root on the final root avoids a temporary
        // put_old directory in the user's workset or any host-backed directory.
        {
            let oldroot = fs::File::open("/")?;
            std::env::set_current_dir("/newroot")?;
            pivot_root(Path::new("."), Path::new("."))?;
            cvt(unsafe { libc::fchdir(oldroot.as_raw_fd()) }).context("chdir to staging root")?;
            unmount(Path::new("."))?;
            std::env::set_current_dir("/")?;
        }
        mount_in_place(&self.mounts, Path::new("/"))
    }
}

fn validate_mounts(set: &Mounts) -> anyhow::Result<()> {
    ensure!(
        set.src.is_dir(),
        "workset directory is missing: {}",
        set.src.display()
    );
    for (what, path) in [("store root", &set.store_root)].into_iter().chain(
        set.store_socket
            .iter()
            .map(|socket| ("store socket", socket)),
    ) {
        ensure!(
            path.is_absolute(),
            "{what} must be an absolute path: {}",
            path.display()
        );
        ensure!(path.exists(), "{what} is missing: {}", path.display());
    }
    ensure!(
        set.store_root.is_dir(),
        "store root is not a directory: {}",
        set.store_root.display()
    );
    Ok(())
}

/// Establishes the process-wide identity user namespace. Call before starting
/// any threads; individual worker threads can then create private mount
/// namespaces with [`unshare_mount_namespace`].
pub fn unshare_identity_user_namespace() -> anyhow::Result<()> {
    let identity_uid = unsafe { libc::getuid() };
    let identity_gid = unsafe { libc::getgid() };
    cvt(unsafe { libc::unshare(libc::CLONE_NEWUSER) }).context("unshare user namespace")?;
    fs::write(
        "/proc/self/uid_map",
        format!("{identity_uid} {identity_uid} 1\n"),
    )
    .context("write identity uid_map")?;
    fs::write("/proc/self/setgroups", "deny").context("deny setgroups")?;
    fs::write(
        "/proc/self/gid_map",
        format!("{identity_gid} {identity_gid} 1\n"),
    )
    .context("write identity gid_map")?;
    Ok(())
}

pub fn unshare_mount_namespace() -> anyhow::Result<()> {
    unshare_fs_attributes()?;
    cvt(unsafe { libc::unshare(libc::CLONE_NEWNS) }).context("unshare mount namespace")?;
    mount_fs(
        None,
        Path::new("/"),
        None,
        libc::MS_REC | libc::MS_PRIVATE,
        None,
    )
    .context("make mounts recursively private")?;
    Ok(())
}

/// Detaches the calling thread's cwd/root state before it changes or enters a
/// mount namespace. Required for both `unshare(CLONE_NEWNS)` and `setns` from
/// a pthread, whose fs_struct is shared by default.
pub fn unshare_fs_attributes() -> anyhow::Result<()> {
    cvt(unsafe { libc::unshare(libc::CLONE_FS) })
        .context("unshare filesystem attributes")
        .map(|_| ())
}

/// Mounts a validated workset below `root` in the current mount namespace:
/// the workset directory at [`MOUNT_ROOT`], the store root read-only at its
/// host path, and the socket at its host path.
pub fn mount_in_place(set: &Mounts, root: &Path) -> anyhow::Result<()> {
    validate_mounts(set)?;
    bind(&set.src, &mount_root(root), false)?;
    let store_root = host_path_in(root, &set.store_root);
    fs::create_dir_all(&store_root).with_context(|| format!("create {}", store_root.display()))?;
    bind(&set.store_root, &store_root, true)?;
    if let Some(socket) = &set.store_socket {
        let target = host_path_in(root, socket);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        if !target.exists() {
            fs::File::create(&target)
                .with_context(|| format!("create socket mount point {}", target.display()))?;
        }
        bind(socket, &target, false)?;
    }
    Ok(())
}

/// Where the host path `path` lands below the namespace root being built.
fn host_path_in(root: &Path, path: &Path) -> PathBuf {
    root.join(path.strip_prefix("/").unwrap_or(path))
}

fn bind(source: &Path, target: &Path, readonly: bool) -> anyhow::Result<()> {
    mount_fs(
        Some(source.as_os_str()),
        target,
        None,
        libc::MS_BIND | libc::MS_REC,
        None,
    )
    .with_context(|| format!("bind {} at {}", source.display(), target.display()))?;
    set_mount_attributes(target, readonly)
}

fn set_mount_attributes(target: &Path, readonly: bool) -> anyhow::Result<()> {
    // Setting flags via mount_setattr (bind root and MS_REC-cloned submounts
    // alike) never clears anything, so it cannot collide with flags the user
    // namespace holds locked — unlike MS_REMOUNT|MS_BIND, which must repeat
    // every locked flag or fail.
    let attr = libc::mount_attr {
        attr_set: libc::MOUNT_ATTR_NOSUID | if readonly { libc::MOUNT_ATTR_RDONLY } else { 0 },
        // A writable bind root must not relax read-only nested mounts.
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };
    let target_c = cstring(target)?;
    const AT_RECURSIVE: libc::c_uint = 0x8000;
    let result = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            libc::AT_FDCWD,
            target_c.as_ptr(),
            AT_RECURSIVE,
            &attr,
            std::mem::size_of::<libc::mount_attr>(),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("set recursive mount attributes on {}", target.display()));
    }
    Ok(())
}

fn mount_fs(
    source: Option<&OsStr>,
    target: &Path,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> anyhow::Result<()> {
    let source = source
        .map(|s| CString::new(s.as_bytes()))
        .transpose()
        .context("mount source contains NUL")?;
    let target = cstring(target)?;
    let fstype = fstype.map(CString::new).transpose()?;
    let data = data.map(CString::new).transpose()?;
    cvt(unsafe {
        libc::mount(
            source.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            target.as_ptr(),
            fstype.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            flags,
            data.as_ref()
                .map_or(std::ptr::null(), |s| s.as_ptr().cast()),
        )
    })?;
    Ok(())
}

fn pivot_root(new_root: &Path, old_root: &Path) -> anyhow::Result<()> {
    let new_root = cstring(new_root)?;
    let old_root = cstring(old_root)?;
    cvt(unsafe {
        libc::syscall(libc::SYS_pivot_root, new_root.as_ptr(), old_root.as_ptr()) as i32
    })
    .context("pivot root")?;
    Ok(())
}

fn unmount(path: &Path) -> anyhow::Result<()> {
    let target = cstring(path)?;
    cvt(unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) })
        .with_context(|| format!("detach {}", path.display()))?;
    Ok(())
}

fn cstring(path: &Path) -> anyhow::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).context("path contains NUL")
}

/// Where [`MOUNT_ROOT`] lands below the namespace root being built.
fn mount_root(root: &Path) -> PathBuf {
    root.join(MOUNT_ROOT.trim_start_matches('/'))
}

fn cvt(value: i32) -> io::Result<i32> {
    if value < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}

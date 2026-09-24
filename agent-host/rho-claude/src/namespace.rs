//! Claude-only child mount overlays. The workset's namespace is never changed.
use std::ffi::CString;
use std::fs::File;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use anyhow::Context as _;

/// Install after the command's workset-entry setup. Everything allocating or
/// opening files happens before fork; the child only performs namespace
/// syscalls.
pub fn prepare(
    command: &mut tokio::process::Command,
    target: &Path,
    account: &Path,
    projects: &Path,
    prompt: &Path,
    settings: Option<&Path>,
) -> anyhow::Result<()> {
    let mut sources = vec![
        (account, target.to_owned()),
        (projects, target.join("projects")),
        (prompt, target.join("CLAUDE.md")),
    ];
    if let Some(settings) = settings {
        sources.push((settings, target.join("settings.json")));
    }
    let mounts = sources
        .into_iter()
        .map(|(source, target)| {
            let file = File::open(source)
                .with_context(|| format!("open Claude mount source {}", source.display()))?;
            // Clone the subtree directly from its open path. Unlike a bind through
            // /proc/self/fd, a detached tree can cross the child's namespace clone.
            let tree = unsafe {
                libc::syscall(
                    libc::SYS_open_tree,
                    file.as_raw_fd(),
                    c"".as_ptr(),
                    1_u32 | libc::O_CLOEXEC as u32 | libc::AT_EMPTY_PATH as u32 | 0x8000_u32,
                )
            };
            if tree < 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("capture Claude mount {}", source.display()));
            }
            let tree = unsafe { OwnedFd::from_raw_fd(tree as i32) };
            let target = CString::new(target.as_os_str().as_bytes())?;
            Ok((tree, target))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut directories = target
        .ancestors()
        .map(|path| CString::new(path.as_os_str().as_bytes()))
        .collect::<Result<Vec<_>, _>>()?;
    directories.reverse();

    unsafe {
        command.pre_exec(move || {
            // Own the descriptors until spawn completes; merely capturing raw
            // numbers would allow their reuse before this closure executes.
            let _owned_mounts = &mounts;
            if libc::unshare(libc::CLONE_NEWNS) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            for directory in &directories {
                if libc::mkdir(directory.as_ptr(), 0o700) != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::EEXIST) {
                        return Err(error);
                    }
                }
            }
            let attributes = libc::mount_attr {
                attr_set: libc::MOUNT_ATTR_NOSUID,
                attr_clr: 0,
                propagation: 0,
                userns_fd: 0,
            };
            for (tree, target) in &mounts {
                if libc::syscall(
                    libc::SYS_move_mount,
                    tree.as_raw_fd(),
                    c"".as_ptr(),
                    libc::AT_FDCWD,
                    target.as_ptr(),
                    4_u32,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::syscall(
                    libc::SYS_mount_setattr,
                    libc::AT_FDCWD,
                    target.as_ptr(),
                    0x8000_u32,
                    &attributes,
                    std::mem::size_of::<libc::mount_attr>(),
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}

// Called while the workset process is still single-threaded and before pivot.
// These are backing sources for the parent runtime, not its CLI home overlay.
pub fn install_sources(
    paths: &crate::accounts::ClaudePaths,
    root: &std::path::Path,
    state: &std::path::Path,
    config_home: camino::Utf8PathBuf,
) -> anyhow::Result<crate::accounts::ClaudePaths> {
    let internal = state.join("claude");
    let accounts = internal.join("accounts");
    let projects = internal.join("projects");
    for (source, target) in [
        (
            paths.accounts_root().as_std_path().to_owned(),
            accounts.clone(),
        ),
        (paths.projects().into_std_path_buf(), projects.clone()),
    ] {
        let target = root.join(target.strip_prefix("/")?);
        std::fs::create_dir_all(&target)?;
        std::fs::create_dir_all(&source)?;
        {
            let source =
                std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(source.as_os_str()))?;
            let target =
                std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(target.as_os_str()))?;
            if unsafe {
                libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND,
                    std::ptr::null(),
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
    }
    Ok(paths.with_sources(
        config_home,
        camino::Utf8PathBuf::from_path_buf(accounts)
            .map_err(|_| anyhow::anyhow!("non-UTF8 account mount"))?,
        camino::Utf8PathBuf::from_path_buf(projects)
            .map_err(|_| anyhow::anyhow!("non-UTF8 projects mount"))?,
    ))
}

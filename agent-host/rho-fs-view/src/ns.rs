use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io::Read as _;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};

use crate::layout::MOUNT_ROOT;
use crate::{PathOverrides, UserEnvironment, Workset};

/// Largest file [`read_file_bounded`] will ever return.
pub const MAX_BOUNDED_READ: usize = 64 * 1024 * 1024;

/// Filesystem inputs for the one execution process of a workset.
/// The agent host supplies paths; the single-threaded child builds the mounts.
#[derive(Clone, Debug, senax_encoder::Encode, senax_encoder::Decode)]
pub struct WorksetLayout {
    pub workset: String,
    pub source: Utf8PathBuf,
    pub store_root: Utf8PathBuf,
    pub store_socket: Option<Utf8PathBuf>,
    pub cache: Utf8PathBuf,
    pub state: Utf8PathBuf,
    pub paths: PathOverrides,
}

impl WorksetLayout {
    pub fn new(workset: &Workset) -> anyhow::Result<Self> {
        let owner = workset.owner()?;
        Ok(Self {
            workset: workset.id().to_owned(),
            source: workset.root().to_owned(),
            store_root: owner.store_root(),
            store_socket: owner.store_socket(),
            cache: owner.cache_dir(),
            state: workset.state_dir()?,
            paths: owner.path_overrides.clone(),
        })
    }

    /// # Safety
    /// Only call in a fresh process before starting any threads.
    pub unsafe fn build(&self) -> anyhow::Result<()> {
        crate::layout::unshare_identity_user_namespace()?;
        crate::layout::unshare_mount_namespace()?;
        std::fs::create_dir_all(&self.state)?;
        let mounts = crate::layout::Mounts {
            src: self.source.clone().into_std_path_buf(),
            store_root: self.store_root.clone().into_std_path_buf(),
            store_socket: self
                .store_socket
                .clone()
                .map(Utf8PathBuf::into_std_path_buf),
        };
        crate::layout::ExposedBuilder::new(mounts)?.build_in_place(Path::new("/"))?;
        Ok(())
    }

    /// Finish startup after provider-owned source mounts have been installed.
    ///
    /// # Safety
    /// Only call before starting threads: this changes root, cwd and
    /// environment.
    pub unsafe fn enter(&self) -> anyhow::Result<()> {
        close_inherited_fds_on_exec()?;
        let environment = UserEnvironment::new(std::env::vars_os().collect());
        std::env::set_current_dir(MOUNT_ROOT)?;
        let mut command = tokio::process::Command::new("");
        self.configure_environment(&environment, &mut command);
        for (key, _) in std::env::vars_os() {
            unsafe {
                std::env::remove_var(key);
            }
        }
        for (key, value) in command.as_std().get_envs() {
            if let Some(value) = value {
                unsafe {
                    std::env::set_var(key, value);
                }
            }
        }
        Ok(())
    }

    fn configure_environment(
        &self,
        environment: &UserEnvironment,
        command: &mut tokio::process::Command,
    ) {
        // The user's environment, with Rho's git ahead of theirs.
        command.envs(environment.0.iter().map(|(name, value)| (name, value)));
        let path = environment.get("PATH").map(|path| self.paths.add_to(path));
        command.env("PATH", prepend_path(&crate::git_dir(), path.as_deref()));
        if let Some(find) = crate::FIND_BIN {
            command.env("RHO_DEVSHELL_PATH_PREFIX", find);
        }
        // The cargo ahead of a dev shell's own.
        if let Some(cargo) = crate::SHARED_CARGO_BIN {
            command.env("RHO_DEVSHELL_CARGO", cargo);
        }
        if let Some(socket) = &self.store_socket {
            command.env(crate::SOCKET_ENV, socket);
        }
    }
}

/// See [`crate::Worksets::devshell_cache_dir`].
pub(crate) fn devshell_cache(cache: &Utf8Path) -> Utf8PathBuf {
    cache.join("rho-devshell")
}

/// Read a regular file at an absolute path beneath `root`, refusing lexical
/// escapes, symlinks that escape `root`, and files above the bounded limit.
pub async fn read_file_bounded(
    root: &Utf8Path,
    path: &Path,
    max_len: usize,
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        max_len <= MAX_BOUNDED_READ,
        "read limit {max_len} exceeds {MAX_BOUNDED_READ} bytes"
    );
    anyhow::ensure!(root.is_absolute(), "root must be absolute: {root}");
    let path = Utf8Path::from_path(path).context("path is not valid UTF-8")?;
    anyhow::ensure!(path.is_absolute(), "file path must be absolute: {path}");
    let relative = crate::visible_relative(root.as_str(), path)?
        .as_std_path()
        .to_owned();
    let root = root.to_owned();
    let display = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut file = open_beneath(root.as_std_path(), &relative)
            .with_context(|| format!("open {display}"))?;
        let metadata = file.metadata()?;
        anyhow::ensure!(metadata.is_file(), "not a regular file: {display}");
        anyhow::ensure!(
            metadata.len() <= max_len as u64,
            "{display} is {} bytes, over the {max_len} byte limit",
            metadata.len()
        );
        let mut contents = Vec::with_capacity(metadata.len() as usize);
        file.by_ref()
            .take(max_len as u64 + 1)
            .read_to_end(&mut contents)?;
        anyhow::ensure!(
            contents.len() <= max_len,
            "{display} grew past the {max_len} byte limit while reading"
        );
        Ok(contents)
    })
    .await
    .context("bounded read task panicked")?
}

/// `PATH` with `bin` first (and not repeated later).
fn prepend_path(bin: &Path, path: Option<&OsStr>) -> OsString {
    let rest = path
        .map(|path| {
            std::env::split_paths(path)
                .filter(|entry| entry != bin)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    std::env::join_paths(std::iter::once(bin.to_owned()).chain(rest))
        .unwrap_or_else(|_| bin.as_os_str().to_owned())
}

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// Opens `path` below `root` with the kernel refusing any resolution step
/// (symlink, `..`, magic link) that would leave `root`. Symlinks that stay
/// inside are followed.
fn open_beneath(root: &Path, path: &Path) -> anyhow::Result<File> {
    let root = File::open(root).with_context(|| format!("open workset {}", root.display()))?;
    let path =
        CString::new(path.as_os_str().as_bytes()).context("file path contains a NUL byte")?;
    const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
    const RESOLVE_BENEATH: u64 = 0x08;
    let how = OpenHow {
        flags: (libc::O_RDONLY | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: RESOLVE_NO_MAGICLINKS | RESOLVE_BENEATH,
    };
    // SAFETY: `path` and `how` outlive the syscall; a non-negative result is
    // a descriptor this process now owns.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root.as_raw_fd(),
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    } as i32;
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(crate) fn close_inherited_fds_on_exec() -> std::io::Result<()> {
    if unsafe { libc::close_range(3, u32::MAX, libc::CLOSE_RANGE_CLOEXEC as libc::c_int) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

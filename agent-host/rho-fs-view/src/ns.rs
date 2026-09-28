use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io::Read as _;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use rho_agent_types::WorksetMode;

use crate::layout::MOUNT_ROOT;
use crate::{PathOverrides, UserEnvironment, Workset};

/// How an agent sees its workset.
#[derive(Clone, Debug, senax_encoder::Encode, senax_encoder::Decode)]
pub enum Mode {
    /// A fresh tmpfs root holding `/nix/store`, a generated `/etc`, an empty
    /// `$HOME` (seeded from `home_skeleton`), and the workset at `/src`.
    View { home_skeleton: Option<PathBuf> },
    /// The full host view, plus the workset at `/src` over the host stub.
    Exposed,
}

impl Mode {
    pub fn from_workset_mode(mode: WorksetMode) -> Self {
        match mode {
            WorksetMode::View => Self::View {
                home_skeleton: None,
            },
            WorksetMode::Exposed => Self::Exposed,
        }
    }

    /// The persisted form of this mode.
    pub fn workset_mode(&self) -> WorksetMode {
        match self {
            Self::View { .. } => WorksetMode::View,
            Self::Exposed => WorksetMode::Exposed,
        }
    }
}

/// Largest file [`read_file_bounded`] will ever return.
pub const MAX_BOUNDED_READ: usize = 64 * 1024 * 1024;

/// Filesystem inputs for the one execution process of a workset.
/// The agent host supplies paths; the single-threaded child builds the mounts.
#[derive(Clone, Debug, senax_encoder::Encode, senax_encoder::Decode)]
pub struct WorksetLayout {
    pub workset: String,
    pub mode: Mode,
    pub source: Utf8PathBuf,
    pub store_root: Utf8PathBuf,
    pub store_socket: Option<Utf8PathBuf>,
    pub root: Utf8PathBuf,
    pub cache: Utf8PathBuf,
    pub state: Utf8PathBuf,
    pub paths: PathOverrides,
    pub identity: Vec<(String, String)>,
}

impl WorksetLayout {
    pub fn new(workset: &Workset, mode: Mode, root: Utf8PathBuf) -> anyhow::Result<Self> {
        let owner = workset.owner()?;
        Ok(Self {
            workset: workset.id().to_owned(),
            mode,
            source: workset.root().to_owned(),
            store_root: owner.store_root(),
            store_socket: owner.store_socket(),
            root,
            cache: owner.cache_dir(),
            state: workset.state_dir()?,
            paths: owner.path_overrides.clone(),
            identity: owner
                .identity_environment()
                .iter()
                .map(|(key, value)| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
                .collect(),
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
        match &self.mode {
            Mode::View { home_skeleton } => {
                let mut config = crate::layout::FsViewConfig::new(mounts)?;
                config.home_skeleton = home_skeleton.clone();
                config.cache = Some(self.cache.clone().into_std_path_buf());
                config.workset_state = Some(self.state.clone().into_std_path_buf());
                config.devshell_cache = Some(devshell_cache(&self.cache).into_std_path_buf());
                crate::layout::FsViewBuilder::new(config)?
                    .build_in_place(self.root.as_std_path())?;
            }
            Mode::Exposed => {
                crate::layout::ExposedBuilder::new(mounts)?.build_in_place(Path::new("/"))?
            }
        }
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
        if matches!(self.mode, Mode::View { .. }) {
            crate::layout::pivot_into(self.root.as_std_path())?;
        }
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
        match &self.mode {
            Mode::View { .. } => {
                // The agent's own nix profile first, then the base userland.
                // Nothing of the host's PATH.
                let home = crate::AGENT_HOME;
                // Passed through from the user: the terminal and the timezone.
                for name in ["TERM", "TZ"] {
                    if let Some(value) = environment.get(name) {
                        command.env(name, value);
                    }
                }
                // Ahead of a flake dev shell's own PATH: the
                // find fork, then cargo-installed binaries.
                let cargo_bin = format!("{home}/.cache/cargo/bin");
                command.env(
                    "RHO_DEVSHELL_PATH_PREFIX",
                    match crate::FIND_BIN {
                        Some(find) => format!("{find}:{cargo_bin}"),
                        None => cargo_bin,
                    },
                );
                // For the base's `nix develop`: the builder,
                // and the agent host's shell cache it asks first.
                command.env("RHO_DEVSHELL_BUILDER", crate::devshell_builder());
                command.env("RHO_DEVSHELL_DIR", devshell_cache(&self.cache));
                command
                    .env(
                        "PATH",
                        format!("{home}/.nix-profile/bin:{}/bin", crate::AGENT_BASE),
                    )
                    .env("HOME", home)
                    .env("USER", "agent")
                    .env("LOGNAME", "agent")
                    .env("LANG", "C.UTF-8")
                    .env("COLORTERM", "truecolor")
                    .env("INSIDE_AGENT", "1")
                    .env("XDG_CACHE_HOME", format!("{home}/.cache"))
                    .env("XDG_CONFIG_HOME", format!("{home}/.config"))
                    .env("XDG_STATE_HOME", format!("{home}/.local/state"))
                    .env("CARGO_HOME", format!("{home}/.cache/cargo"))
                    .env(
                        "CARGO_BUILD_TARGET_DIR",
                        format!("{home}/.cache/cargo-target"),
                    )
                    .env("GIT_CONFIG_SYSTEM", "/etc/gitconfig")
                    .env("FIND_DENY_ROOTS", format!("/:/nix/store:{home}"));
                command.envs(self.identity.iter().map(|(name, value)| (name, value)));
                if Path::new(crate::layout::NIX_DAEMON_SOCKET).exists() {
                    command.env("NIX_REMOTE", "daemon");
                }
            }
            Mode::Exposed => {
                // The user's environment, with Rho's git ahead of theirs.
                command.envs(environment.0.iter().map(|(name, value)| (name, value)));
                let path = environment.get("PATH").map(|path| self.paths.add_to(path));
                command.env("PATH", prepend_path(&crate::git_dir(), path.as_deref()));
                if let Some(find) = crate::FIND_BIN {
                    command.env("RHO_DEVSHELL_PATH_PREFIX", find);
                }
            }
        }
        // The cargo ahead of a dev shell's own.
        if let Some(cargo) = crate::SHARED_CARGO_BIN {
            command.env("RHO_DEVSHELL_CARGO", cargo);
        }
        if let Some(socket) = &self.store_socket {
            command.env(crate::SOCKET_ENV, socket);
        }
    }

    /// The temporary tree before pivot, or the exposed process's root.
    pub fn staging_root(&self) -> &Path {
        match self.mode {
            Mode::View { .. } => self.root.as_std_path(),
            Mode::Exposed => Path::new("/"),
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

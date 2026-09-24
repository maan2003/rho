use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io::Read as _;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// Largest file [`Namespace::read_file_bounded`] will ever return.
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
    pub unsafe fn enter(&self) -> anyhow::Result<Arc<Namespace>> {
        close_inherited_fds_on_exec()?;
        let environment = UserEnvironment::new(std::env::vars_os().collect());
        if matches!(self.mode, Mode::View { .. }) {
            crate::layout::pivot_into(self.root.as_std_path())?;
        }
        std::env::set_current_dir(MOUNT_ROOT)?;
        let view = Arc::new(Namespace {
            workset: self.workset.clone(),
            mode: self.mode.clone(),
            cwd: MOUNT_ROOT.into(),
            src: MOUNT_ROOT.into(),
            environment,
            path_overrides: self.paths.clone(),
            store_environment: self
                .store_socket
                .as_ref()
                .map(|socket| vec![(crate::SOCKET_ENV.into(), socket.as_os_str().to_owned())])
                .unwrap_or_default(),
            identity: self
                .identity
                .iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
            workset_state: self.state.clone(),
        });
        let mut command = tokio::process::Command::new("");
        view.configure_environment(&mut command);
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
        Ok(view)
    }

    /// The temporary tree before pivot, or the exposed process's root.
    pub fn staging_root(&self) -> &Path {
        match self.mode {
            Mode::View { .. } => self.root.as_std_path(),
            Mode::Exposed => Path::new("/"),
        }
    }
}

/// Paths and command configuration in a workset view. This value owns no
/// namespace: execution inherits the process's workset mounts.
#[derive(Clone, Debug)]
pub struct Namespace {
    workset: String,
    mode: Mode,
    cwd: Utf8PathBuf,
    src: Utf8PathBuf,
    environment: UserEnvironment,
    path_overrides: PathOverrides,
    store_environment: Vec<(OsString, OsString)>,
    identity: Vec<(OsString, OsString)>,
    workset_state: Utf8PathBuf,
}

impl Namespace {
    pub(crate) fn new(workset: Workset, mode: Mode, cwd: &Utf8Path) -> anyhow::Result<Arc<Self>> {
        let owner = workset.owner()?;
        let visible_root = Utf8Path::new(MOUNT_ROOT);
        let cwd = visible_root.join(crate::visible_relative(MOUNT_ROOT, cwd)?);
        let src = workset.root().to_owned();
        anyhow::ensure!(
            src.join(cwd.strip_prefix(visible_root).unwrap_or(&cwd))
                .is_dir(),
            "working directory does not exist in workset {}: {cwd}",
            workset.id()
        );
        let store_environment = owner.store_environment();
        let identity = owner.identity_environment().to_vec();
        let workset_state = workset.state_dir()?;
        let environment = owner.environment.clone();
        let path_overrides = owner.path_overrides.clone();
        Ok(Arc::new(Self {
            workset: workset.id().to_owned(),
            mode,
            cwd,
            src,
            environment,
            path_overrides,
            store_environment,
            identity,
            workset_state,
        }))
    }

    pub fn workset_id(&self) -> &str {
        &self.workset
    }
    pub fn state_dir(&self) -> &Utf8Path {
        &self.workset_state
    }

    pub fn for_cwd(&self, cwd: &Utf8Path) -> anyhow::Result<Arc<Self>> {
        let mut view = self.clone();
        view.cwd = namespace_cwd(self.visible_root(), self.visible_root(), Some(cwd))?.into();
        anyhow::ensure!(
            view.host_cwd().is_dir(),
            "working directory does not exist: {}",
            view.cwd
        );
        Ok(Arc::new(view))
    }

    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    /// The persisted form of this namespace's mode.
    pub fn workset_mode(&self) -> WorksetMode {
        self.mode.workset_mode()
    }

    /// Where the workset directory appears inside this namespace: `/src`.
    pub fn visible_root(&self) -> &'static Utf8Path {
        Utf8Path::new(MOUNT_ROOT)
    }

    /// The agent's working directory as it sees it.
    pub fn cwd(&self) -> &Utf8Path {
        &self.cwd
    }

    /// The agent's working directory on the host.
    pub fn host_cwd(&self) -> Utf8PathBuf {
        self.src
            .join(self.cwd.strip_prefix(MOUNT_ROOT).unwrap_or(&self.cwd))
    }

    /// The repository the agent works in: the git checkout containing its
    /// working directory (or the directory itself), as `(visible, host)`.
    pub fn context_roots(&self) -> anyhow::Result<(Utf8PathBuf, Utf8PathBuf)> {
        let host_cwd = self.host_cwd();
        let (root, _) = crate::resolve_workdir_root(host_cwd.as_std_path())?;
        let root = if root.starts_with(&self.src) {
            root
        } else {
            host_cwd
        };
        let visible = self
            .visible_root()
            .join(root.strip_prefix(&self.src).unwrap_or(&root));
        Ok((visible, root))
    }

    /// Reads a file below the workset directory, refusing symlinks that
    /// escape it and files larger than `max_len` (capped at
    /// [`MAX_BOUNDED_READ`]). `path` is a visible path or relative to the
    /// working directory.
    pub async fn read_file_bounded(&self, path: &Path, max_len: usize) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            max_len <= MAX_BOUNDED_READ,
            "read limit {max_len} exceeds {MAX_BOUNDED_READ} bytes"
        );
        let relative = self.src_relative(path)?;
        let root = self.src.clone();
        let display = path.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut file = open_beneath(root.as_std_path(), &relative)
                .with_context(|| format!("open {}", display.display()))?;
            let metadata = file.metadata()?;
            anyhow::ensure!(
                metadata.is_file(),
                "not a regular file: {}",
                display.display()
            );
            anyhow::ensure!(
                metadata.len() <= max_len as u64,
                "{} is {} bytes, over the {max_len} byte limit",
                display.display(),
                metadata.len()
            );
            let mut contents = Vec::with_capacity(metadata.len() as usize);
            file.by_ref()
                .take(max_len as u64 + 1)
                .read_to_end(&mut contents)?;
            anyhow::ensure!(
                contents.len() <= max_len,
                "{} grew past the {max_len} byte limit while reading",
                display.display()
            );
            Ok(contents)
        })
        .await
        .context("bounded read task panicked")?
    }

    /// The path below the workset directory that a visible path, or a path
    /// relative to the working directory, denotes.
    fn src_relative(&self, path: &Path) -> anyhow::Result<PathBuf> {
        let path = Utf8Path::from_path(path).context("path is not valid UTF-8")?;
        let visible = if path.is_absolute() {
            path.to_owned()
        } else {
            self.cwd.join(path)
        };
        Ok(crate::visible_relative(MOUNT_ROOT, &visible)?
            .as_std_path()
            .to_owned())
    }

    /// Configures `command` to run in the namespace. `cwd` is a visible
    /// path or relative to the agent's working directory, which is the
    /// default. View mode replaces the environment with an allowlist;
    /// exposed mode passes the user's through. Inherited descriptors above
    /// stdio are closed on exec either way.
    pub async fn prepare_command(
        &self,
        command: &mut tokio::process::Command,
        cwd: Option<&Utf8Path>,
    ) -> anyhow::Result<()> {
        self.configure_environment(command);
        // Syscall-only; preopened provider mount sources remain usable until exec.
        crate::command_stdio_only(command);
        command.current_dir(namespace_cwd(self.visible_root(), &self.cwd, cwd)?);
        Ok(())
    }

    fn configure_environment(&self, command: &mut tokio::process::Command) {
        // What the caller set explicitly wins over the mode's environment.
        let overrides = command
            .as_std()
            .get_envs()
            .map(|(name, value)| (name.to_owned(), value.map(OsStr::to_owned)))
            .collect::<Vec<_>>();
        command.env_clear();
        match &self.mode {
            Mode::View { .. } => {
                // The agent's own nix profile first, then the base userland
                // (VIEW.md). Nothing of the host's PATH.
                let home = crate::AGENT_HOME;
                // Passed through from the user: the terminal, the timezone,
                // and the agent host's own find-fork directory for direnvrc.
                for name in ["TERM", "TZ", "RHO_DIRENV_PATH_BEFORE"] {
                    if let Some(value) = self.environment.get(name) {
                        command.env(name, value);
                    }
                }
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
                    .env("DIRENV_CONFIG", "/etc/rho/direnv")
                    .env("RHO_DIRENV_LAYOUT_DIR", self.workset_state.join("direnv"))
                    .env("FIND_DENY_ROOTS", format!("/:/nix/store:{home}"));
                command.envs(self.identity.iter().map(|(name, value)| (name, value)));
                if Path::new(crate::layout::NIX_DAEMON_SOCKET).exists() {
                    command.env("NIX_REMOTE", "daemon");
                }
            }
            Mode::Exposed => {
                // The user's environment, with Rho's git ahead of theirs.
                command.envs(self.environment.0.iter().map(|(name, value)| (name, value)));
                let path = self
                    .environment
                    .get("PATH")
                    .map(|path| self.path_overrides.add_to(path));
                command.env("PATH", prepend_path(&crate::git_dir(), path.as_deref()));
            }
        }
        command.envs(
            self.store_environment
                .iter()
                .map(|(name, value)| (name, value)),
        );
        for (name, value) in overrides {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
    }

    /// Give the dedicated interpreter thread a private cwd, retaining the
    /// workset process's mount namespace.
    ///
    /// # Safety
    /// This thread must not return to a reusable thread pool.
    pub async unsafe fn enter_interpreter_thread(&self) -> anyhow::Result<()> {
        crate::layout::unshare_fs_attributes()?;
        std::env::set_current_dir(&self.cwd)
            .with_context(|| format!("enter working directory {}", self.cwd))
    }

    /// The host path behind a visible path, or a path relative to the
    /// working directory, without touching the filesystem: a lexical
    /// mapping that refuses `.` and `..`.
    pub fn resolve_host_path_checked(&self, path: &Path) -> anyhow::Result<PathBuf> {
        Ok(self.src.as_std_path().join(self.src_relative(path)?))
    }
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

/// The working directory a command starts in: `requested` as a visible
/// path or relative to `default`, checked lexically against `visible_root`.
fn namespace_cwd(
    visible_root: &Utf8Path,
    default: &Utf8Path,
    requested: Option<&Utf8Path>,
) -> anyhow::Result<String> {
    let cwd = match requested {
        Some(path) if path.is_absolute() => path.to_owned(),
        Some(path) => default.join(path),
        None => default.to_owned(),
    };
    anyhow::ensure!(
        !cwd.as_str()
            .split('/')
            .any(|component| component == "." || component == ".."),
        "namespace cwd must not contain . or .. components: {cwd}"
    );
    anyhow::ensure!(
        cwd.starts_with(visible_root),
        "namespace cwd must be below {visible_root}: {cwd}"
    );
    Ok(cwd.into_string())
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

#[cfg(test)]
mod tests {
    use camino::Utf8Path;

    use super::namespace_cwd;

    #[test]
    fn cwd_stays_lexically_below_the_visible_root() {
        let ws = Utf8Path::new("/ws");
        let project = Utf8Path::new("/ws/project");
        assert!(namespace_cwd(ws, project, Some(Utf8Path::new("/ws/../tmp"))).is_err());
        assert!(namespace_cwd(ws, project, Some(Utf8Path::new("/wsfoo"))).is_err());
        assert!(namespace_cwd(ws, project, Some(Utf8Path::new("../x"))).is_err());
        assert!(namespace_cwd(ws, project, Some(Utf8Path::new("src/./x"))).is_err());
        assert_eq!(
            namespace_cwd(ws, project, Some(Utf8Path::new("src/nested"))).unwrap(),
            "/ws/project/src/nested"
        );
        assert_eq!(
            namespace_cwd(Utf8Path::new("/src"), Utf8Path::new("/src/zeta"), None).unwrap(),
            "/src/zeta"
        );
        assert_eq!(
            namespace_cwd(ws, project, Some(Utf8Path::new("/ws/other"))).unwrap(),
            "/ws/other"
        );
    }
}

pub(crate) fn close_inherited_fds_on_exec() -> std::io::Result<()> {
    if unsafe { libc::close_range(3, u32::MAX, libc::CLOSE_RANGE_CLOEXEC as libc::c_int) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

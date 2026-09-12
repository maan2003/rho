use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io::Read as _;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};

use crate::layout::MOUNT_ROOT;
use crate::{PathOverrides, UserEnvironment, Workset, WorksetMode};

/// How an agent sees its workset.
#[derive(Clone, Debug)]
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

/// Claude Code's home for one agent, mounted over its `~/.claude` inside the
/// namespace. All paths are host paths; `config_home` is where the agent
/// expects the directory, and view mode retargets a host-home-relative
/// `config_home` under `/home/agent`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeHome {
    /// Per-account state directory mounted over `config_home`.
    pub account: PathBuf,
    /// The agent's `~/.claude` as a host path.
    pub config_home: PathBuf,
    /// Directory mounted over `<config_home>/projects`.
    pub shared_projects: PathBuf,
    /// File mounted over `<config_home>/CLAUDE.md`.
    pub prompt: PathBuf,
    /// Optional file mounted over `<config_home>/settings.json`.
    pub settings: Option<PathBuf>,
}

/// One agent's view of its workset: a mode, a working directory, and the
/// mount namespace realizing them. The namespace is built on the first
/// command (a load must not fail on a namespace it never uses), then kept
/// for the life of this value; commands enter it with `setns`.
#[derive(Debug)]
pub struct Namespace {
    workset: Workset,
    mode: Mode,
    /// The agent's working directory as it sees it.
    cwd: Utf8PathBuf,
    /// The workset directory on the host.
    src: Utf8PathBuf,
    environment: UserEnvironment,
    path_overrides: PathOverrides,
    store_environment: Vec<(OsString, OsString)>,
    /// The directory holding the `git` wrapper, first on the agent's PATH.
    store_bin: Option<Utf8PathBuf>,
    view_path: Option<OsString>,
    state: tokio::sync::Mutex<NsState>,
}

#[derive(Debug, Default)]
struct NsState {
    live: Option<LiveNs>,
    /// The Claude home the agent wants, mounted when the namespace exists.
    claude_home: Option<ClaudeHome>,
}

/// Holding the fds keeps the namespace alive.
#[derive(Debug)]
struct LiveNs {
    _user_ns: OwnedFd,
    mount_ns: OwnedFd,
    root: OwnedFd,
    // Declared after namespace fds so their mount references close first.
    _view_root: Option<tempfile::TempDir>,
    /// The Claude home currently mounted, keyed by the visible path it sits
    /// on so a replacement can detach it first.
    mounted_claude: Option<(ClaudeHome, PathBuf)>,
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
        let store_bin = owner.store_bin();
        let environment = owner.environment.clone();
        let path_overrides = owner.path_overrides.clone();
        let view_path = matches!(&mode, Mode::View { .. })
            .then(|| filtered_view_path(&environment, &path_overrides))
            .transpose()?
            .flatten();
        Ok(Arc::new(Self {
            workset,
            mode,
            cwd,
            src,
            environment,
            path_overrides,
            store_environment,
            store_bin,
            view_path,
            state: tokio::sync::Mutex::new(NsState::default()),
        }))
    }

    pub fn workset(&self) -> &Workset {
        &self.workset
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

    /// The mount namespace, built on first use.
    async fn live<'a>(&self, state: &'a mut NsState) -> anyhow::Result<&'a mut LiveNs> {
        if state.live.is_none() {
            let owner = self.workset.owner()?;
            let mounts = crate::layout::Mounts {
                src: self.src.as_std_path().to_owned(),
                store_root: owner.store_root().into_std_path_buf(),
                store_socket: owner.store_socket().map(Utf8PathBuf::into_std_path_buf),
                store_bin: owner.store_bin().map(Utf8PathBuf::into_std_path_buf),
            };
            // Own the temporary directory in the caller's host-root frame. If
            // it were created and dropped after pivot_root, cleanup would
            // resolve its host path from inside the view and leak an empty
            // directory.
            let view_root = matches!(&self.mode, Mode::View { .. })
                .then(|| {
                    tempfile::Builder::new()
                        .prefix("rho-workset-view-")
                        .tempdir()
                        .context("create namespace root")
                })
                .transpose()?;
            let view_root_path = view_root.as_ref().map(|root| root.path().to_owned());
            let mode = self.mode.clone();
            let (user_ns, mount_ns, root) = namespace_thread("rho-workset-namespace", move || {
                crate::layout::unshare_mount_namespace()?;
                match mode {
                    Mode::View { home_skeleton } => {
                        let root = view_root_path.context("view mode has no namespace root")?;
                        let mut config = crate::layout::FsViewConfig::new(mounts)?;
                        config.home_skeleton = home_skeleton;
                        let builder = crate::layout::FsViewBuilder::new(config)?;
                        builder.build_in_place(&root)?;
                        builder.pivot_into(&root)?;
                    }
                    Mode::Exposed => {
                        crate::layout::ExposedBuilder::new(mounts)?
                            .build_in_place(Path::new("/"))?;
                    }
                }
                Ok::<_, anyhow::Error>((
                    File::open("/proc/thread-self/ns/user")?.into(),
                    File::open("/proc/thread-self/ns/mnt")?.into(),
                    open_root()?,
                ))
            })
            .await?;
            state.live = Some(LiveNs {
                _user_ns: user_ns,
                mount_ns,
                root,
                _view_root: view_root,
                mounted_claude: None,
            });
        }
        let live = state.live.as_mut().expect("namespace was just built");
        if let Some(home) = state.claude_home.clone()
            && live
                .mounted_claude
                .as_ref()
                .is_none_or(|(mounted, _)| *mounted != home)
        {
            let target = self.visible_path_for(&home.config_home);
            let previous = live
                .mounted_claude
                .as_ref()
                .map(|(_, target)| target.clone());
            let mount_ns = live.mount_ns.try_clone()?;
            let root = live.root.try_clone()?;
            let install_target = target.clone();
            let mounting = home.clone();
            namespace_thread("rho-workset-claude-home", move || {
                use crate::layout::{capture_mount, detach_mount, install_captured_mount};
                crate::layout::unshare_mount_namespace()?;
                let account = capture_mount(&mounting.account)?;
                let projects = capture_mount(&mounting.shared_projects)?;
                let prompt = capture_mount(&mounting.prompt)?;
                let settings = mounting
                    .settings
                    .as_deref()
                    .map(capture_mount)
                    .transpose()?;
                enter(&mount_ns, &root)?;
                if let Some(previous) = previous {
                    detach_mount(&previous)?;
                }
                std::fs::create_dir_all(&install_target)
                    .with_context(|| format!("create {}", install_target.display()))?;
                install_captured_mount(&account, &install_target)?;
                let inner = (|| {
                    install_captured_mount(&projects, &install_target.join("projects"))?;
                    install_captured_mount(&prompt, &install_target.join("CLAUDE.md"))?;
                    if let Some(settings) = &settings {
                        install_captured_mount(settings, &install_target.join("settings.json"))?;
                    }
                    anyhow::Ok(())
                })();
                if inner.is_err() {
                    // Leave no half-assembled home behind; the caller sees
                    // the original error.
                    let _ = detach_mount(&install_target);
                }
                inner
            })
            .await?;
            live.mounted_claude = Some((home, target));
        }
        Ok(live)
    }

    /// Mounts `home` over the agent's `~/.claude` inside the namespace:
    /// now when it exists, otherwise when it is built. Setting the same
    /// home again is a no-op; a different one replaces the previous mount
    /// stack. Every mount is a bind of a host path, so the agent's writes
    /// land in the account and shared-projects directories.
    pub async fn set_claude_home(&self, home: ClaudeHome) -> anyhow::Result<()> {
        let mut state = self.state.lock().await;
        let previous = state.claude_home.replace(home);
        if state.live.is_some()
            && let Err(error) = self.live(&mut state).await
        {
            state.claude_home = previous;
            return Err(error);
        }
        Ok(())
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

    /// Where a host path appears inside the namespace: view mode relocates
    /// the host home to `/home/agent`, exposed mode keeps host paths.
    fn visible_path_for(&self, host_path: &Path) -> PathBuf {
        if matches!(&self.mode, Mode::View { .. })
            && let Some(relative) = dirs::home_dir()
                .as_deref()
                .and_then(|home| host_path.strip_prefix(home).ok())
        {
            return Path::new("/home/agent").join(relative);
        }
        host_path.to_owned()
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
        // What the caller set explicitly wins over the mode's environment.
        let overrides = command
            .as_std()
            .get_envs()
            .map(|(name, value)| (name.to_owned(), value.map(OsStr::to_owned)))
            .collect::<Vec<_>>();
        command.env_clear();
        match &self.mode {
            Mode::View { .. } => {
                if let Some(value) = self.environment.get("TERM") {
                    command.env("TERM", value);
                }
                if let Some(path) = &self.view_path {
                    command.env("PATH", path);
                }
                command
                    .env("HOME", "/home/agent")
                    .env("USER", "agent")
                    .env("LOGNAME", "agent");
            }
            Mode::Exposed => {
                command.envs(self.environment.0.iter().map(|(name, value)| (name, value)));
                if let Some(path) = self.environment.get("PATH") {
                    command.env("PATH", self.path_overrides.add_to(path));
                }
            }
        }
        command.envs(
            self.store_environment
                .iter()
                .map(|(name, value)| (name, value)),
        );
        if let Some(bin) = &self.store_bin {
            let path = command
                .as_std()
                .get_envs()
                .find_map(|(name, value)| (name == "PATH").then_some(value.map(OsStr::to_owned)))
                .flatten();
            command.env("PATH", prepend_path(bin.as_std_path(), path.as_deref()));
        }
        for (name, value) in overrides {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
        let cwd = namespace_cwd(self.visible_root(), &self.cwd, cwd)?;
        let mut state = self.state.lock().await;
        let live = self.live(&mut state).await?;
        let cwd = CString::new(cwd).context("namespace cwd contains NUL")?;
        let mount_ns = live.mount_ns.as_raw_fd();
        let root = live.root.as_raw_fd();
        unsafe {
            command.pre_exec(move || {
                // Whatever the daemon holds open without CLOEXEC must not
                // reach the agent; stdio is the only deliberate channel.
                if libc::syscall(
                    libc::SYS_close_range,
                    3_u32,
                    u32::MAX,
                    libc::CLOSE_RANGE_CLOEXEC,
                ) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                setns(mount_ns)?;
                if libc::fchdir(root) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::chroot(c".".as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Establish the requested cwd after setns and chroot; never
                // inherit the launcher thread's cwd.
                if libc::chdir(cwd.as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }

    /// Enters this namespace on a dedicated, long-lived interpreter thread
    /// and moves it to the agent's working directory. This installs the
    /// view, not a sandbox.
    ///
    /// # Safety
    /// The caller must never return this thread to a reusable thread pool:
    /// its cwd, root and mount namespace change here. This future must be
    /// polled on that same thread throughout.
    pub async unsafe fn enter_interpreter_thread(&self) -> anyhow::Result<()> {
        let mut state = self.state.lock().await;
        let live = self.live(&mut state).await?;
        let mount_ns = live.mount_ns.try_clone()?;
        let root = live.root.try_clone()?;
        // Building the namespace may have started blocking threads from this
        // one, sharing its fs_struct again; setns needs it private now.
        crate::layout::unshare_fs_attributes()?;
        enter(&mount_ns, &root)?;
        std::env::set_current_dir(&self.cwd)
            .with_context(|| format!("enter working directory {}", self.cwd))?;
        Ok(())
    }

    /// The host path behind a visible path, or a path relative to the
    /// working directory, without touching the filesystem: a lexical
    /// mapping that refuses `.` and `..`.
    pub fn resolve_host_path_checked(&self, path: &Path) -> anyhow::Result<PathBuf> {
        Ok(self.src.as_std_path().join(self.src_relative(path)?))
    }
}

/// Namespace-mutating work must run on a dedicated thread that exits. No
/// runtime pool thread may ever call `unshare` or `setns`: both the mount
/// namespace and the detached fs_struct would otherwise survive into an
/// unrelated task when that pool thread is reused.
async fn namespace_thread<T, F>(name: &str, work: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _ = sender.send(work());
        })
        .with_context(|| format!("start {name} thread"))?;
    receiver
        .await
        .with_context(|| format!("{name} thread panicked"))?
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

fn filtered_view_path(
    environment: &UserEnvironment,
    overrides: &PathOverrides,
) -> anyhow::Result<Option<OsString>> {
    let Some(path) = environment.get("PATH") else {
        return Ok(None);
    };
    let mut entries = Vec::new();
    for entry in std::env::split_paths(&overrides.add_to(path)) {
        let resolved = if entry.starts_with("/nix/store") {
            entry
        } else {
            match entry.canonicalize() {
                Ok(resolved) if resolved.starts_with("/nix/store") => resolved,
                _ => continue,
            }
        };
        if resolved.is_dir() && !entries.contains(&resolved) {
            entries.push(resolved);
        }
    }
    Ok(Some(
        std::env::join_paths(entries).context("join filtered view PATH")?,
    ))
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

fn open_root() -> anyhow::Result<OwnedFd> {
    let fd = unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("open namespace root");
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn enter(mount_ns: &OwnedFd, root: &OwnedFd) -> anyhow::Result<()> {
    setns(mount_ns.as_raw_fd()).context("enter workset mount namespace")?;
    let result = unsafe { libc::fchdir(root.as_raw_fd()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("enter namespace root");
    }
    let result = unsafe { libc::chroot(c".".as_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("chroot namespace root");
    }
    Ok(())
}

fn setns(fd: i32) -> std::io::Result<()> {
    if unsafe { libc::setns(fd, 0) } != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
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

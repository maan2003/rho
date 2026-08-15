use std::ffi::{CString, OsString};
use std::fs::File;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use camino::Utf8Path;

use crate::{Checkout, PathOverrides, UserEnvironment, Workset};

#[derive(Clone, Debug)]
pub enum Mode {
    View { home_skeleton: Option<PathBuf> },
    Exposed,
}

/// A live user+mount namespace presenting one workset at `/src`.
#[derive(Debug)]
pub struct Namespace {
    workset: Workset,
    mode: Mode,
    _user_ns: OwnedFd,
    mount_ns: OwnedFd,
    root: OwnedFd,
    // Declared after namespace fds so their mount references close first.
    _view_root: Option<tempfile::TempDir>,
    primary: String,
    checkouts: Vec<Arc<Checkout>>,
    host_paths: Vec<(PathBuf, PathBuf)>,
    environment: UserEnvironment,
    path_overrides: PathOverrides,
    view_path: Option<OsString>,
}

impl Namespace {
    pub(crate) async fn create(workset: Workset, mode: Mode) -> anyhow::Result<Arc<Self>> {
        let owner = workset.owner()?;
        let checkouts = workset.checkouts().await;
        let mounts = workset.mounts().await;
        let primary = workset.primary_name().await?;
        anyhow::ensure!(
            checkouts.iter().any(|checkout| checkout.name() == primary),
            "workset primary checkout is not materialized: {primary}"
        );
        let visible_root = match &mode {
            Mode::View { .. } => rho_fs_view::VIEW_MOUNT_ROOT,
            Mode::Exposed => rho_fs_view::EXPOSED_MOUNT_ROOT,
        };
        let host_paths = mounts
            .workspaces
            .iter()
            .map(|checkout| {
                (
                    Path::new(visible_root).join(&checkout.name),
                    checkout.source.clone(),
                )
            })
            .collect();
        let environment = owner.environment.clone();
        let path_overrides = owner.path_overrides.clone();
        let view_path = matches!(&mode, Mode::View { .. })
            .then(|| filtered_view_path(&environment, &path_overrides))
            .transpose()?
            .flatten();
        // Own the temporary directory in the caller's host-root frame. If it
        // were created and dropped after pivot_root, cleanup would resolve its
        // host path from inside the view and leak an empty directory.
        let view_root = matches!(&mode, Mode::View { .. })
            .then(|| {
                tempfile::Builder::new()
                    .prefix("rho-workset-view-")
                    .tempdir()
                    .context("create namespace root")
            })
            .transpose()?;
        let view_root_path = view_root.as_ref().map(|root| root.path().to_owned());
        let mode_for_thread = mode.clone();
        let (user_ns, mount_ns, root) = namespace_thread("rho-workset-namespace", move || {
            rho_fs_view::unshare_mount_namespace()?;
            match mode_for_thread {
                Mode::View { home_skeleton } => {
                    let root = view_root_path.context("view mode has no namespace root")?;
                    let mut config = rho_fs_view::FsViewConfig::new(mounts)?;
                    config.home_skeleton = home_skeleton;
                    let builder = rho_fs_view::FsViewBuilder::new(config)?;
                    builder.build_in_place(&root)?;
                    builder.pivot_into(&root)?;
                }
                Mode::Exposed => {
                    rho_fs_view::ExposedBuilder::new(mounts)?.build_in_place(Path::new("/"))?;
                }
            }
            Ok::<_, anyhow::Error>((
                File::open("/proc/thread-self/ns/user")?.into(),
                File::open("/proc/thread-self/ns/mnt")?.into(),
                open_root()?,
            ))
        })
        .await?;
        Ok(Arc::new(Self {
            workset,
            mode,
            _user_ns: user_ns,
            mount_ns,
            root,
            _view_root: view_root,
            primary,
            checkouts,
            host_paths,
            environment,
            path_overrides,
            view_path,
        }))
    }

    pub fn workset(&self) -> &Workset {
        &self.workset
    }

    pub fn primary(&self) -> Arc<Checkout> {
        Arc::clone(&self.checkouts[0])
    }

    pub fn entries(&self) -> &[Arc<Checkout>] {
        &self.checkouts
    }

    pub async fn snapshot(&self) -> anyhow::Result<()> {
        for checkout in &self.checkouts {
            checkout.snapshot().await?;
        }
        Ok(())
    }

    pub async fn refresh(&self, mounts: rho_fs_view::Mounts) -> anyhow::Result<()> {
        let mount_ns = self.mount_ns.try_clone()?;
        let root = self.root.try_clone()?;
        let visible_root = match &self.mode {
            Mode::View { .. } => rho_fs_view::VIEW_MOUNT_ROOT,
            Mode::Exposed => rho_fs_view::EXPOSED_MOUNT_ROOT,
        };
        namespace_thread("rho-workset-refresh", move || {
            rho_fs_view::unshare_mount_namespace()?;
            let mounts = rho_fs_view::prepare_mounts(&mounts)?;
            enter(&mount_ns, &root)?;
            rho_fs_view::mount_prepared_in_place(&mounts, Path::new("/"), visible_root)
        })
        .await
    }

    pub fn prepare_command(
        &self,
        command: &mut tokio::process::Command,
        cwd: Option<&Utf8Path>,
    ) -> anyhow::Result<()> {
        self.prepare_command_with_mounts(command, cwd, Vec::new())
    }

    pub fn prepare_command_with_mounts(
        &self,
        command: &mut tokio::process::Command,
        cwd: Option<&Utf8Path>,
        file_mounts: Vec<(camino::Utf8PathBuf, camino::Utf8PathBuf)>,
    ) -> anyhow::Result<()> {
        match &self.mode {
            Mode::View { .. } => {
                command.env_clear();
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
                self.environment.apply(command);
                if let Some(path) = self.environment.get("PATH") {
                    command.env("PATH", self.path_overrides.add_to(path));
                }
            }
        }
        let visible_root = match &self.mode {
            Mode::View { .. } => rho_fs_view::VIEW_MOUNT_ROOT,
            Mode::Exposed => rho_fs_view::EXPOSED_MOUNT_ROOT,
        };
        let cwd = namespace_cwd(visible_root, &self.primary, cwd)?;
        let cwd = CString::new(cwd).context("namespace cwd contains NUL")?;
        let mount_ns = self.mount_ns.as_raw_fd();
        let root = self.root.as_raw_fd();
        let host_home = dirs::home_dir();
        let file_mounts = file_mounts
            .into_iter()
            .map(|(source, target)| {
                let target = if matches!(&self.mode, Mode::View { .. }) {
                    host_home
                        .as_deref()
                        .and_then(|home| target.as_std_path().strip_prefix(home).ok())
                        .map_or_else(
                            || target.as_std_path().to_owned(),
                            |relative| Path::new("/home/agent").join(relative),
                        )
                } else {
                    target.into_std_path_buf()
                };
                let relative = target.strip_prefix("/").unwrap_or(&target);
                let target = Path::new(&format!("/proc/self/fd/{root}")).join(relative);
                let parent = target.parent().context("file mount target has no parent")?;
                Ok::<_, anyhow::Error>((
                    CString::new(source.as_os_str().as_bytes())?,
                    CString::new(parent.as_os_str().as_bytes())?,
                    CString::new(target.as_os_str().as_bytes())?,
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        unsafe {
            command.pre_exec(move || {
                setns(mount_ns)?;
                for (source, parent, target) in &file_mounts {
                    if libc::mkdir(parent.as_ptr(), 0o700) != 0 {
                        let error = std::io::Error::last_os_error();
                        if error.kind() != std::io::ErrorKind::AlreadyExists {
                            return Err(error);
                        }
                    }
                    let target_fd = libc::open(
                        target.as_ptr(),
                        libc::O_WRONLY | libc::O_CREAT | libc::O_CLOEXEC,
                        0o600,
                    );
                    if target_fd < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::close(target_fd);
                    if libc::mount(
                        source.as_ptr(),
                        target.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND,
                        std::ptr::null(),
                    ) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if libc::fchdir(root) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::chroot(c".".as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Establish the requested checkout cwd after setns and
                // chroot; never inherit the launcher thread's cwd.
                if libc::chdir(cwd.as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }

    pub fn resolve_host_path_checked(&self, path: &Path) -> anyhow::Result<PathBuf> {
        if path.is_absolute() {
            for (visible, host) in &self.host_paths {
                if let Ok(relative) = path.strip_prefix(visible) {
                    return Ok(host.join(relative));
                }
            }
            anyhow::bail!("path is outside every checkout: {}", path.display())
        }
        let host = self
            .host_paths
            .iter()
            .find(|(visible, _)| visible.file_name() == Some(self.primary.as_ref()))
            .map(|(_, host)| host)
            .context("primary checkout is unavailable")?;
        Ok(host.join(path))
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

fn namespace_cwd(
    visible_root: &str,
    primary: &str,
    requested: Option<&Utf8Path>,
) -> anyhow::Result<String> {
    let visible_root = Utf8Path::new(visible_root);
    let cwd = match requested {
        Some(path) if path.is_absolute() => path.to_owned(),
        Some(path) => visible_root.join(primary).join(path),
        None => visible_root.join(primary),
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
        assert!(namespace_cwd("/ws", "project", Some(Utf8Path::new("/ws/../tmp"))).is_err());
        assert!(namespace_cwd("/ws", "project", Some(Utf8Path::new("/wsfoo"))).is_err());
        assert!(namespace_cwd("/ws", "project", Some(Utf8Path::new("../x"))).is_err());
        assert!(namespace_cwd("/ws", "project", Some(Utf8Path::new("src/./x"))).is_err());
        assert_eq!(
            namespace_cwd("/ws", "project", Some(Utf8Path::new("src/nested"))).unwrap(),
            "/ws/project/src/nested"
        );
        assert_eq!(namespace_cwd("/src", "zeta", None).unwrap(), "/src/zeta");
        assert_eq!(
            namespace_cwd(
                "/ws",
                "project",
                Some(Utf8Path::new("/ws/project/src/nested"))
            )
            .unwrap(),
            "/ws/project/src/nested"
        );
    }
}

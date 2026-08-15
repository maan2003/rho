use std::ffi::CString;
use std::fs::File;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
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
    workset: Arc<Workset>,
    mode: Mode,
    user_ns: OwnedFd,
    mount_ns: OwnedFd,
    root: OwnedFd,
    primary: String,
    environment: UserEnvironment,
    path_overrides: PathOverrides,
}

impl Namespace {
    pub(crate) async fn create(workset: Arc<Workset>, mode: Mode) -> anyhow::Result<Arc<Self>> {
        let owner = workset
            .owner
            .upgrade()
            .context("worksets manager was dropped")?;
        let mounts = workset.mounts().await;
        let primary = mounts
            .workspaces
            .first()
            .map(|workspace| workspace.name.clone())
            .context("workset has no workspace")?;
        let environment = owner.environment.clone();
        let path_overrides = owner.path_overrides.clone();
        let mode_for_thread = mode.clone();
        let (user_ns, mount_ns, root) = tokio::task::spawn_blocking(move || {
            rho_fs_view::unshare_identity_namespaces()?;
            match mode_for_thread {
                Mode::View { home_skeleton } => {
                    let root = tempfile::Builder::new()
                        .prefix("rho-workset-view-")
                        .tempdir()
                        .context("create namespace root")?;
                    let mut config = rho_fs_view::FsViewConfig::new(mounts)?;
                    config.home_skeleton = home_skeleton;
                    let builder = rho_fs_view::FsViewBuilder::new(config)?;
                    builder.build_in_place(root.path())?;
                    builder.pivot_into(root.path())?;
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
        .await
        .context("namespace builder thread panicked")??;
        Ok(Arc::new(Self {
            workset,
            mode,
            user_ns,
            mount_ns,
            root,
            primary,
            environment,
            path_overrides,
        }))
    }

    pub fn workset(&self) -> &Arc<Workset> {
        &self.workset
    }

    pub async fn primary(&self) -> anyhow::Result<Arc<Checkout>> {
        self.workset
            .workspaces
            .lock()
            .await
            .values()
            .next()
            .cloned()
            .context("workset has no workspace")
    }

    pub async fn refresh(&self, mounts: rho_fs_view::Mounts) -> anyhow::Result<()> {
        let user_ns = self.user_ns.try_clone()?;
        let mount_ns = self.mount_ns.try_clone()?;
        let root = self.root.try_clone()?;
        tokio::task::spawn_blocking(move || {
            enter(&user_ns, &mount_ns, &root)?;
            rho_fs_view::mount_in_place(&mounts, Path::new("/"))
        })
        .await
        .context("namespace refresh thread panicked")?
    }

    pub fn prepare_command(
        &self,
        command: &mut tokio::process::Command,
        cwd: Option<&Utf8Path>,
    ) -> anyhow::Result<()> {
        match &self.mode {
            Mode::View { .. } => {
                command.env_clear();
                for name in ["PATH", "TERM"] {
                    if let Some(value) = self.environment.get(name) {
                        command.env(name, value);
                    }
                }
                command
                    .env("HOME", "/home/agent")
                    .env("USER", "agent")
                    .env("LOGNAME", "agent");
            }
            Mode::Exposed => self.environment.apply(command),
        }
        if let Some(path) = self.environment.get("PATH") {
            command.env("PATH", self.path_overrides.add_to(path));
        }
        let cwd = cwd.map_or_else(
            || format!("{}/{}", rho_fs_view::MOUNT_ROOT, self.primary),
            |path| {
                if path.is_absolute() {
                    path.as_str().to_owned()
                } else {
                    format!("{}/{}/{}", rho_fs_view::MOUNT_ROOT, self.primary, path)
                }
            },
        );
        anyhow::ensure!(
            cwd.starts_with(rho_fs_view::MOUNT_ROOT),
            "namespace cwd must be below /src: {cwd}"
        );
        let cwd = CString::new(cwd).context("namespace cwd contains NUL")?;
        let user_ns = self.user_ns.as_raw_fd();
        let mount_ns = self.mount_ns.as_raw_fd();
        let root = self.root.as_raw_fd();
        unsafe {
            command.pre_exec(move || {
                setns(user_ns)?;
                setns(mount_ns)?;
                if libc::fchdir(root) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::chroot(c".".as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::chdir(cwd.as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }
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

fn enter(user_ns: &OwnedFd, mount_ns: &OwnedFd, root: &OwnedFd) -> anyhow::Result<()> {
    setns(user_ns.as_raw_fd()).context("enter workset user namespace")?;
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

use std::ffi::{OsStr, OsString};
use std::path::Path;

use camino::{Utf8Path, Utf8PathBuf};

use crate::layout::MOUNT_ROOT;
use crate::{PathOverrides, UserEnvironment, Workset, WorksetId, Worksets};

/// Filesystem inputs for one execution process: a workset's, or the one
/// host process that runs agents without a workset. The agent host
/// supplies paths; the single-threaded child builds the mounts.
#[derive(Clone, Debug, senax_encoder::Encode, senax_encoder::Decode)]
pub struct Layout {
    /// The workset mounted at `/src`. Without one, the process runs on the
    /// host as it is: no namespace, so no user namespaces are needed.
    pub mount: Option<WorksetMount>,
    pub store_socket: Option<Utf8PathBuf>,
    pub cache: Utf8PathBuf,
    pub state: Utf8PathBuf,
    pub paths: PathOverrides,
}

/// What a workset's namespace mounts.
#[derive(Clone, Debug, senax_encoder::Encode, senax_encoder::Decode)]
pub struct WorksetMount {
    pub workset: WorksetId,
    pub source: Utf8PathBuf,
    pub store_root: Utf8PathBuf,
}

impl Layout {
    pub fn workset(workset: &Workset) -> anyhow::Result<Self> {
        let owner = workset.owner()?;
        Ok(Self {
            mount: Some(WorksetMount {
                workset: workset.id(),
                source: workset.root().to_owned(),
                store_root: owner.store_root(),
            }),
            store_socket: owner.store_socket(),
            cache: owner.cache_dir(),
            state: workset.state_dir()?,
            paths: owner.path_overrides.clone(),
        })
    }

    /// The host process: agents that work in host directories.
    pub fn host(worksets: &Worksets) -> Self {
        Self {
            mount: None,
            store_socket: worksets.store_socket(),
            cache: worksets.cache_dir(),
            state: worksets.root().join("host").join("state"),
            paths: worksets.path_overrides.clone(),
        }
    }

    /// # Safety
    /// Only call in a fresh process before starting any threads.
    pub unsafe fn build(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.state)?;
        let Some(mount) = &self.mount else {
            return Ok(());
        };
        crate::layout::unshare_identity_user_namespace()?;
        crate::layout::unshare_mount_namespace()?;
        let mounts = crate::layout::Mounts {
            src: mount.source.clone().into_std_path_buf(),
            store_root: mount.store_root.clone().into_std_path_buf(),
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
        if self.mount.is_some() {
            std::env::set_current_dir(MOUNT_ROOT)?;
        }
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
        if let Some(find) = crate::find_bin() {
            command.env("RHO_DEVSHELL_PATH_PREFIX", find);
        }
        // The cargo ahead of a dev shell's own.
        if let Some(cargo) = crate::shared_cargo_bin() {
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

pub(crate) fn close_inherited_fds_on_exec() -> std::io::Result<()> {
    if unsafe { libc::close_range(3, u32::MAX, libc::CLOSE_RANGE_CLOEXEC as libc::c_int) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

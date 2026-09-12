//! The agent filesystem view: a private mount namespace whose root is a
//! fresh tmpfs holding only what an agent works with — `/nix/store`, a
//! generated `/etc`, an empty `$HOME`, the workset directory at `/src`, and
//! the read-only clone-store root at its host path.
//!
//! This is a layout, not a sandbox: everything runs as the invoking user,
//! and no security boundary is claimed.

use std::ffi::{CString, OsStr};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::{fs, io};

use anyhow::{Context as _, bail, ensure};

/// Where the workset directory appears to an agent, in every mode.
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

/// The nix daemon's socket; bound into the view when the host has one, and
/// `NIX_REMOTE=daemon` then points the agent's nix at it.
pub const NIX_DAEMON_SOCKET: &str = "/nix/var/nix/daemon-socket/socket";

/// Host-derived files captured before the namespace is constructed.
#[derive(Clone, Debug)]
pub struct HostEtc {
    pub resolv_conf: Vec<u8>,
    pub localtime: Option<PathBuf>,
}

impl HostEtc {
    pub fn discover() -> anyhow::Result<Self> {
        let resolv_conf = fs::read("/etc/resolv.conf").context("read host /etc/resolv.conf")?;
        let localtime = resolve_store_path("/etc/localtime")?;
        Ok(Self {
            resolv_conf,
            localtime,
        })
    }
}

fn resolve_store_path(path: impl AsRef<Path>) -> anyhow::Result<Option<PathBuf>> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(None);
    }
    let resolved = path
        .canonicalize()
        .with_context(|| format!("resolve {}", path.display()))?;
    ensure!(
        resolved.starts_with("/nix/store"),
        "{} resolves outside /nix/store: {}",
        path.display(),
        resolved.display()
    );
    Ok(Some(resolved))
}

#[derive(Clone, Debug)]
pub struct FsViewConfig {
    pub home_skeleton: Option<PathBuf>,
    pub mounts: Mounts,
    pub host_etc: HostEtc,
    /// The base userland (`crate::AGENT_BASE`): shebang targets, the CA
    /// bundle and the flake registry come from it.
    pub base: PathBuf,
    /// The shared persistent cache, mounted read-write as `~/.cache`.
    pub cache: Option<PathBuf>,
    /// The workset's state directory, mounted read-write at its host path
    /// (direnv layout, nix GC roots).
    pub workset_state: Option<PathBuf>,
    /// The directory holding this process's own executable when it lies
    /// outside `/nix/store` (a cargo build), bound read-only at its host
    /// path so a development daemon can launch its sibling sidecars.
    pub own_binaries: Option<PathBuf>,
}

impl FsViewConfig {
    pub fn new(mounts: Mounts) -> anyhow::Result<Self> {
        Ok(Self {
            home_skeleton: None,
            mounts,
            host_etc: HostEtc::discover()?,
            base: PathBuf::from(crate::AGENT_BASE),
            cache: None,
            workset_state: None,
            own_binaries: own_binaries_dir()?,
        })
    }
}

fn own_binaries_dir() -> anyhow::Result<Option<PathBuf>> {
    let exe = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .context("locate own executable")?;
    Ok(exe
        .parent()
        .filter(|dir| !dir.starts_with("/nix/store"))
        .map(Path::to_owned))
}

pub struct FsViewBuilder {
    config: FsViewConfig,
}

impl FsViewBuilder {
    pub fn new(config: FsViewConfig) -> anyhow::Result<Self> {
        validate_mounts(&config.mounts)?;
        if let Some(skeleton) = &config.home_skeleton {
            ensure!(
                skeleton.is_dir(),
                "HOME skeleton is not a directory: {}",
                skeleton.display()
            );
        }
        Ok(Self { config })
    }

    /// Builds the generated filesystem at `root` in the current mount
    /// namespace. This does not unshare, pivot the caller's root, or change
    /// its environment; the caller must already hold mount capability in
    /// the current user namespace.
    pub fn build_in_place(&self, root: &Path) -> anyhow::Result<()> {
        build_filesystem(&self.config, root)
    }

    /// Pivots the calling thread's mount namespace into a root produced by
    /// [`Self::build_in_place`], detaching the host root.
    pub fn pivot_into(&self, root: &Path) -> anyhow::Result<()> {
        pivot_into(root)
    }
}

/// Exposed mode: the full host view as the invoking user, plus the workset
/// directory mounted over the host's permanently empty `/src` stub and the
/// store root made read-only. Environment, `$HOME`, and every other host
/// path stay exactly as they are.
pub struct ExposedBuilder {
    mounts: Mounts,
}

impl ExposedBuilder {
    pub fn new(mounts: Mounts) -> anyhow::Result<Self> {
        validate_mounts(&mounts)?;
        Ok(Self { mounts })
    }

    /// Mounts the workset directory over `root/src` in the current mount
    /// namespace without unsharing or changing cwd/environment. The stub
    /// must exist: an unprivileged mount namespace can only mount over a
    /// directory that is already there.
    pub fn build_in_place(&self, root: &Path) -> anyhow::Result<()> {
        let stub = mount_root(root);
        ensure!(
            stub.is_dir(),
            "host {MOUNT_ROOT} mount stub is missing: {} (deploy it with systemd-tmpfiles: d {MOUNT_ROOT} 0500 root root -)",
            stub.display()
        );
        mount_in_place(&self.mounts, root)
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

fn build_filesystem(config: &FsViewConfig, root: &Path) -> anyhow::Result<()> {
    // The root is one fresh tmpfs; /home/agent, /tmp, /src, and /dev are plain
    // directories on it rather than mounts of their own.
    mount_fs(
        Some(OsStr::new("tmpfs")),
        root,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some("mode=0755"),
    )?;
    for dir in [
        "nix/store",
        "etc",
        "bin",
        "usr/bin",
        "home/agent",
        "tmp",
        "proc",
        "dev",
        "src",
        "old-root",
    ] {
        fs::create_dir_all(root.join(dir)).with_context(|| format!("create /{dir}"))?;
    }
    bind(Path::new("/nix/store"), &root.join("nix/store"), true)?;
    // The nix daemon's socket, so `nix` in the view builds through it.
    let nix_socket = Path::new(NIX_DAEMON_SOCKET);
    if nix_socket.exists() {
        let target = host_path_in(root, nix_socket);
        fs::create_dir_all(target.parent().expect("socket has a directory"))?;
        fs::File::create(&target).context("create nix daemon socket mount point")?;
        bind(nix_socket, &target, false)?;
    }
    // Shebang targets: the two paths scripts hardcode. Everything else on
    // PATH comes from the base and the agent's profile.
    symlink(config.base.join("bin/sh"), root.join("bin/sh")).context("link /bin/sh")?;
    symlink(config.base.join("bin/env"), root.join("usr/bin/env")).context("link /usr/bin/env")?;
    if let Some(dir) = &config.own_binaries {
        let target = host_path_in(root, dir);
        fs::create_dir_all(&target).with_context(|| format!("create {}", target.display()))?;
        bind(dir, &target, true)?;
    }
    // The pid namespace is the host's, so a fresh procfs mount is not
    // permitted here; the host's proc view is the correct one anyway.
    bind(Path::new("/proc"), &root.join("proc"), false)?;
    write_etc(config, root)?;

    fs::set_permissions(root.join("home/agent"), fs::Permissions::from_mode(0o700))?;
    if let Some(skeleton) = &config.home_skeleton {
        copy_tree(skeleton, &root.join("home/agent"))?;
    }
    // The XDG directories the environment names; some tools fail rather
    // than create them.
    for dir in [".config", ".local/state", ".local/share", ".cache"] {
        fs::create_dir_all(root.join("home/agent").join(dir))
            .with_context(|| format!("create ~/{dir}"))?;
    }
    if let Some(cache) = &config.cache {
        bind(cache, &root.join("home/agent/.cache"), false)?;
    }
    if let Some(state) = &config.workset_state {
        let target = host_path_in(root, state);
        fs::create_dir_all(&target).with_context(|| format!("create {}", target.display()))?;
        bind(state, &target, false)?;
    }
    fs::set_permissions(root.join("tmp"), fs::Permissions::from_mode(0o1777))?;
    build_dev(root)?;
    mount_in_place(&config.mounts, root)?;
    Ok(())
}

fn write_etc(config: &FsViewConfig, root: &Path) -> anyhow::Result<()> {
    let etc = root.join("etc");
    let (agent_uid, agent_gid) = (unsafe { libc::getuid() }, unsafe { libc::getgid() });
    fs::write(
        etc.join("passwd"),
        format!(
            "root:x:0:0:root:/root:/bin/sh\nagent:x:{agent_uid}:{agent_gid}:rho agent:/home/agent:/bin/sh\n"
        ),
    )?;
    fs::write(
        etc.join("group"),
        format!("root:x:0:\nagent:x:{agent_gid}:\n"),
    )?;
    fs::write(etc.join("resolv.conf"), &config.host_etc.resolv_conf)?;
    fs::write(etc.join("hosts"), "127.0.0.1 localhost\n::1 localhost\n")?;
    fs::write(
        etc.join("nsswitch.conf"),
        "passwd: files\ngroup: files\nhosts: files dns\n",
    )?;
    let ca_bundle = config.base.join("etc/ssl/certs/ca-bundle.crt");
    if ca_bundle.exists() {
        fs::create_dir_all(etc.join("ssl/certs"))?;
        symlink(ca_bundle, etc.join("ssl/certs/ca-certificates.crt"))?;
    }
    if let Some(localtime) = &config.host_etc.localtime {
        symlink(localtime, etc.join("localtime"))?;
    }
    fs::create_dir_all(etc.join("nix"))?;
    fs::write(
        etc.join("nix/nix.conf"),
        "experimental-features = nix-command flakes\n",
    )?;
    // `nixpkgs` pinned to the revision Rho is built from, so installs
    // share the base's closure and need no lookup of what is newest.
    let registry = config.base.join("etc/nix/registry.json");
    if registry.exists() {
        symlink(registry, etc.join("nix/registry.json"))?;
    }
    // Git's behaviour (VIEW.md 6); identity is environment.
    fs::write(
        etc.join("gitconfig"),
        "[core]\n\tpager = cat\n[commit]\n\tgpgSign = false\n[tag]\n\tgpgSign = false\n[init]\n\tdefaultBranch = main\n",
    )?;
    // direnv under Rho's configuration (VIEW.md 3): everything under /src
    // is trusted, the layout lives in the workset's state directory, and
    // `use flake` puts the daemon's find fork and cargo's bin first.
    fs::create_dir_all(etc.join("rho/direnv"))?;
    fs::write(
        etc.join("rho/direnv/direnv.toml"),
        "[whitelist]\nprefix = [ \"/src\" ]\n",
    )?;
    fs::write(
        etc.join("rho/direnv/direnvrc"),
        format!(
            r#"source {base}/share/nix-direnv/direnvrc
eval "$(declare -f use_flake | sed '1s/use_flake/rho_nix_direnv_use_flake/')"

direnv_layout_dir() {{
    local checkout key
    checkout="$(pwd -P)"
    key="$(printf '%s' "$checkout" | sha256sum)"
    printf '%s/%s
' "${{RHO_DIRENV_LAYOUT_DIR:?RHO_DIRENV_LAYOUT_DIR is not set}}" "${{key%% *}}"
}}

use_flake() {{
    rho_nix_direnv_use_flake "$@" || return
    PATH="${{RHO_DIRENV_PATH_BEFORE:+${{RHO_DIRENV_PATH_BEFORE}}:}}${{CARGO_HOME:-$HOME/.cache/cargo}}/bin:${{PATH}}"
    export PATH
}}
"#,
            base = config.base.display()
        ),
    )?;
    // The nixpkgs bash reads these itself (SYS_BASHRC).
    fs::write(
        etc.join("bashrc"),
        "eval \"$(direnv hook bash)\"\nPS1='agent:\\w\\$ '\n",
    )?;
    fs::write(etc.join("profile"), "[ -r /etc/bashrc ] && . /etc/bashrc\n")?;
    Ok(())
}

fn build_dev(root: &Path) -> anyhow::Result<()> {
    let dev = root.join("dev");
    for name in ["null", "zero", "full", "random", "urandom", "tty"] {
        let target = dev.join(name);
        fs::File::create(&target)?;
        bind(&Path::new("/dev").join(name), &target, false)?;
    }
    fs::create_dir(dev.join("pts"))?;
    mount_fs(
        Some(OsStr::new("devpts")),
        &dev.join("pts"),
        Some("devpts"),
        libc::MS_NOSUID | libc::MS_NOEXEC,
        Some("newinstance,ptmxmode=0666,mode=0620"),
    )?;
    symlink("pts/ptmx", dev.join("ptmx"))?;
    fs::create_dir(dev.join("shm"))?;
    fs::set_permissions(dev.join("shm"), fs::Permissions::from_mode(0o1777))?;
    Ok(())
}

/// Captures one host directory or file as a detached mount tree that can be
/// installed into another mount namespace with [`install_captured_mount`].
/// Must run in a private mount namespace: it stages a bind mount in a
/// temporary location while cloning it.
pub fn capture_mount(source: &Path) -> anyhow::Result<OwnedFd> {
    clone_mount(source)
}

/// Installs a tree captured by [`capture_mount`] over `target`, read-write.
pub fn install_captured_mount(source: &OwnedFd, target: &Path) -> anyhow::Result<()> {
    install_mount(source, target, false)
}

/// Lazily detaches the mount at `target` together with everything mounted
/// below it.
pub fn detach_mount(target: &Path) -> anyhow::Result<()> {
    let path = cstring(target)?;
    if unsafe { libc::umount2(path.as_ptr(), libc::MNT_DETACH) } < 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("detach mount at {}", target.display()));
    }
    Ok(())
}

fn clone_mount(source: &Path) -> anyhow::Result<OwnedFd> {
    // A bind mount needs a target of the same kind, so files stage over a
    // temporary file and directories over a temporary directory.
    let is_dir = fs::metadata(source)
        .with_context(|| format!("stat mount source {}", source.display()))?
        .is_dir();
    let staging_dir;
    let staging_file;
    let staging: &Path = if is_dir {
        staging_dir = tempfile::tempdir().context("create mount staging directory")?;
        staging_dir.path()
    } else {
        staging_file = tempfile::NamedTempFile::new().context("create mount staging file")?;
        staging_file.path()
    };
    bind(source, staging, false)?;
    let path = cstring(staging)?;
    const OPEN_TREE_CLONE: libc::c_uint = 1;
    const AT_RECURSIVE: libc::c_uint = 0x8000;
    let fd = unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            libc::AT_FDCWD,
            path.as_ptr(),
            OPEN_TREE_CLONE | libc::O_CLOEXEC as libc::c_uint | AT_RECURSIVE,
        ) as i32
    };
    let open_result = if fd < 0 {
        Err(io::Error::last_os_error()).context("clone staged mount")
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    };
    let unmounted = unsafe { libc::umount2(path.as_ptr(), libc::MNT_DETACH) };
    if unmounted < 0 {
        return Err(io::Error::last_os_error()).context("unmount staged source");
    }
    open_result
}

fn install_mount(source: &OwnedFd, target: &Path, readonly: bool) -> anyhow::Result<()> {
    let target = cstring(target)?;
    const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 4;
    let result = unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            source.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            MOVE_MOUNT_F_EMPTY_PATH,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error()).context("install prepared mount");
    }
    set_mount_attributes(Path::new(OsStr::from_bytes(target.as_bytes())), readonly)
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

fn pivot_into(root: &Path) -> anyhow::Result<()> {
    let root_c = cstring(root)?;
    let old = cstring(&root.join("old-root"))?;
    cvt(unsafe { libc::syscall(libc::SYS_pivot_root, root_c.as_ptr(), old.as_ptr()) as i32 })
        .context("pivot_root")?;
    cvt(unsafe { libc::chdir(c"/".as_ptr()) }).context("chdir /")?;
    cvt(unsafe { libc::umount2(c"/old-root".as_ptr(), libc::MNT_DETACH) })
        .context("detach host root")?;
    fs::remove_dir("/old-root").context("remove old root mount point")?;
    Ok(())
}

fn copy_tree(source: &Path, target: &Path) -> anyhow::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        let metadata = fs::symlink_metadata(&from)?;
        if metadata.is_dir() {
            fs::create_dir(&to)?;
            fs::set_permissions(&to, fs::Permissions::from_mode(metadata.mode()))?;
            copy_tree(&from, &to)?;
        } else if metadata.file_type().is_symlink() {
            symlink(fs::read_link(&from)?, &to)?;
        } else if metadata.is_file() {
            fs::copy(&from, &to)?;
            fs::set_permissions(&to, fs::Permissions::from_mode(metadata.mode()))?;
        } else {
            bail!("unsupported skeleton entry: {}", from.display());
        }
    }
    Ok(())
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

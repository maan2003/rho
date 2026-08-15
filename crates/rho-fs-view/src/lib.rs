//! The agent filesystem view: a private mount namespace whose root is a
//! fresh tmpfs holding only what an agent works with — `/nix/store`, a
//! generated `/etc`, an empty `$HOME`, and the `/src` workspace tree.
//!
//! This is a layout, not a sandbox: everything runs as the invoking user,
//! and no security boundary is claimed.

use std::ffi::{CString, OsStr, OsString};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Component, Path, PathBuf};
use std::process::ExitStatus;
use std::{fs, io};

use anyhow::{Context as _, bail, ensure};

pub const VIEW_MOUNT_ROOT: &str = "/src";
pub const EXPOSED_MOUNT_ROOT: &str = "/ws";

#[derive(Clone, Debug)]
pub struct WorkspaceMount {
    pub name: String,
    pub source: PathBuf,
}

#[derive(Clone, Debug)]
pub struct StoreMount {
    pub name: String,
    pub source: PathBuf,
    /// The id below `source/clones` owned by this workset. That subtree is
    /// mounted read-write over the otherwise read-only store.
    pub writable_clone: String,
}

#[derive(Clone, Debug, Default)]
pub struct Mounts {
    pub workspaces: Vec<WorkspaceMount>,
    pub stores: Vec<StoreMount>,
}

/// Host-derived files captured before the namespace is constructed.
#[derive(Clone, Debug)]
pub struct HostEtc {
    pub resolv_conf: Vec<u8>,
    pub ssl_cert_file: Option<PathBuf>,
    pub localtime: Option<PathBuf>,
}

impl HostEtc {
    pub fn discover() -> anyhow::Result<Self> {
        let resolv_conf = fs::read("/etc/resolv.conf").context("read host /etc/resolv.conf")?;
        let ssl_cert_file = resolve_store_path("/etc/ssl/certs/ca-certificates.crt")?;
        let localtime = resolve_store_path("/etc/localtime")?;
        Ok(Self {
            resolv_conf,
            ssl_cert_file,
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
    /// Explicit non-secret environment passed through to the command. HOME,
    /// USER, and LOGNAME are always set by the builder.
    pub environment: Vec<(OsString, OsString)>,
}

impl FsViewConfig {
    pub fn new(mounts: Mounts) -> anyhow::Result<Self> {
        Ok(Self {
            home_skeleton: None,
            mounts,
            host_etc: HostEtc::discover()?,
            environment: ["PATH", "TERM"]
                .into_iter()
                .filter_map(|name| std::env::var_os(name).map(|value| (name.into(), value)))
                .collect(),
        })
    }
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
    /// namespace. This does not fork, unshare, pivot the caller's root, or
    /// change its environment.
    ///
    /// The caller must already have mount capability in the current user
    /// namespace. Keeping this operation separate lets a daemon construct a
    /// long-lived agent namespace while the development runner continues to
    /// use the fork-and-exec convenience path.
    pub fn build_in_place(&self, root: &Path) -> anyhow::Result<()> {
        build_filesystem(&self.config, root)
    }

    /// Pivots the current process into a root produced by
    /// [`Self::build_in_place`] and starts it in `/src`.
    pub fn pivot_into(&self, root: &Path) -> anyhow::Result<()> {
        pivot_into(root)
    }

    /// Replaces the current process environment with the view's allowlist.
    pub fn apply_environment(&self) -> anyhow::Result<()> {
        apply_view_environment(&self.config)
    }

    /// Runs a command inside the view (a new user and mount namespace).
    ///
    /// # Safety
    ///
    /// The calling process must be single-threaded. This implementation forks
    /// and performs filesystem construction before exec. The dev launcher is
    /// deliberately a tiny single-threaded process; a later daemon integration
    /// must launch such a helper rather than call this from the Tokio process.
    pub unsafe fn run<I, S>(&self, program: &OsStr, args: I) -> anyhow::Result<ExitStatus>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let (program, argv) = prepare_command(program, args)?;
        let root = tempfile::Builder::new()
            .prefix("rho-fs-view-")
            .tempdir()
            .context("create view mount point")?;
        // SAFETY: required by this method's single-threaded contract.
        unsafe { spawn_setup(|| setup_child(&self.config, root.path(), &program, &argv)) }
    }
}

/// Exposed mode: the full host view as the invoking user, plus the same
/// `/ws` mount-list tree, mounted over the host's permanently empty `/ws` stub.
/// Environment, `$HOME`, and every host path stay exactly as they are.
pub struct ExposedBuilder {
    mounts: Mounts,
}

impl ExposedBuilder {
    pub fn new(mounts: Mounts) -> anyhow::Result<Self> {
        validate_mounts(&mounts)?;
        Ok(Self { mounts })
    }

    /// Mounts the mount-list tmpfs at `root/ws` in the current mount
    /// namespace without forking, unsharing, or changing cwd/environment.
    pub fn build_in_place(&self, root: &Path) -> anyhow::Result<()> {
        let ws = mount_root(root, EXPOSED_MOUNT_ROOT);
        ensure!(
            ws.is_dir(),
            "mount-list mount stub is missing: {}",
            ws.display()
        );
        mount_fs(
            Some(OsStr::new("tmpfs")),
            &ws,
            Some("tmpfs"),
            libc::MS_NOSUID | libc::MS_NODEV,
            Some("mode=0755"),
        )
        .with_context(|| format!("mount tmpfs over {}", ws.display()))?;
        fs::create_dir(ws.join(".stores")).context("create mount-list .stores")?;
        mount_in_place(&self.mounts, root, EXPOSED_MOUNT_ROOT)
    }

    /// Runs a command with `/ws` mounted (a new user and mount namespace).
    ///
    /// # Safety
    ///
    /// The calling process must be single-threaded; see [`FsViewBuilder::run`].
    pub unsafe fn run<I, S>(&self, program: &OsStr, args: I) -> anyhow::Result<ExitStatus>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let (program, argv) = prepare_command(program, args)?;
        // SAFETY: required by this method's single-threaded contract.
        unsafe { spawn_setup(|| setup_exposed(&self.mounts, &program, &argv)) }
    }
}

fn prepare_command<I, S>(program: &OsStr, args: I) -> anyhow::Result<(CString, Vec<CString>)>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let program = CString::new(program.as_bytes()).context("program contains NUL")?;
    let mut argv = vec![program.clone()];
    for arg in args {
        argv.push(CString::new(arg.as_ref().as_bytes()).context("argument contains NUL")?);
    }
    Ok((program, argv))
}

/// Forks; the child runs `setup`, which only returns on error (on success
/// exec replaces it). The parent waits for the command's exit status.
unsafe fn spawn_setup(setup: impl FnOnce() -> anyhow::Result<()>) -> anyhow::Result<ExitStatus> {
    let parent_pid = unsafe { libc::getpid() };
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error()).context("fork setup process");
    }
    if pid == 0 {
        let result = arm_parent_death_signal(parent_pid).and_then(|()| setup());
        if let Err(ref error) = result {
            eprintln!("rho-fs-view setup: {error:#}");
        }
        unsafe { libc::_exit(125) }
    }
    wait_pid(pid)
}

fn validate_mounts(set: &Mounts) -> anyhow::Result<()> {
    let mut targets = std::collections::HashSet::new();
    for workspace in &set.workspaces {
        validate_name(&workspace.name)?;
        ensure!(
            workspace.source.is_dir(),
            "workspace is not a directory: {}",
            workspace.source.display()
        );
        ensure!(
            targets.insert(format!("w/{}", workspace.name)),
            "duplicate workspace name: {}",
            workspace.name
        );
    }
    for store in &set.stores {
        validate_name(&store.name)?;
        validate_name(&store.writable_clone)?;
        ensure!(
            store.source.is_dir(),
            "store is not a directory: {}",
            store.source.display()
        );
        ensure!(
            store
                .source
                .join("clones")
                .join(&store.writable_clone)
                .is_dir(),
            "writable clone is not a directory: {}/clones/{}",
            store.source.display(),
            store.writable_clone
        );
        ensure!(
            targets.insert(format!("s/{}", store.name)),
            "duplicate store name: {}",
            store.name
        );
    }
    Ok(())
}

fn validate_name(name: &str) -> anyhow::Result<()> {
    ensure!(!name.is_empty(), "mount name is empty");
    ensure!(
        Path::new(name)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
            && !name.contains('/'),
        "mount name is not one path component: {name}"
    );
    Ok(())
}

fn setup_child(
    config: &FsViewConfig,
    root: &Path,
    program: &CString,
    argv: &[CString],
) -> anyhow::Result<()> {
    mark_inherited_fds_close_on_exec()?;
    unshare_identity_namespaces()?;
    unsafe { libc::umask(0o022) };
    let builder = FsViewBuilder {
        config: config.clone(),
    };
    builder.build_in_place(root)?;
    builder.pivot_into(root)?;
    builder.apply_environment()?;
    exec_command(program, argv)
}

fn setup_exposed(set: &Mounts, program: &CString, argv: &[CString]) -> anyhow::Result<()> {
    ensure!(
        Path::new(EXPOSED_MOUNT_ROOT).is_dir(),
        "host /ws mount stub is missing (deployed via systemd-tmpfiles: d /ws 0500 root root -)"
    );
    mark_inherited_fds_close_on_exec()?;
    unshare_identity_namespaces()?;
    ExposedBuilder {
        mounts: set.clone(),
    }
    .build_in_place(Path::new("/"))?;
    let mount_root = CString::new(EXPOSED_MOUNT_ROOT)?;
    cvt(unsafe { libc::chdir(mount_root.as_ptr()) }).context("chdir mount root")?;
    exec_command(program, argv)
}

/// Unshares user and mount namespaces, maps the caller's uid/gid onto
/// themselves, and makes all mounts private to the namespace.
pub fn unshare_identity_namespaces() -> anyhow::Result<()> {
    unshare_identity_user_namespace()?;
    unshare_mount_namespace()
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

fn arm_parent_death_signal(expected_parent: libc::pid_t) -> anyhow::Result<()> {
    cvt(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) })
        .context("arm parent-death signal")?;
    ensure!(
        unsafe { libc::getppid() } == expected_parent,
        "view launcher exited during setup"
    );
    Ok(())
}

fn mark_inherited_fds_close_on_exec() -> anyhow::Result<()> {
    // stdin/stdout/stderr are the dev launcher's deliberate command channel.
    // Everything else, including descriptors the eventual daemon happens to
    // hold, must disappear at exec. CLOEXEC rather than immediate close keeps
    // this setup process's synchronization pipes usable until then.
    let result = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3_u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error()).context("mark inherited fds close-on-exec");
    }
    Ok(())
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
        "home/agent",
        "tmp",
        "proc",
        "dev",
        "src/.stores",
        "old-root",
    ] {
        fs::create_dir_all(root.join(dir)).with_context(|| format!("create /{dir}"))?;
    }
    bind(Path::new("/nix/store"), &root.join("nix/store"), true)?;
    // The pid namespace is the host's, so a fresh procfs mount is not
    // permitted here; the host's proc view is the correct one anyway.
    bind(Path::new("/proc"), &root.join("proc"), false)?;
    write_etc(config, root)?;

    fs::set_permissions(root.join("home/agent"), fs::Permissions::from_mode(0o700))?;
    if let Some(skeleton) = &config.home_skeleton {
        copy_tree(skeleton, &root.join("home/agent"))?;
    }
    fs::set_permissions(root.join("tmp"), fs::Permissions::from_mode(0o1777))?;
    build_dev(root)?;
    mount_in_place(&config.mounts, root, VIEW_MOUNT_ROOT)?;
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
    if let Some(cert) = &config.host_etc.ssl_cert_file {
        fs::create_dir_all(etc.join("ssl/certs"))?;
        symlink(cert, etc.join("ssl/certs/ca-certificates.crt"))?;
    }
    if let Some(localtime) = &config.host_etc.localtime {
        symlink(localtime, etc.join("localtime"))?;
    }
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

struct PreparedEntry {
    name: String,
    source: OwnedFd,
}
struct PreparedStore {
    name: String,
    source: OwnedFd,
    writable_clone: String,
    clone_source: OwnedFd,
}
/// Detached mount trees captured before entering a target mount namespace.
pub struct PreparedMounts {
    workspaces: Vec<PreparedEntry>,
    stores: Vec<PreparedStore>,
}

/// Captures mount sources in detached trees that survive a namespace switch.
pub fn prepare_mounts(set: &Mounts) -> anyhow::Result<PreparedMounts> {
    validate_mounts(set)?;
    let workspaces = set
        .workspaces
        .iter()
        .map(|entry| {
            Ok(PreparedEntry {
                name: entry.name.clone(),
                source: clone_mount(&entry.source)?,
            })
        })
        .collect::<anyhow::Result<_>>()?;
    let stores = set
        .stores
        .iter()
        .map(|entry| {
            Ok(PreparedStore {
                name: entry.name.clone(),
                source: clone_mount(&entry.source)?,
                writable_clone: entry.writable_clone.clone(),
                clone_source: clone_mount(
                    &entry.source.join("clones").join(&entry.writable_clone),
                )?,
            })
        })
        .collect::<anyhow::Result<_>>()?;
    Ok(PreparedMounts { workspaces, stores })
}

fn clone_mount(source: &Path) -> anyhow::Result<OwnedFd> {
    let staging = tempfile::tempdir().context("create mount staging directory")?;
    bind(source, staging.path(), false)?;
    let path = cstring(staging.path())?;
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

/// Installs previously captured mount trees below a visible root.
pub fn mount_prepared_in_place(
    set: &PreparedMounts,
    root: &Path,
    visible_root: &str,
) -> anyhow::Result<()> {
    let ws = mount_root(root, visible_root);
    for store in &set.stores {
        let target = ws.join(".stores").join(&store.name);
        fs::create_dir(&target)?;
        install_mount(&store.source, &target, true)?;
        install_mount(
            &store.clone_source,
            &target.join("clones").join(&store.writable_clone),
            false,
        )?;
    }
    for entry in &set.workspaces {
        let target = ws.join(&entry.name);
        fs::create_dir(&target)?;
        install_mount(&entry.source, &target, false)?;
    }
    Ok(())
}

/// Adds a validated working set below the selected visible root in the current
/// mount namespace. This is also the primitive used to refresh a live namespace
/// after a daemon grants another store or creates another workspace. For
/// refresh, pass a `Mounts` containing only the newly added entries; collisions
/// with already-mounted names fail when their target directory is created.
pub fn mount_in_place(set: &Mounts, root: &Path, visible_root: &str) -> anyhow::Result<()> {
    validate_mounts(set)?;
    let ws = mount_root(root, visible_root);
    for store in &set.stores {
        let target = ws.join(".stores").join(&store.name);
        fs::create_dir(&target)?;
        bind(&store.source, &target, true)?;
        let clone_target = target.join("clones").join(&store.writable_clone);
        bind(
            &store.source.join("clones").join(&store.writable_clone),
            &clone_target,
            false,
        )?;
    }
    for workspace in &set.workspaces {
        let target = ws.join(&workspace.name);
        fs::create_dir(&target)?;
        bind(&workspace.source, &target, false)?;
    }
    Ok(())
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
    let mount_root = CString::new(VIEW_MOUNT_ROOT)?;
    cvt(unsafe { libc::chdir(mount_root.as_ptr()) }).context("chdir mount root")?;
    Ok(())
}

fn apply_view_environment(config: &FsViewConfig) -> anyhow::Result<()> {
    cvt(unsafe { libc::clearenv() }).context("clear inherited environment")?;
    for (name, value) in &config.environment {
        let name = CString::new(name.as_bytes()).context("environment name contains NUL")?;
        let value = CString::new(value.as_bytes()).context("environment value contains NUL")?;
        cvt(unsafe { libc::setenv(name.as_ptr(), value.as_ptr(), 1) })
            .context("set configured environment")?;
    }
    unsafe {
        libc::setenv(c"HOME".as_ptr(), c"/home/agent".as_ptr(), 1);
        libc::setenv(c"USER".as_ptr(), c"agent".as_ptr(), 1);
        libc::setenv(c"LOGNAME".as_ptr(), c"agent".as_ptr(), 1);
    }
    Ok(())
}

fn exec_command(program: &CString, argv: &[CString]) -> anyhow::Result<()> {
    let mut pointers = argv.iter().map(|arg| arg.as_ptr()).collect::<Vec<_>>();
    pointers.push(std::ptr::null());
    unsafe { libc::execvp(program.as_ptr(), pointers.as_ptr()) };
    Err(io::Error::last_os_error()).with_context(|| format!("exec {}", program.to_string_lossy()))
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

fn mount_root(root: &Path, visible_root: &str) -> PathBuf {
    root.join(visible_root.trim_start_matches('/'))
}
fn cvt(value: i32) -> io::Result<i32> {
    if value < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}
fn wait_pid(pid: libc::pid_t) -> anyhow::Result<ExitStatus> {
    let mut status = 0;
    loop {
        let result = unsafe { libc::waitpid(pid, &mut status, 0) };
        if result == pid {
            return Ok(std::os::unix::process::ExitStatusExt::from_raw(status));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error).context("waitpid");
        }
    }
}

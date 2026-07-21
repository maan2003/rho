// Copyright 2026 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
//! Repository-local managed workspaces.
//!
//! Managed IDs are allocated by jj and are deliberately separate from user
//! workspace names. Their full `ws-<id>` handle is used as the internal
//! workspace name, while checkout and lease paths are derived solely from the
//! repository path and ID.

use std::ffi::CString;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::ErrorKind;
use std::io::Write as _;
use std::os::fd::AsFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use jj_lib::file_util::IoResultExt as _;
use jj_lib::object_id::ObjectId as _;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::merge_commit_trees;
use jj_lib::workspace::Workspace;
use jj_lib::workspace_store::SimpleWorkspaceStore;
use jj_lib::workspace_store::WorkspaceStore as _;
use prefix_id::PrefixId;
use prefix_id::PrefixIdDomain;
use prefix_id::PrefixResolution;
use rustix::fs::FlockOperation;
use rustix::ioctl::Opcode;
use rustix::ioctl::Setter;
use serde_json::json;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::cli_util::WorkspaceCommandHelper;
use crate::command_error::CommandError;
use crate::command_error::internal_error_with_message;
use crate::command_error::user_error;
use crate::ui::Ui;

#[derive(Debug)]
struct ManagedIdDomain(u64);
impl PrefixIdDomain for ManagedIdDomain {
    const KIND: &'static str = "managed-workspace-id";
    fn machine_seed(&self) -> u64 {
        self.0
    }
}
type ManagedId = PrefixId<ManagedIdDomain>;

#[derive(Debug)]
struct Registry {
    seed: u64,
    counter: u64,
}

struct RegistryLock {
    _lock: File,
    path: PathBuf,
}

impl RegistryLock {
    fn load(repo_path: &Path) -> Result<(Self, Registry), CommandError> {
        let path = repo_path.join("managed-workspaces");
        let lock_path = path.with_extension("lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .context(&lock_path)?;
        rustix::fs::flock(&lock, FlockOperation::LockExclusive).map_err(|error| {
            user_error(format!(
                "Failed to lock managed workspace registry {}: {error}",
                lock_path.display()
            ))
        })?;
        let registry = match fs::read_to_string(&path) {
            Ok(text) => parse_registry(&text, &path)?,
            Err(err) if err.kind() == ErrorKind::NotFound => Registry {
                seed: rand::random(),
                counter: 0,
            },
            Err(err) => return Ok(Err(err).context(&path)?),
        };
        Ok((Self { _lock: lock, path }, registry))
    }

    fn store(&self, registry: &Registry) -> Result<(), CommandError> {
        let text = format!("{} {}\n", registry.seed, registry.counter);
        let pending = self.path.with_extension("pending");
        fs::write(&pending, text).context(&pending)?;
        fs::rename(&pending, &self.path).context(&self.path)?;
        Ok(())
    }
}

fn parse_registry(text: &str, path: &Path) -> Result<Registry, CommandError> {
    let mut lines = text.lines();
    let mut header = lines.next().unwrap_or_default().split_whitespace();
    let malformed = || {
        user_error(format!(
            "Malformed managed workspace registry: {}",
            path.display()
        ))
    };
    let seed = header
        .next()
        .ok_or_else(malformed)?
        .parse()
        .map_err(|_| malformed())?;
    let counter = header
        .next()
        .ok_or_else(malformed)?
        .parse()
        .map_err(|_| malformed())?;
    if header.next().is_some() {
        return Err(malformed());
    }
    if lines.next().is_some() {
        return Err(malformed());
    }
    Ok(Registry { seed, counter })
}

fn allocate(repo_path: &Path) -> Result<String, CommandError> {
    let (lock, mut registry) = RegistryLock::load(repo_path)?;
    let domain = ManagedIdDomain(registry.seed);
    let id = ManagedId::from_counter(registry.counter, &domain)
        .ok_or_else(|| user_error("Managed workspace id space is exhausted"))?
        .encoded();
    registry.counter += 1;
    lock.store(&registry)?;
    Ok(id)
}

fn resolve_id(repo_path: &Path, handle: &str) -> Result<String, CommandError> {
    let (_, registry) = RegistryLock::load(repo_path)?;
    let prefix = handle
        .strip_prefix("ws-")
        .ok_or_else(|| user_error("Managed workspace handles must start with 'ws-'"))?;
    if prefix.is_empty() {
        return Err(user_error("Managed workspace ID prefix is empty"));
    }
    let domain = ManagedIdDomain(registry.seed);
    match ManagedId::from_prefix(prefix, registry.counter, &domain) {
        Ok(PrefixResolution::Unique(id) | PrefixResolution::Ambiguous { first: id, .. }) => {
            Ok(id.encoded())
        }
        Ok(PrefixResolution::NotFound) | Err(_) => Err(user_error(format!(
            "No managed workspace matches 'ws-{prefix}'"
        ))),
    }
}

pub(super) fn is_managed_name(repo_path: &Path, name: &WorkspaceName) -> bool {
    let Some(id) = name.as_str().strip_prefix("ws-") else {
        return false;
    };
    let path = repo_path.join("managed-workspaces");
    fs::read_to_string(&path)
        .ok()
        .and_then(|text| parse_registry(&text, &path).ok())
        .is_some_and(|registry| {
            let domain = ManagedIdDomain(registry.seed);
            matches!(
                ManagedId::from_prefix(id, registry.counter, &domain),
                Ok(PrefixResolution::Unique(found)) if found.encoded() == id
            )
        })
}

fn workspace_name(id: &str) -> WorkspaceNameBuf {
    format!("ws-{id}").into()
}
fn base_path(repo_path: &Path) -> PathBuf {
    repo_path.parent().unwrap().join("managed-workspaces")
}
fn root_path(repo_path: &Path, id: &str) -> PathBuf {
    base_path(repo_path).join(format!("ws-{id}"))
}
fn lock_path(repo_path: &Path, id: &str) -> PathBuf {
    base_path(repo_path).join(format!("ws-{id}.lock"))
}

struct Lease {
    file: File,
    heartbeat: bool,
}

#[derive(Clone, Copy)]
enum LeaseMode {
    Shared,
    TryExclusive,
}

impl Lease {
    fn acquire(
        path: &Path,
        mode: LeaseMode,
        heartbeat: bool,
    ) -> Result<Option<Self>, CommandError> {
        fs::create_dir_all(path.parent().unwrap()).context(path.parent().unwrap())?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .context(path)?;
        let op = match mode {
            LeaseMode::Shared => FlockOperation::LockShared,
            LeaseMode::TryExclusive => FlockOperation::NonBlockingLockExclusive,
        };
        match rustix::fs::flock(&file, op) {
            Ok(()) => {
                if heartbeat {
                    file.set_modified(std::time::SystemTime::now())
                        .context(path)?;
                }
                Ok(Some(Self { file, heartbeat }))
            }
            Err(rustix::io::Errno::WOULDBLOCK) if matches!(mode, LeaseMode::TryExclusive) => {
                Ok(None)
            }
            Err(err) => Err(user_error(format!(
                "Failed to lock {}: {err}",
                path.display()
            ))),
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if self.heartbeat {
            drop(self.file.set_modified(std::time::SystemTime::now()));
        }
        let _ = rustix::fs::flock(&self.file, FlockOperation::Unlock);
    }
}

#[derive(clap::Subcommand, Clone, Debug)]
pub enum ManagedWorkspaceCommand {
    /// Allocate and materialize a managed workspace
    Create(CreateArgs),
    /// Report stable paths without materializing the workspace
    Resolve(IdArgs),
    /// Idempotently materialize an allocated workspace
    Attach(AttachArgs),
    /// Snapshot and delete stale, unlocked materializations
    Gc(GcArgs),
}
#[derive(clap::Args, Clone, Debug)]
pub struct CreateArgs {
    /// Revision on which to create the working-copy commit
    #[arg(long = "revision", short = 'r', default_value = "@")]
    revision: RevisionArg,
}
#[derive(clap::Args, Clone, Debug)]
pub struct IdArgs {
    /// Full ws-id or prefix; ambiguous prefixes select their first generated
    /// match
    id: String,
}
#[derive(clap::Args, Clone, Debug)]
pub struct AttachArgs {
    /// Full ws-id or prefix; ambiguous prefixes select their first generated
    /// match
    id: String,
    /// Caller already holds the reported lifetime lock
    #[arg(long)]
    external_lease: bool,
}
#[derive(clap::Args, Clone, Debug)]
pub struct GcArgs {
    /// Only collect roots whose mtime is at least this old
    #[arg(long, default_value_t = 0)]
    older_than_seconds: u64,
}

#[instrument(skip_all)]
pub async fn cmd_managed_workspace(
    ui: &mut Ui,
    command: &CommandHelper,
    sub: &ManagedWorkspaceCommand,
) -> Result<(), CommandError> {
    match sub {
        ManagedWorkspaceCommand::Create(args) => create(ui, command, args).await,
        ManagedWorkspaceCommand::Resolve(args) => resolve(ui, command, args).await,
        ManagedWorkspaceCommand::Attach(args) => attach(ui, command, args).await,
        ManagedWorkspaceCommand::Gc(args) => gc(ui, command, args).await,
    }
}

fn print_record(ui: &mut Ui, repo_path: &Path, id: &str) -> Result<(), CommandError> {
    let root = root_path(repo_path, id);
    writeln!(
        ui.stdout(),
        "{}",
        json!({
            "id": format!("ws-{id}"), "root": root, "lock": lock_path(repo_path, id),
            "materialized": root.exists()
        })
    )?;
    Ok(())
}

async fn resolve(ui: &mut Ui, command: &CommandHelper, args: &IdArgs) -> Result<(), CommandError> {
    let helper = command.workspace_helper(ui).await?;
    let id = resolve_id(helper.repo_path(), &args.id)?;
    print_record(ui, helper.repo_path(), &id)
}

async fn create(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &CreateArgs,
) -> Result<(), CommandError> {
    let mut helper = command.workspace_helper(ui).await?;
    let parent_id = helper
        .resolve_single_rev(ui, &args.revision)
        .await?
        .id()
        .clone();
    let id = allocate(helper.repo_path())?;
    let name = workspace_name(&id);
    let parent = helper.repo().store().get_commit(&parent_id)?;
    let mut tx = helper.start_transaction();
    let tree = merge_commit_trees(tx.repo(), &[parent]).await?;
    let commit = tx
        .repo_mut()
        .new_commit(vec![parent_id], tree)
        .write()
        .await?;
    tx.repo_mut().edit(name.clone(), &commit).await?;
    tx.finish(ui, format!("create managed workspace {}", name.as_symbol()))
        .await?;
    attach_id(ui, command, &id, false).await?;
    let helper = command.workspace_helper(ui).await?;
    print_record(ui, helper.repo_path(), &id)
}

async fn attach(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &AttachArgs,
) -> Result<(), CommandError> {
    let helper = command.workspace_helper(ui).await?;
    let id = resolve_id(helper.repo_path(), &args.id)?;
    drop(helper);
    attach_id(ui, command, &id, args.external_lease).await?;
    let helper = command.workspace_helper(ui).await?;
    print_record(ui, helper.repo_path(), &id)
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BcachefsSubvolumeArgs {
    flags: u32,
    dirfd: u32,
    mode: u16,
    pad: [u16; 3],
    dst_ptr: u64,
    src_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BcachefsErrorMessage {
    msg_ptr: u64,
    msg_len: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BcachefsSubvolumeArgsV2 {
    flags: u32,
    dirfd: u32,
    mode: u16,
    pad: [u16; 3],
    dst_ptr: u64,
    src_ptr: u64,
    error: BcachefsErrorMessage,
}

const SUBVOLUME_CREATE: Opcode = rustix::ioctl::opcode::write::<BcachefsSubvolumeArgs>(0xbc, 16);
const SUBVOLUME_CREATE_V2: Opcode =
    rustix::ioctl::opcode::write::<BcachefsSubvolumeArgsV2>(0xbc, 29);
const SUBVOLUME_DELETE: Opcode = rustix::ioctl::opcode::write::<BcachefsSubvolumeArgs>(0xbc, 17);
const SUBVOLUME_DELETE_V2: Opcode =
    rustix::ioctl::opcode::write::<BcachefsSubvolumeArgsV2>(0xbc, 30);

/// Minimal implementation of the bcachefs subvolume UAPI. Keep this local:
/// linking bcachefs-tools would also pull in its much larger libbcachefs C
/// implementation, while managed workspaces need only these two ioctls.
fn bcachefs_subvolume<const V2: Opcode, const V1: Opcode>(path: &Path) -> Result<(), CommandError> {
    let parent = path.parent().unwrap();
    let filesystem = File::open(parent).context(parent)?;
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        user_error(format!(
            "bcachefs subvolume path contains a NUL byte: {}",
            path.display()
        ))
    })?;
    let base = BcachefsSubvolumeArgs {
        dirfd: libc::AT_FDCWD as u32,
        mode: 0o777,
        dst_ptr: path.as_ptr() as u64,
        ..Default::default()
    };
    let mut message = [0_u8; 8192];
    let v2 = BcachefsSubvolumeArgsV2 {
        flags: base.flags,
        dirfd: base.dirfd,
        mode: base.mode,
        pad: base.pad,
        dst_ptr: base.dst_ptr,
        src_ptr: base.src_ptr,
        error: BcachefsErrorMessage {
            msg_ptr: message.as_mut_ptr() as u64,
            msg_len: message.len() as u32,
            pad: 0,
        },
    };
    let result = unsafe { rustix::ioctl::ioctl(filesystem.as_fd(), Setter::<V2, _>::new(v2)) };
    match result {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::NOTTY) => {
            unsafe { rustix::ioctl::ioctl(filesystem.as_fd(), Setter::<V1, _>::new(base)) }
                .map_err(|error| user_error(format!("bcachefs operation failed: {error}")))
        }
        Err(error) => {
            let end = message
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(message.len());
            let detail = String::from_utf8_lossy(&message[..end]);
            let detail = detail.trim();
            Err(user_error(if detail.is_empty() {
                format!("bcachefs operation failed: {error}")
            } else {
                format!("bcachefs operation failed: {error}: {detail}")
            }))
        }
    }
}

fn create_subvolume(path: &Path) -> Result<(), CommandError> {
    fs::create_dir_all(path.parent().unwrap()).context(path.parent().unwrap())?;
    bcachefs_subvolume::<SUBVOLUME_CREATE_V2, SUBVOLUME_CREATE>(path)
}
fn delete_subvolume(path: &Path) -> Result<(), CommandError> {
    bcachefs_subvolume::<SUBVOLUME_DELETE_V2, SUBVOLUME_DELETE>(path)
}

fn run_git(
    settings: &jj_lib::settings::UserSettings,
    repo: &dyn jj_lib::repo::Repo,
    args: &[&std::ffi::OsStr],
) -> Result<(), CommandError> {
    let backend = jj_lib::git::get_git_backend(repo.store())?;
    let git_settings = jj_lib::git::GitSettings::from_settings(settings)?;
    let output = Command::new(&git_settings.executable_path)
        .args(["-c", "core.fsmonitor=false"])
        .arg("--git-dir")
        .arg(backend.git_repo_path())
        .args(args)
        .output()
        .map_err(|err| user_error(format!("Could not execute git: {err}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(user_error(format!(
            "Git worktree operation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn create_git_worktree(
    helper: &WorkspaceCommandHelper,
    name: &WorkspaceName,
    root: &Path,
) -> Result<bool, CommandError> {
    if jj_lib::git::get_git_backend(helper.repo().store()).is_err() {
        return Ok(false);
    }
    let wc = helper.repo().view().get_wc_commit_id(name).unwrap();
    let commit = helper.repo().store().get_commit(wc)?;
    let base = commit
        .parent_ids()
        .iter()
        .find(|id| *id != helper.repo().store().root_commit_id())
        .ok_or_else(|| {
            user_error("Cannot create Git worktree: managed revision has no Git commit")
        })?;
    run_git(
        helper.settings(),
        helper.repo().as_ref(),
        &[
            "worktree".as_ref(),
            "add".as_ref(),
            "--detach".as_ref(),
            "--no-checkout".as_ref(),
            root.as_os_str(),
            base.hex().as_ref(),
        ],
    )?;
    Ok(true)
}

async fn attach_id(
    ui: &mut Ui,
    command: &CommandHelper,
    id: &str,
    external_lease: bool,
) -> Result<(), CommandError> {
    let mut helper = command.workspace_helper(ui).await?;
    let repo_path = helper.repo_path().to_owned();
    // Lifetime leases only inhibit GC and are intentionally shared. Registry
    // serialization separately prevents concurrent materializers from
    // rebuilding the same absent checkout.
    let (_materialization_lock, _) = RegistryLock::load(&repo_path)?;
    let name = workspace_name(id);
    let target_id = helper
        .repo()
        .view()
        .get_wc_commit_id(&name)
        .cloned()
        .ok_or_else(|| {
            user_error(format!(
                "Managed workspace ws-{id} has no working-copy commit"
            ))
        })?;
    let root = root_path(&repo_path, id);
    let _lease = if external_lease {
        None
    } else {
        Lease::acquire(&lock_path(&repo_path, id), LeaseMode::Shared, true)?
    };
    let store = SimpleWorkspaceStore::load(&repo_path)?;
    if root.exists() {
        let valid = command
            .load_workspace_at(&root, helper.settings())
            .is_ok_and(|workspace| workspace.workspace_name() == &*name);
        if valid {
            store.add(&name, &root)?;
            return Ok(());
        }
        // A present but incomplete root is a failed materialization. Only
        // bcachefs may remove it; never fall back to recursive deletion.
        delete_subvolume(&root)?;
        #[cfg(feature = "git")]
        if jj_lib::git::get_git_backend(helper.repo().store()).is_ok() {
            run_git(
                helper.settings(),
                helper.repo().as_ref(),
                &["worktree".as_ref(), "prune".as_ref()],
            )?;
        }
    }
    create_subvolume(&root)?;
    let result = async {
        #[cfg(feature = "git")]
        let has_git = create_git_worktree(&helper, &name, &root)?;
        #[cfg(not(feature = "git"))]
        let has_git = false;
        let workspace = Workspace::attach_workspace_with_existing_repo(
            &root,
            &repo_path,
            helper.repo(),
            command.get_working_copy_factory()?,
            name.clone(),
        )?;
        if has_git {
            let path = root.join(".jj/.gitignore");
            fs::write(&path, "/*\n").context(&path)?;
        }
        let checkout = workspace
            .repo_loader()
            .load_at(helper.repo().operation())
            .await?
            .store()
            .get_commit(&target_id)?;
        let mut locked = workspace.start_working_copy_mutation_owned().await?;
        locked.check_out(&checkout).await.map_err(|err| {
            internal_error_with_message("Failed to check out managed workspace", err)
        })?;
        let tx = helper.start_transaction().into_inner();
        let repo = tx
            .commit(format!("attach managed workspace {}", name.as_symbol()))
            .await?;
        locked.finish(repo.op_id().clone()).await?;
        Ok::<(), CommandError>(())
    }
    .await;
    if result.is_err() {
        drop(store.forget(&[&name]));
        if root.exists() {
            drop(delete_subvolume(&root));
        }
    }
    result
}

async fn gc(ui: &mut Ui, command: &CommandHelper, args: &GcArgs) -> Result<(), CommandError> {
    // workspace_helper snapshots all live workspaces before we inspect leases.
    let helper = command.workspace_helper(ui).await?;
    let repo_path = helper.repo_path().to_owned();
    let store = SimpleWorkspaceStore::load(&repo_path)?;
    let candidates = helper
        .repo()
        .view()
        .wc_commit_ids()
        .keys()
        .filter_map(|name| {
            let id = name.as_str().strip_prefix("ws-")?;
            is_managed_name(&repo_path, name).then_some((name.clone(), id.to_owned()))
        })
        .map(|(name, id)| {
            let path = store
                .get_workspace_path(&name)?
                .map(|path| repo_path.join(path));
            Ok((name, id, path))
        })
        .collect::<Result<Vec<_>, CommandError>>()?;
    let mut removed = Vec::new();
    let mut removed_git = false;
    for (name, id, root) in candidates {
        let Some(root) = root else {
            continue;
        };
        let expected_root = root_path(&repo_path, &id);
        let (Ok(root), Ok(expected_root)) = (
            dunce::canonicalize(root),
            dunce::canonicalize(expected_root),
        ) else {
            continue;
        };
        if root != expected_root {
            continue;
        }
        let lease_path = lock_path(&repo_path, &id);
        let Some(_lease) = Lease::acquire(&lease_path, LeaseMode::TryExclusive, false)? else {
            continue;
        };
        // The persistent lockfile is the last-use heartbeat. Read it only
        // after taking the lease so a just-released Rho process cannot race a
        // stale timestamp checked before acquisition.
        let age = lease_path
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map_or(0, |d| d.as_secs());
        if age < args.older_than_seconds {
            continue;
        }
        // The command itself is an implicit lease on its current checkout.
        if helper.workspace_name() == &*name {
            continue;
        }
        if root.join(".git").exists() {
            fs::remove_file(root.join(".git")).context(root.join(".git"))?;
            removed_git = true;
        }
        store.forget(&[&name])?;
        delete_subvolume(&root)?;
        removed.push(format!("ws-{id}"));
    }
    if removed_git {
        run_git(
            helper.settings(),
            helper.repo().as_ref(),
            &["worktree".as_ref(), "prune".as_ref()],
        )?;
    }
    writeln!(ui.stdout(), "{}", json!({"removed": removed}))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registry_prefixes_and_paths_are_stable() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let id1 = allocate(&repo).unwrap();
        let id2 = allocate(&repo).unwrap();
        assert!(repo.join("managed-workspaces.lock").is_file());
        let mut ambiguous_id = String::new();
        for _ in 2..=36 {
            ambiguous_id = allocate(&repo).unwrap();
        }
        let registry_text = fs::read_to_string(repo.join("managed-workspaces")).unwrap();
        assert_eq!(registry_text.lines().count(), 1);
        assert!(parse_registry(&format!("{registry_text}{id1}\n"), &repo).is_err());
        assert_eq!(id1.len(), prefix_id::LEN);
        assert_eq!(resolve_id(&repo, &format!("ws-{id1}")).unwrap(), id1);
        let unique = (1..=prefix_id::LEN)
            .find(|&n| !id2.starts_with(&id1[..n]))
            .unwrap();
        assert_eq!(
            resolve_id(&repo, &format!("ws-{}", &id1[..unique])).unwrap(),
            id1
        );
        assert_eq!(
            root_path(&repo, &id1),
            repo.parent()
                .unwrap()
                .join("managed-workspaces")
                .join(format!("ws-{id1}"))
        );
        assert_eq!(
            lock_path(&repo, &id1),
            root_path(&repo, &id1).with_extension("lock")
        );

        // An ambiguous prefix intentionally selects the first generated id
        // leading to that prefix, matching PrefixId's resolver semantics.
        assert!(ambiguous_id.starts_with(&id1[..1]));
        let (_, registry) = RegistryLock::load(&repo).unwrap();
        let domain = ManagedIdDomain(registry.seed);
        let ambiguous = &id1[..1];
        let expected = match ManagedId::from_prefix(ambiguous, registry.counter, &domain).unwrap() {
            PrefixResolution::Ambiguous { first, .. } => first.encoded(),
            other => panic!("expected ambiguous prefix, got {other:?}"),
        };
        assert_eq!(
            resolve_id(&repo, &format!("ws-{ambiguous}")).unwrap(),
            expected
        );
    }

    #[test]
    fn shared_leases_collectively_exclude_gc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.lock");
        let first = Lease::acquire(&path, LeaseMode::Shared, true)
            .unwrap()
            .unwrap();
        let second = Lease::acquire(&path, LeaseMode::Shared, true)
            .unwrap()
            .unwrap();
        assert!(
            Lease::acquire(&path, LeaseMode::TryExclusive, false)
                .unwrap()
                .is_none()
        );
        drop(first);
        drop(second);
        assert!(path.exists());
        assert!(
            Lease::acquire(&path, LeaseMode::TryExclusive, false)
                .unwrap()
                .is_some()
        );
    }
}

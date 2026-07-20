// Copyright 2020 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use clap_complete::ArgValueCandidates;
use jj_lib::file_util::IoResultExt as _;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::workspace::Workspace;
use jj_lib::workspace_store::SimpleWorkspaceStore;
use jj_lib::workspace_store::WorkspaceStore as _;
use tracing::instrument;

use super::add::create_git_worktree_with_git;
use super::add::git_worktree_base_commit;
use crate::cli_util::CommandHelper;
use crate::cli_util::WorkspaceCommandHelper;
use crate::command_error::CommandError;
use crate::command_error::internal_error_with_message;
use crate::command_error::user_error;
use crate::complete;
use crate::ui::Ui;
use crate::workspace_pool::WorkspacePool;

/// Attach a detached workspace to a pool slot
///
/// The working copy is checked out into a free slot of the workspace pool
/// (see `jj workspace pool`), creating one if necessary. A slot previously
/// freed by `jj workspace pool detach` is updated incrementally, so ignored
/// files (build caches etc.) and unchanged tracked files are reused.
///
/// To create a new workspace directly in a pool slot, use `jj workspace add
/// --pool --name <NAME>`.
///
/// If the repo is backed by Git, a Git worktree is created in the slot so
/// Git commands work inside it.
#[derive(clap::Args, Clone, Debug)]
pub struct WorkspacePoolAttachArgs {
    /// Name of the workspace to attach
    ///
    /// The workspace must already exist in the repo (e.g. created with `jj
    /// workspace add --detached` or freed with `jj workspace pool detach`)
    /// and must not be attached elsewhere.
    #[arg(add = ArgValueCandidates::new(complete::workspaces))]
    name: WorkspaceNameBuf,
}

#[instrument(skip_all)]
pub async fn cmd_workspace_pool_attach(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &WorkspacePoolAttachArgs,
) -> Result<(), CommandError> {
    attach_to_pool(ui, command, &args.name).await
}

/// Claims a free pool slot (growing the pool if needed) and attaches
/// workspace `name` to it. No-op if the workspace is already in a slot.
pub(super) async fn attach_to_pool(
    ui: &mut Ui,
    command: &CommandHelper,
    name: &WorkspaceName,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui).await?;
    let pool = WorkspacePool::new(workspace_command.repo_path());
    // Check attachment before claiming a slot so the no-op and error paths
    // don't grow the pool.
    let workspace_store = SimpleWorkspaceStore::load(workspace_command.repo_path())?;
    if let Some(attached_path) = workspace_store.get_workspace_path(name)? {
        if WorkspacePool::slot_index_of_store_path(&attached_path).is_some() {
            writeln!(ui.status(), "Nothing changed.")?;
            return Ok(());
        }
        return Err(user_error(format!(
            "Workspace {name} is already attached at \"{path}\"",
            name = name.as_symbol(),
            path = workspace_command.repo_path().join(attached_path).display()
        )));
    }
    let occupied = occupied_paths(&workspace_command)?;
    let claim = match pool.claim_free_slot(&occupied)? {
        Some(claim) => claim,
        None => {
            let slot = pool.create_slot()?;
            pool.try_claim(&slot)?
                .expect("newly created slot should be uncontended")
        }
    };
    attach_workspace(ui, command, workspace_command, name, &claim.slot).await
}

/// Paths recorded in the workspace store for live workspaces, as stored
/// (relative to the repo path). Comparisons must use these relative paths,
/// not canonicalized absolute ones: absolute paths are namespace-dependent
/// (a mount namespace may present a slot at a different absolute location).
pub(super) fn occupied_paths(
    workspace_command: &WorkspaceCommandHelper,
) -> Result<Vec<PathBuf>, CommandError> {
    let workspace_store = SimpleWorkspaceStore::load(workspace_command.repo_path())?;
    let mut occupied = vec![];
    for ws_name in workspace_command.repo().view().wc_commit_ids().keys() {
        if let Some(path) = workspace_store.get_workspace_path(ws_name)? {
            occupied.push(path);
        }
    }
    Ok(occupied)
}

/// Creates Git worktree metadata for a possibly non-empty slot directory.
///
/// Git refuses to create a worktree in a non-empty directory, so the
/// worktree is created at a temporary sibling path, its `.git` file is moved
/// into the slot, and `git worktree repair` fixes the back-pointer.
fn create_git_worktree_in_slot(
    workspace_command: &WorkspaceCommandHelper,
    name: &WorkspaceName,
    slot_path: &Path,
) -> Result<(), CommandError> {
    let repo = workspace_command.repo().as_ref();
    let settings = workspace_command.settings();
    let base_commit_id = git_worktree_base_commit(repo, name)?;
    let is_empty = fs::read_dir(slot_path).context(slot_path)?.next().is_none();
    if is_empty {
        create_git_worktree_with_git(settings, repo, slot_path, &base_commit_id, false)?;
        return Ok(());
    }
    let temp_path = slot_path.with_extension("wt-tmp");
    if temp_path.exists() {
        fs::remove_dir_all(&temp_path).context(&temp_path)?;
    }
    create_git_worktree_with_git(settings, repo, &temp_path, &base_commit_id, false)?;
    fs::rename(temp_path.join(".git"), slot_path.join(".git")).context(slot_path)?;
    fs::remove_dir_all(&temp_path).context(&temp_path)?;
    super::add::run_git_command(
        settings,
        repo,
        &["worktree".as_ref(), "repair".as_ref(), slot_path.as_ref()],
    )?;
    Ok(())
}

/// Attaches workspace `name` at the pool slot `destination_path`. The slot
/// must not host another live workspace. `workspace_command` must be a fresh
/// helper (its prelude snapshot guarantees freed slots' on-disk state matches
/// their recorded tree state).
async fn attach_workspace(
    ui: &mut Ui,
    command: &CommandHelper,
    mut workspace_command: WorkspaceCommandHelper,
    name: &WorkspaceName,
    slot: &crate::workspace_pool::Slot,
) -> Result<(), CommandError> {
    let destination_path = &slot.path;
    let Some(target_commit_id) = workspace_command
        .repo()
        .view()
        .get_wc_commit_id(name)
        .cloned()
    else {
        return Err(user_error(format!(
            "No such workspace: {}",
            name.as_symbol()
        )));
    };
    let target_commit = workspace_command
        .repo()
        .store()
        .get_commit(&target_commit_id)?;

    let workspace_store = SimpleWorkspaceStore::load(workspace_command.repo_path())?;

    // Create Git worktree metadata so Git commands work inside the slot. The
    // worktree is deleted again on detach; the pool preserves build caches,
    // not Git state, and worktree creation is cheap.
    #[cfg(feature = "git")]
    let git = if jj_lib::git::get_git_backend(workspace_command.repo().store()).is_ok() {
        if !destination_path.join(".git").exists() {
            create_git_worktree_in_slot(&workspace_command, name, destination_path)?;
        }
        true
    } else {
        false
    };
    #[cfg(not(feature = "git"))]
    let git = false;

    let has_leftover_state = destination_path.join(".jj").exists();
    let (workspace, mut locked_wc, checkout_commit) = if has_leftover_state {
        // The slot was freed by a detach and still carries the working copy
        // state of its previous occupant. Rename the state and update the
        // working copy incrementally against it.
        let workspace =
            command.load_workspace_at(destination_path, workspace_command.settings())?;
        // The working copy compares and applies trees through its own store
        // instance, so load the commits through the workspace's own loader.
        let workspace_repo = workspace
            .repo_loader()
            .load_at(workspace_command.repo().operation())
            .await?;
        let checkout_commit = workspace_repo.store().get_commit(&target_commit_id)?;
        let mut locked_wc = workspace.start_working_copy_mutation_owned().await?;
        locked_wc.rename_workspace(name.to_owned());
        (workspace, locked_wc, checkout_commit)
    } else {
        let repo = workspace_command.repo().clone();
        // Registers the workspace in the store as a side effect.
        let workspace = Workspace::attach_workspace_with_existing_repo(
            destination_path,
            workspace_command.repo_path(),
            &repo,
            command.get_working_copy_factory()?,
            name.to_owned(),
        )?;
        let locked_wc = workspace.start_working_copy_mutation_owned().await?;
        (workspace, locked_wc, target_commit.clone())
    };
    if git {
        let gitignore_path: PathBuf = destination_path.join(".jj").join(".gitignore");
        fs::write(&gitignore_path, "/*\n").context(&gitignore_path)?;
    }

    let stats = locked_wc
        .check_out(&checkout_commit)
        .await
        .map_err(|err| internal_error_with_message("Failed to check out the workspace", err))?;

    let mut tx = workspace_command.start_transaction().into_inner();
    #[cfg(feature = "git")]
    if let Some(git_repo) = crate::git_util::open_workspace_git_repo(&workspace, tx.repo())? {
        // Reset from the worktree's ACTUAL HEAD, not the view's recorded
        // git_head for the workspace: the workspace is arriving at a fresh
        // worktree whose HEAD points at the worktree base commit. Comparing
        // against the recorded git_head could skip the update, and the next
        // command would import the stale HEAD as a user-made `git checkout`,
        // moving the workspace to a new change.
        let actual_head_target = match git_repo.head_id() {
            Ok(id) => jj_lib::op_store::RefTarget::normal(jj_lib::backend::CommitId::from_bytes(
                id.as_bytes(),
            )),
            // Unborn HEAD.
            Err(_) => jj_lib::op_store::RefTarget::absent(),
        };
        jj_lib::git::reset_head_for_workspace_from_old_target(
            tx.repo_mut(),
            name,
            &git_repo,
            &target_commit,
            actual_head_target,
        )
        .await?;
    }
    if has_leftover_state {
        workspace_store.add(name, workspace.workspace_root())?;
    }
    let repo = tx
        .commit(format!(
            "attach workspace {name} at \"{path}\"",
            name = name.as_symbol(),
            path = workspace.workspace_root().display()
        ))
        .await?;
    locked_wc.finish(repo.op_id().clone()).await?;

    writeln!(
        ui.status(),
        "Attached workspace {name} in \"{path}\"",
        name = name.as_symbol(),
        path = workspace.workspace_root().display()
    )?;
    if stats.added_files > 0 || stats.updated_files > 0 || stats.removed_files > 0 {
        writeln!(
            ui.status(),
            "Added {} files, modified {} files, removed {} files",
            stats.added_files,
            stats.updated_files,
            stats.removed_files
        )?;
    }
    Ok(())
}

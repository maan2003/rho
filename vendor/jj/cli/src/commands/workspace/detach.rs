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
use std::io;

use clap_complete::ArgValueCandidates;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::workspace_store::SimpleWorkspaceStore;
use jj_lib::workspace_store::WorkspaceStore as _;
use tracing::instrument;

use super::add::run_git_command;
use crate::cli_util::CommandHelper;
use crate::command_error::CommandError;
use crate::command_error::user_error;
use crate::complete;
use crate::ui::Ui;
use crate::workspace_pool::WorkspacePool;

/// Detach workspaces from their pool slots
///
/// The workspaces keep tracking their working-copy commits in the repo view
/// (they follow rebases like any other workspace); only their slots are
/// released. A freed slot keeps its build caches and is reused by the next
/// `jj workspace pool attach`; its Git worktree metadata is deleted (a new
/// worktree is created on attach).
///
/// The working copies are snapshotted before detaching, so no changes are
/// lost. Do not modify files in a freed slot: the next attach will not
/// notice such changes and they would leak into the attached workspace's
/// commit.
///
/// Already-detached and unknown workspaces are skipped with a note, so a
/// batch detach of every possibly-attached workspace is cheap: one
/// invocation, one snapshot.
///
/// Only workspaces attached to a pool slot can be detached.
#[derive(clap::Args, Clone, Debug)]
pub struct WorkspacePoolDetachArgs {
    /// Names of the workspaces to detach
    #[arg(required_unless_present = "all", add = ArgValueCandidates::new(complete::workspaces))]
    names: Vec<WorkspaceNameBuf>,

    /// Detach every workspace currently attached to a pool slot
    #[arg(long, conflicts_with = "names")]
    all: bool,
}

#[instrument(skip_all)]
pub async fn cmd_workspace_pool_detach(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &WorkspacePoolDetachArgs,
) -> Result<(), CommandError> {
    // The prelude snapshot of all workspaces guarantees the freed slots'
    // on-disk state matches the workspaces' tracked commits.
    let workspace_command = command.workspace_helper(ui).await?;
    let workspace_store = SimpleWorkspaceStore::load(workspace_command.repo_path())?;
    let names: Vec<WorkspaceNameBuf> = if args.all {
        // Everything attached to a pool slot, except the workspace this
        // command runs in (which cannot release its own directory).
        let mut names = vec![];
        for ws_name in workspace_command.repo().view().wc_commit_ids().keys() {
            if ws_name == workspace_command.workspace_name() {
                continue;
            }
            if let Some(path) = workspace_store.get_workspace_path(ws_name)?
                && WorkspacePool::slot_index_of_store_path(&path).is_some()
            {
                names.push(ws_name.to_owned());
            }
        }
        names
    } else {
        args.names.clone()
    };
    for name in &names {
        if &**name == workspace_command.workspace_name() {
            return Err(
                user_error("Cannot detach the workspace this command is running in")
                    .hinted("Run this from another workspace, e.g. the default one."),
            );
        }
    }
    let mut removed_worktree = false;
    for name in &names {
        let name = &**name;
        if workspace_command
            .repo()
            .view()
            .get_wc_commit_id(name)
            .is_none()
        {
            writeln!(
                ui.warning_default(),
                "No such workspace: {name}",
                name = name.as_symbol()
            )?;
            continue;
        }
        let Some(path) = workspace_store.get_workspace_path(name)? else {
            writeln!(
                ui.status(),
                "Workspace {name} is already detached.",
                name = name.as_symbol()
            )?;
            continue;
        };
        // Compare the stored relative path, not canonicalized absolute
        // paths: absolute paths are namespace-dependent (a mount namespace
        // may present a slot at a different absolute location).
        if WorkspacePool::slot_index_of_store_path(&path).is_none() {
            return Err(user_error(format!(
                "Workspace {name} is attached at \"{path}\", which is not a pool slot",
                name = name.as_symbol(),
                path = workspace_command.repo_path().join(&path).display()
            ))
            .hinted("Only workspaces attached with `jj workspace pool attach` can be detached."));
        }
        let path = workspace_command.repo_path().join(path);
        workspace_store.forget(&[name])?;
        // The pool preserves build caches, not Git state: drop the worktree
        // metadata (recreated cheaply on the next attach). The `.git` file
        // is removed first so `git worktree prune` considers the worktree
        // gone.
        match fs::remove_file(path.join(".git")) {
            Ok(()) => removed_worktree = true,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(user_error(format!(
                    "Failed to remove \"{path}\": {err}",
                    path = path.join(".git").display()
                )));
            }
        }
        writeln!(
            ui.status(),
            "Detached workspace {name} from \"{path}\"; the slot is now free",
            name = name.as_symbol(),
            path = path.display()
        )?;
    }
    if removed_worktree {
        run_git_command(
            workspace_command.settings(),
            workspace_command.repo().as_ref(),
            &["worktree".as_ref(), "prune".as_ref()],
        )?;
    }
    Ok(())
}

// Copyright 2026 The Jujutsu Authors
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

use std::io::Write as _;

use clap_complete::ArgValueCandidates;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::workspace_store::SimpleWorkspaceStore;
use jj_lib::workspace_store::WorkspaceStore as _;
use tracing::instrument;

use super::attach::WorkspacePoolAttachArgs;
use super::attach::cmd_workspace_pool_attach;
use super::attach::occupied_paths;
use super::detach::WorkspacePoolDetachArgs;
use super::detach::cmd_workspace_pool_detach;
use crate::cli_util::CommandHelper;
use crate::command_error::CommandError;
use crate::command_error::user_error;
use crate::complete;
use crate::ui::Ui;
use crate::workspace_pool::WorkspacePool;
use crate::workspace_pool::seed_ignored_files;

/// Manage the pool of workspace checkout directories
///
/// Slots are anonymous numbered directories under `.jj/ws-pool/`. A slot is
/// bound to a workspace by `jj workspace pool attach` and freed again by `jj
/// workspace pool detach`, keeping its build caches for the next occupant.
#[derive(clap::Subcommand, Clone, Debug)]
pub enum WorkspacePoolCommand {
    Attach(WorkspacePoolAttachArgs),
    Detach(WorkspacePoolDetachArgs),
    List(WorkspacePoolListArgs),
    Prepare(WorkspacePoolPrepareArgs),
}

/// List pool slots and their occupancy
#[derive(clap::Args, Clone, Debug)]
pub struct WorkspacePoolListArgs {}

/// Warm up slots before they are needed
///
/// Ensures the pool has at least `--count` free slots, allocating new slot
/// directories as needed and seeding each with the ignored files (build
/// caches etc.) of an existing workspace, reflinking file contents when the
/// filesystem supports it. A later `jj workspace pool attach` then starts
/// with warm caches. Existing free slots (which keep their previous
/// occupant's caches) count toward the target, so this is safe to run
/// repeatedly.
#[derive(clap::Args, Clone, Debug)]
pub struct WorkspacePoolPrepareArgs {
    /// Number of free slots to ensure
    #[arg(long, default_value = "1")]
    count: usize,

    /// Workspace whose ignored files seed new slots
    ///
    /// Defaults to the current workspace.
    #[arg(long, add = ArgValueCandidates::new(complete::workspaces))]
    seed_from: Option<WorkspaceNameBuf>,
}

#[instrument(skip_all)]
pub async fn cmd_workspace_pool(
    ui: &mut Ui,
    command: &CommandHelper,
    subcommand: &WorkspacePoolCommand,
) -> Result<(), CommandError> {
    match subcommand {
        WorkspacePoolCommand::Attach(args) => cmd_workspace_pool_attach(ui, command, args).await,
        WorkspacePoolCommand::Detach(args) => cmd_workspace_pool_detach(ui, command, args).await,
        WorkspacePoolCommand::List(args) => cmd_workspace_pool_list(ui, command, args).await,
        WorkspacePoolCommand::Prepare(args) => cmd_workspace_pool_prepare(ui, command, args).await,
    }
}

async fn cmd_workspace_pool_list(
    ui: &mut Ui,
    command: &CommandHelper,
    _args: &WorkspacePoolListArgs,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui).await?;
    let pool = WorkspacePool::new(workspace_command.repo_path());
    let workspace_store = SimpleWorkspaceStore::load(workspace_command.repo_path())?;
    // Occupancy compares the store's relative paths (absolute paths are
    // namespace-dependent).
    let mut occupants = vec![];
    for ws_name in workspace_command.repo().view().wc_commit_ids().keys() {
        if let Some(path) = workspace_store.get_workspace_path(ws_name)? {
            occupants.push((path, ws_name.to_owned()));
        }
    }
    for slot in pool.slots()? {
        let store_path = WorkspacePool::store_path(slot.index);
        let status = match occupants.iter().find(|(path, _)| *path == store_path) {
            Some((_, name)) => format!("workspace {}", name.as_symbol()),
            None => "free".to_owned(),
        };
        writeln!(ui.stdout(), "{}: {}", slot.path.display(), status)?;
    }
    Ok(())
}

async fn cmd_workspace_pool_prepare(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &WorkspacePoolPrepareArgs,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui).await?;
    let pool = WorkspacePool::new(workspace_command.repo_path());

    let donor_root = match &args.seed_from {
        Some(name) => {
            let workspace_store = SimpleWorkspaceStore::load(workspace_command.repo_path())?;
            let Some(path) = workspace_store.get_workspace_path(name)? else {
                return Err(user_error(format!(
                    "Workspace {name} is not attached to any directory",
                    name = name.as_symbol()
                )));
            };
            workspace_command.repo_path().join(path)
        }
        None => workspace_command.workspace_root().to_owned(),
    };

    let occupied = occupied_paths(&workspace_command)?;
    let free = pool
        .slots()?
        .iter()
        .filter(|slot| !occupied.contains(&WorkspacePool::store_path(slot.index)))
        .count();
    if free >= args.count {
        writeln!(
            ui.status(),
            "Pool already has {free} free slot(s); nothing to do."
        )?;
        return Ok(());
    }
    for _ in free..args.count {
        let slot = pool.create_slot()?;
        let _claim = pool
            .try_claim(&slot)?
            .expect("newly created slot should be uncontended");
        let stats = seed_ignored_files(&donor_root, &slot.path, workspace_command.base_ignores()?)?;
        writeln!(
            ui.status(),
            "Prepared slot \"{path}\": seeded {files} ignored files ({mb:.1} MiB, {how}) from \
             \"{donor}\"",
            path = slot.path.display(),
            files = stats.files,
            mb = stats.bytes as f64 / (1024.0 * 1024.0),
            how = if stats.reflinked {
                "reflinked"
            } else {
                "copied"
            },
            donor = donor_root.display(),
        )?;
    }
    Ok(())
}

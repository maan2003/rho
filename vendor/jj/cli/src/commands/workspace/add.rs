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
use std::process::Command;

use futures::future::try_join_all;
use itertools::Itertools as _;
use jj_lib::commit::CommitIteratorExt as _;
use jj_lib::file_util;
use jj_lib::file_util::IoResultExt as _;
use jj_lib::git;
use jj_lib::object_id::ObjectId as _;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::merge_commit_trees;
use jj_lib::workspace::Workspace;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::command_error::CommandError;
use crate::command_error::internal_error_with_message;
use crate::command_error::user_error;
use crate::command_error::user_error_with_message;
use crate::commands::git::maybe_add_gitignore;
use crate::description_util::add_trailers;
use crate::description_util::join_message_paragraphs;
use crate::ui::Ui;

/// How to handle sparse patterns when creating a new workspace.
#[derive(clap::ValueEnum, Clone, Debug, Eq, PartialEq)]
enum SparseInheritance {
    /// Copy all sparse patterns from the current workspace.
    Copy,
    /// Include all files in the new workspace.
    Full,
    /// Clear all files from the workspace (it will be empty).
    Empty,
}

/// Add a workspace
///
/// By default, the new workspace inherits the sparse patterns of the current
/// workspace. You can override this with the `--sparse-patterns` option.
#[derive(clap::Args, Clone, Debug)]
pub struct WorkspaceAddArgs {
    /// Where to create the new workspace
    #[arg(
        value_hint = clap::ValueHint::DirPath,
        required_unless_present_any = ["detached", "pool"],
        conflicts_with_all = ["detached", "pool"]
    )]
    destination: Option<String>,

    /// A name for the workspace
    ///
    /// To override the default, which is the basename of the destination
    /// directory.
    #[arg(long)]
    name: Option<WorkspaceNameBuf>,

    /// A list of parent revisions for the working-copy commit of the newly
    /// created workspace. You may specify nothing, or any number of parents.
    ///
    /// If no revisions are specified, the new workspace will be created, and
    /// its working-copy commit will exist on top of the parent(s) of the
    /// working-copy commit in the current workspace, i.e. they will share the
    /// same parent(s).
    ///
    /// If any revisions are specified, the new workspace will be created, and
    /// the new working-copy commit will be created with all these revisions as
    /// parents, i.e. the working-copy commit will exist as if you had run `jj
    /// new r1 r2 r3 ...`.
    #[arg(long = "revision", short, value_name = "REVSETS", alias = "revisions")]
    revisions: Vec<RevisionArg>,

    /// The change description to use
    #[arg(long = "message", short, value_name = "MESSAGE")]
    message_paragraphs: Vec<String>,

    /// How to handle sparse patterns when creating a new workspace.
    #[arg(long, value_enum, default_value_t = SparseInheritance::Copy)]
    sparse_patterns: SparseInheritance,

    /// Create Git worktree metadata so Git commands can run in the new
    /// workspace.
    #[arg(long)]
    git: bool,

    /// Create the workspace without a working-copy directory
    ///
    /// The workspace only exists in the repo view, tracking its working-copy
    /// commit like any other workspace. Materialize it into a directory later
    /// with `jj workspace pool attach`. Requires `--name`.
    #[arg(long, requires = "name", conflicts_with = "pool")]
    detached: bool,

    /// Create the workspace in a slot of the workspace pool
    ///
    /// Equivalent to creating a detached workspace and running `jj workspace
    /// pool attach` on it. Requires `--name`.
    #[arg(long, requires = "name")]
    pool: bool,
}

#[instrument(skip_all)]
pub async fn cmd_workspace_add(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &WorkspaceAddArgs,
) -> Result<(), CommandError> {
    if args.detached || args.pool {
        let workspace_name = args
            .name
            .clone()
            .expect("clap requires --name with --detached/--pool");
        create_detached_workspace(
            ui,
            command,
            workspace_name.clone(),
            &args.revisions,
            &args.message_paragraphs,
        )
        .await?;
        if args.pool {
            super::attach::attach_to_pool(ui, command, &workspace_name).await?;
        }
        return Ok(());
    }
    let destination = args
        .destination
        .as_ref()
        .expect("clap requires destination");
    let old_workspace_command = command.workspace_helper(ui).await?;
    let destination_path = command.cwd().join(destination);
    let workspace_name = if let Some(name) = &args.name {
        name.to_owned()
    } else {
        let file_name = destination_path.file_name().unwrap();
        file_name
            .to_str()
            .ok_or_else(|| user_error("Destination path is not valid UTF-8"))?
            .into()
    };
    if workspace_name.as_str().is_empty() {
        return Err(user_error("New workspace name cannot be empty"));
    }

    let repo = old_workspace_command.repo();
    if repo.view().get_wc_commit_id(&workspace_name).is_some() {
        return Err(user_error(format!(
            "Workspace named '{name}' already exists",
            name = workspace_name.as_symbol()
        )));
    }
    if !destination_path.exists() {
        fs::create_dir(&destination_path).context(&destination_path)?;
    } else if !file_util::is_empty_dir(&destination_path)? {
        return Err(user_error(
            "Destination path exists and is not an empty directory",
        ));
    }

    let working_copy_factory = command.get_working_copy_factory()?;
    let repo_path = old_workspace_command.repo_path();
    if args.git {
        let base_commit_id =
            git_worktree_base_commit(repo.as_ref(), old_workspace_command.workspace_name())?;
        create_git_worktree_with_git(
            command.settings(),
            repo.as_ref(),
            &destination_path,
            &base_commit_id,
            false,
        )?;
    }
    // If we add per-workspace configuration, we'll need to reload settings for
    // the new workspace.
    let (new_workspace, repo) = Workspace::init_workspace_with_existing_repo(
        &destination_path,
        repo_path,
        repo,
        working_copy_factory,
        workspace_name.clone(),
    )
    .await?;
    writeln!(
        ui.status(),
        "Created workspace in \"{}\"",
        file_util::relative_path(command.cwd(), &destination_path).display()
    )?;
    // Show a warning if the user passed a path without a separator, since they
    // may have intended the argument to only be the name for the workspace.
    if !destination.contains(std::path::is_separator) {
        writeln!(
            ui.warning_default(),
            r#"Workspace created inside current directory. If this was unintentional, delete the "{}" directory and run `jj workspace forget {name}` to remove it."#,
            destination,
            name = workspace_name.as_symbol()
        )?;
    }

    let mut new_workspace_command = command.for_workable_repo(ui, new_workspace, repo)?;
    if args.git {
        maybe_add_gitignore(&new_workspace_command)?;
    }

    let sparsity = match args.sparse_patterns {
        SparseInheritance::Full => None,
        SparseInheritance::Empty => Some(vec![]),
        SparseInheritance::Copy => {
            let sparse_patterns = old_workspace_command
                .working_copy()
                .sparse_patterns()?
                .to_vec();
            Some(sparse_patterns)
        }
    };

    if let Some(sparse_patterns) = sparsity {
        let (mut locked_ws, _wc_commit) =
            new_workspace_command.start_working_copy_mutation().await?;
        locked_ws
            .locked_wc()
            .set_sparse_patterns(sparse_patterns)
            .await
            .map_err(|err| internal_error_with_message("Failed to set sparse patterns", err))?;
        let operation_id = locked_ws.locked_wc().old_operation_id().clone();
        locked_ws.finish(operation_id).await?;
    }

    let mut tx = new_workspace_command.start_transaction();

    // If no parent revisions are specified, create a working-copy commit based
    // on the parent of the current working-copy commit.
    let parents = if args.revisions.is_empty() {
        // Check out parents of the current workspace's working-copy commit, or the
        // root if there is no working-copy commit in the current workspace.
        if let Some(old_wc_commit_id) = tx
            .base_repo()
            .view()
            .get_wc_commit_id(old_workspace_command.workspace_name())
        {
            tx.repo()
                .store()
                .get_commit_async(old_wc_commit_id)
                .await?
                .parents()
                .await?
        } else {
            vec![tx.repo().store().root_commit()]
        }
    } else {
        try_join_all(
            old_workspace_command
                .resolve_some_revsets(ui, &args.revisions)
                .await?
                .iter()
                .map(|id| tx.repo().store().get_commit_async(id)),
        )
        .await?
    };

    let tree = merge_commit_trees(tx.repo(), &parents).await?;
    let parent_ids = parents.iter().ids().cloned().collect_vec();
    let mut commit_builder = tx.repo_mut().new_commit(parent_ids, tree).detach();
    let mut description = join_message_paragraphs(&args.message_paragraphs);
    if !description.is_empty() {
        // The first trailer would become the first line of the description.
        // Also, a commit with no description is treated in a special way in jujutsu: it
        // can be discarded as soon as it's no longer the working copy. Adding a
        // trailer to an empty description would break that logic.
        commit_builder.set_description(description);
        description = add_trailers(ui, &tx, &commit_builder).await?;
    }
    commit_builder.set_description(&description);
    let new_wc_commit = commit_builder.write(tx.repo_mut()).await?;

    tx.edit(&new_wc_commit)?;
    tx.finish(
        ui,
        format!(
            "create initial working-copy commit in workspace {name}",
            name = workspace_name.as_symbol()
        ),
    )
    .await?;
    Ok(())
}

#[instrument(skip_all)]
/// Creates a detached workspace: a working-copy commit tracked in the view
/// without any directory attached to it.
pub(super) async fn create_detached_workspace(
    ui: &mut Ui,
    command: &CommandHelper,
    workspace_name: jj_lib::ref_name::WorkspaceNameBuf,
    revisions: &[crate::cli_util::RevisionArg],
    message_paragraphs: &[String],
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui).await?;
    if workspace_command
        .repo()
        .view()
        .get_wc_commit_id(&workspace_name)
        .is_some()
    {
        return Err(user_error(format!(
            "Workspace named '{name}' already exists",
            name = workspace_name.as_symbol()
        )));
    }

    // Resolve parents against the current repo before starting the
    // transaction; same semantics as the materialized path below.
    let parent_ids = if revisions.is_empty() {
        if let Some(old_wc_commit_id) = workspace_command
            .repo()
            .view()
            .get_wc_commit_id(workspace_command.workspace_name())
        {
            workspace_command
                .repo()
                .store()
                .get_commit(old_wc_commit_id)?
                .parent_ids()
                .to_vec()
        } else {
            vec![workspace_command.repo().store().root_commit_id().clone()]
        }
    } else {
        workspace_command
            .resolve_some_revsets(ui, revisions)
            .await?
            .into_iter()
            .collect_vec()
    };

    let mut tx = workspace_command.start_transaction();
    let parents: Vec<_> = parent_ids
        .iter()
        .map(|id| tx.repo().store().get_commit(id))
        .try_collect()?;
    let tree = merge_commit_trees(tx.repo(), &parents).await?;
    let mut commit_builder = tx.repo_mut().new_commit(parent_ids, tree).detach();
    let mut description = join_message_paragraphs(message_paragraphs);
    if !description.is_empty() {
        commit_builder.set_description(description);
        description = add_trailers(ui, &tx, &commit_builder).await?;
    }
    commit_builder.set_description(&description);
    let new_wc_commit = commit_builder.write(tx.repo_mut()).await?;
    tx.repo_mut()
        .edit(workspace_name.clone(), &new_wc_commit)
        .await?;
    writeln!(
        ui.status(),
        "Created detached workspace {name}",
        name = workspace_name.as_symbol()
    )?;
    tx.finish(
        ui,
        format!(
            "create initial working-copy commit in detached workspace {name}",
            name = workspace_name.as_symbol()
        ),
    )
    .await?;
    Ok(())
}

pub(super) fn git_worktree_base_commit(
    repo: &dyn jj_lib::repo::Repo,
    workspace_name: &WorkspaceName,
) -> Result<String, CommandError> {
    if let Some(commit_id) = repo
        .view()
        .git_head_for_workspace(workspace_name)
        .as_normal()
    {
        return Ok(commit_id.hex());
    }
    if let Some(wc_commit_id) = repo.view().get_wc_commit_id(workspace_name) {
        let wc_commit = repo.store().get_commit(wc_commit_id)?;
        if let Some(parent_id) = wc_commit
            .parent_ids()
            .iter()
            .find(|id| *id != repo.store().root_commit_id())
        {
            return Ok(parent_id.hex());
        }
    }
    Err(user_error(
        "Cannot create a Git worktree because there is no Git commit to initialize it from",
    ))
}

/// Runs a Git command against the repo's backing Git repository.
pub(super) fn run_git_command(
    settings: &jj_lib::settings::UserSettings,
    repo: &dyn jj_lib::repo::Repo,
    args: &[&std::ffi::OsStr],
) -> Result<(), CommandError> {
    let git_backend = git::get_git_backend(repo.store())?;
    let git_settings = git::GitSettings::from_settings(settings)?;
    let output = Command::new(&git_settings.executable_path)
        .args(["-c", "core.fsmonitor=false"])
        .arg("--git-dir")
        .arg(git_backend.git_repo_path())
        .args(args)
        .output()
        .map_err(|err| {
            user_error_with_message(
                format!(
                    "Could not execute git using '{}'",
                    git_settings.executable_path.display()
                ),
                err,
            )
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(user_error(format!(
            "Git command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim_end()
        )))
    }
}

pub(super) fn create_git_worktree_with_git(
    settings: &jj_lib::settings::UserSettings,
    repo: &dyn jj_lib::repo::Repo,
    destination_path: &Path,
    base_commit_id: &str,
    // Required when the destination already exists and is not empty (e.g. a
    // directory pre-populated with build caches).
    force: bool,
) -> Result<(), CommandError> {
    let git_backend = git::get_git_backend(repo.store())?;
    let git_settings = git::GitSettings::from_settings(settings)?;
    let output = Command::new(&git_settings.executable_path)
        .args(["-c", "core.fsmonitor=false"])
        .arg("--git-dir")
        .arg(git_backend.git_repo_path())
        .args(["worktree", "add", "--detach", "--no-checkout"])
        .args(force.then_some("--force"))
        .arg(destination_path)
        .arg(base_commit_id)
        .output()
        .map_err(|err| {
            user_error_with_message(
                format!(
                    "Could not execute git worktree add using '{}'",
                    git_settings.executable_path.display()
                ),
                err,
            )
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(user_error(format!(
            "Git failed to create a worktree: {}",
            String::from_utf8_lossy(&output.stderr).trim_end()
        ))
        .hinted("The Jujutsu workspace was not created. Resolve the Git worktree error and retry."))
    }
}

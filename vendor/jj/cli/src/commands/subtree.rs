// Copyright 2025 The Jujutsu Authors
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

use futures::AsyncReadExt as _;
use jj_lib::backend::CommitId;
use jj_lib::backend::CopyId;
use jj_lib::backend::TreeValue;
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo;
use jj_lib::repo_path::RepoPath;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::repo_path::RepoPathComponent;
use jj_lib::tree_builder::TreeBuilder;
use serde::Deserialize;
use serde::Serialize;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::command_error::CommandError;
use crate::command_error::user_error;
use crate::ui::Ui;

const METADATA_FILE: &str = ".jjsubtree.toml";
const METADATA_VERSION: u32 = 1;

/// Manage imported directory trees
#[derive(clap::Subcommand, Clone, Debug)]
pub(crate) enum SubtreeCommand {
    /// Add the root tree of another revision at a new path
    Add(SubtreeAddArgs),
    /// List managed subtrees in the working-copy commit
    List(SubtreeListArgs),
    /// Merge a newer source revision into a managed subtree
    Update(SubtreeUpdateArgs),
}

#[derive(clap::Args, Clone, Debug)]
pub(crate) struct SubtreeAddArgs {
    /// Destination directory for the imported tree
    #[arg(value_name = "DESTINATION", value_hint = clap::ValueHint::DirPath)]
    destination: String,

    /// Revision whose root tree should be imported
    #[arg(long, short, value_name = "REV")]
    source: RevisionArg,
}

#[derive(clap::Args, Clone, Debug)]
pub(crate) struct SubtreeUpdateArgs {
    /// Managed subtree directory to update
    #[arg(value_name = "DESTINATION", value_hint = clap::ValueHint::DirPath)]
    destination: String,

    /// New source revision to merge into the subtree
    #[arg(long, short, value_name = "REV")]
    source: RevisionArg,
}

#[derive(clap::Args, Clone, Debug)]
pub(crate) struct SubtreeListArgs {}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SubtreeMetadata {
    version: u32,
    source_commit: String,
}

pub(crate) async fn cmd_subtree(
    ui: &mut Ui,
    command: &CommandHelper,
    subcommand: &SubtreeCommand,
) -> Result<(), CommandError> {
    match subcommand {
        SubtreeCommand::Add(args) => cmd_subtree_add(ui, command, args).await,
        SubtreeCommand::List(args) => cmd_subtree_list(ui, command, args).await,
        SubtreeCommand::Update(args) => cmd_subtree_update(ui, command, args).await,
    }
}

#[instrument(skip_all)]
async fn cmd_subtree_add(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &SubtreeAddArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui).await?;
    let destination = workspace_command.parse_file_path(&args.destination)?;
    validate_destination(&destination)?;
    let source = workspace_command
        .resolve_single_rev(ui, &args.source)
        .await?;
    let target = workspace_command
        .resolve_single_rev(ui, &RevisionArg::AT)
        .await?;
    workspace_command.check_rewritable([target.id()]).await?;
    require_source_not_descendant(workspace_command.repo().as_ref(), &target, &source)?;

    require_resolved_source(&source.tree())?;
    validate_reserved_components(&destination)?;
    validate_destination_ancestors(&target.tree(), &destination).await?;
    if !target.tree().path_value(&destination).await?.is_absent() {
        return Err(user_error(format!(
            "Subtree destination '{}' already exists",
            workspace_command.format_file_path(&destination)
        )));
    }
    validate_source_tree(&source.tree()).await?;

    let metadata_path = metadata_path(&destination);
    let metadata = SubtreeMetadata {
        version: METADATA_VERSION,
        source_commit: source.id().hex(),
    };
    let mounted_tree = mount_tree(&source.tree(), &destination).await?;
    let new_tree = MergedTree::merge(Merge::from_vec(vec![
        (target.tree(), "current repository".to_owned()),
        (
            target.store().empty_merged_tree(),
            "empty source base".to_owned(),
        ),
        (mounted_tree, "added subtree".to_owned()),
    ]))
    .await?;
    let new_tree = with_metadata(&new_tree, &metadata_path, &metadata).await?;

    let destination_display = workspace_command.format_file_path(&destination).to_string();
    let mut tx = workspace_command.start_transaction();
    tx.repo_mut()
        .rewrite_commit(&target)
        .set_tree(new_tree)
        .write()
        .await?;
    tx.finish(
        ui,
        format!(
            "add subtree {} from {}",
            destination_display,
            source.id().hex()
        ),
    )
    .await
}

#[instrument(skip_all)]
async fn cmd_subtree_update(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &SubtreeUpdateArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui).await?;
    let destination = workspace_command.parse_file_path(&args.destination)?;
    validate_destination(&destination)?;
    let new_source = workspace_command
        .resolve_single_rev(ui, &args.source)
        .await?;
    let target = workspace_command
        .resolve_single_rev(ui, &RevisionArg::AT)
        .await?;
    workspace_command.check_rewritable([target.id()]).await?;
    require_source_not_descendant(workspace_command.repo().as_ref(), &target, &new_source)?;

    require_resolved_source(&new_source.tree())?;
    validate_reserved_components(&destination)?;
    validate_source_tree(&new_source.tree()).await?;
    let metadata_path = metadata_path(&destination);
    let metadata = read_metadata(&target.tree(), &metadata_path).await?;
    let old_source_id = validate_metadata(&metadata, target.store().commit_id_length())?;
    if old_source_id == *new_source.id() {
        writeln!(
            ui.status(),
            "Subtree '{}' is already at source {}",
            workspace_command.format_file_path(&destination),
            new_source.id().hex()
        )?;
        return Ok(());
    }
    let old_source = target
        .store()
        .get_commit_async(&old_source_id)
        .await
        .map_err(|_| {
            user_error(format!(
                "Previous subtree source {} is not available",
                metadata.source_commit
            ))
        })?;
    require_resolved_source(&old_source.tree())?;
    if !workspace_command
        .repo()
        .index()
        .is_ancestor(old_source.id(), new_source.id())?
    {
        writeln!(
            ui.warning_default(),
            "Source {} is not a descendant of previous source {} in the commit graph (this may be \
             expected after rewriting the source); applying the change anyway",
            new_source.id().hex(),
            old_source.id().hex()
        )?;
    }

    let old_source_tree = mount_tree(&old_source.tree(), &destination).await?;
    let new_source_tree = mount_tree(&new_source.tree(), &destination).await?;
    let merged_tree = MergedTree::merge(Merge::from_vec(vec![
        (target.tree(), "local repository".to_owned()),
        (old_source_tree, "previous source".to_owned()),
        (new_source_tree, "new source".to_owned()),
    ]))
    .await?;
    let new_metadata = SubtreeMetadata {
        version: METADATA_VERSION,
        source_commit: new_source.id().hex(),
    };
    let new_tree = with_metadata(&merged_tree, &metadata_path, &new_metadata).await?;

    let destination_display = workspace_command.format_file_path(&destination).to_string();
    let mut tx = workspace_command.start_transaction();
    tx.repo_mut()
        .rewrite_commit(&target)
        .set_tree(new_tree)
        .write()
        .await?;
    tx.finish(
        ui,
        format!(
            "update subtree {} to {}",
            destination_display,
            new_source.id().hex()
        ),
    )
    .await
}

#[instrument(skip_all)]
async fn cmd_subtree_list(
    ui: &mut Ui,
    command: &CommandHelper,
    _args: &SubtreeListArgs,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui).await?;
    let target = workspace_command
        .resolve_single_rev(ui, &RevisionArg::AT)
        .await?;
    for (path, value) in target.tree().entries() {
        let Some((destination, basename)) = path.split() else {
            continue;
        };
        if basename.as_internal_str() != METADATA_FILE {
            continue;
        }
        if !value?.is_resolved() {
            writeln!(
                ui.stdout(),
                "{} <conflicted metadata>",
                workspace_command.format_file_path(destination)
            )?;
            continue;
        }
        let metadata = read_metadata(&target.tree(), &path).await?;
        validate_metadata(&metadata, target.store().commit_id_length())?;
        writeln!(
            ui.stdout(),
            "{} {}",
            workspace_command.format_file_path(destination),
            metadata.source_commit
        )?;
    }
    Ok(())
}

fn validate_destination(destination: &RepoPath) -> Result<(), CommandError> {
    if destination.is_root() {
        Err(user_error(
            "Subtree destination must not be the repository root",
        ))
    } else {
        Ok(())
    }
}

fn validate_reserved_components(path: &RepoPath) -> Result<(), CommandError> {
    if let Some(component) = path.components().find(|component| {
        let name = component.as_internal_str();
        name.eq_ignore_ascii_case(".jj") || name.eq_ignore_ascii_case(".git")
    }) {
        Err(user_error(format!(
            "Subtree path contains reserved component '{}'",
            component.as_internal_str()
        )))
    } else {
        Ok(())
    }
}

async fn validate_destination_ancestors(
    tree: &MergedTree,
    destination: &RepoPath,
) -> Result<(), CommandError> {
    for ancestor in destination
        .ancestors()
        .skip(1)
        .filter(|path| !path.is_root())
    {
        let value = tree
            .path_value(ancestor)
            .await?
            .into_resolved()
            .map_err(|_| {
                user_error(format!(
                    "Subtree destination has conflicted ancestor '{}'",
                    ancestor.as_internal_file_string()
                ))
            })?;
        if value.is_some_and(|value| !matches!(value, TreeValue::Tree(_))) {
            return Err(user_error(format!(
                "Subtree destination has non-directory ancestor '{}'",
                ancestor.as_internal_file_string()
            )));
        }
    }
    Ok(())
}

fn require_resolved_source(tree: &MergedTree) -> Result<(), CommandError> {
    if tree.has_conflict() {
        Err(user_error(
            "Subtree source revision must not contain conflicts",
        ))
    } else {
        Ok(())
    }
}

fn require_source_not_descendant(
    repo: &dyn Repo,
    target: &jj_lib::commit::Commit,
    source: &jj_lib::commit::Commit,
) -> Result<(), CommandError> {
    if repo.index().is_ancestor(target.id(), source.id())? {
        Err(user_error(
            "Subtree source must not be the working-copy commit or one of its descendants",
        ))
    } else {
        Ok(())
    }
}

fn metadata_path(destination: &RepoPath) -> RepoPathBuf {
    destination.join(RepoPathComponent::new(METADATA_FILE).unwrap())
}

async fn validate_source_tree(tree: &MergedTree) -> Result<(), CommandError> {
    for (path, value) in tree.entries() {
        value?;
        validate_reserved_components(&path)?;
        if path
            .split()
            .is_some_and(|(_, name)| name.as_internal_str() == METADATA_FILE)
        {
            return Err(user_error(format!(
                "Source revision contains reserved file '{}' at '{}'",
                METADATA_FILE,
                path.as_internal_file_string()
            )));
        }
    }
    Ok(())
}

async fn mount_tree(
    source: &MergedTree,
    destination: &RepoPath,
) -> Result<MergedTree, CommandError> {
    let store = source.store().clone();
    let mut builder = TreeBuilder::new(store.clone(), store.empty_tree_id().clone());
    for (source_path, value) in source.entries() {
        let value = value?.into_resolved().map_err(|_| {
            user_error(format!(
                "Subtree source is conflicted at '{}'",
                source_path.as_internal_file_string()
            ))
        })?;
        let Some(value) = value else {
            continue;
        };
        let destination_path = RepoPathBuf::from_internal_string(format!(
            "{}{}",
            destination.to_internal_dir_string(),
            source_path.as_internal_file_string()
        ))
        .unwrap();
        let value = match value {
            TreeValue::File { id, executable, .. } => {
                let id = {
                    let mut contents = store.read_file(&source_path, &id).await?;
                    store.write_file(&destination_path, &mut contents).await?
                };
                ensure_file_readable(&store, &destination_path, &id).await?;
                TreeValue::File {
                    id,
                    executable,
                    copy_id: CopyId::placeholder(),
                }
            }
            TreeValue::Symlink(id) => {
                let target = store.read_symlink(&source_path, &id).await?;
                let id = store.write_symlink(&destination_path, &target).await?;
                store.read_symlink(&destination_path, &id).await?;
                TreeValue::Symlink(id)
            }
            TreeValue::GitSubmodule(id) => TreeValue::GitSubmodule(id),
            TreeValue::Tree(_) => {
                return Err(user_error(format!(
                    "Cannot mount unresolved tree value at '{}'",
                    source_path.as_internal_file_string()
                )));
            }
        };
        builder.set(destination_path, value);
    }
    let tree_id = builder.write_tree().await?;
    Ok(MergedTree::resolved(store, tree_id))
}

async fn with_metadata(
    tree: &MergedTree,
    storage_path: &RepoPath,
    metadata: &SubtreeMetadata,
) -> Result<MergedTree, CommandError> {
    let contents = toml::to_string(metadata).map_err(|err| user_error(err.to_string()))?;
    let file_id = tree
        .store()
        .write_file(storage_path, &mut contents.as_bytes())
        .await?;
    ensure_file_readable(tree.store(), storage_path, &file_id).await?;
    let mut builder = MergedTreeBuilder::new(tree.clone());
    builder.set_or_remove(
        storage_path.to_owned(),
        Merge::resolved(Some(TreeValue::File {
            id: file_id,
            executable: false,
            copy_id: CopyId::placeholder(),
        })),
    );
    Ok(builder.write_tree().await?)
}

async fn ensure_file_readable(
    store: &jj_lib::store::Store,
    path: &RepoPath,
    id: &jj_lib::backend::FileId,
) -> Result<(), CommandError> {
    let mut contents = store.read_file(path, id).await?;
    futures::io::copy(&mut contents, &mut futures::io::sink()).await?;
    Ok(())
}

async fn read_metadata(
    tree: &MergedTree,
    path: &RepoPath,
) -> Result<SubtreeMetadata, CommandError> {
    let value = tree.path_value(path).await?.into_resolved().map_err(|_| {
        user_error(format!(
            "Subtree metadata '{}' is conflicted",
            path.as_internal_file_string()
        ))
    })?;
    let Some(TreeValue::File { id, .. }) = value else {
        return Err(user_error(format!(
            "No managed subtree metadata found at '{}'",
            path.as_internal_file_string()
        )));
    };
    let mut reader = tree.store().read_file(path, &id).await?;
    let mut contents = String::new();
    reader.read_to_string(&mut contents).await?;
    toml::from_str(&contents).map_err(|err| {
        user_error(format!(
            "Invalid subtree metadata at '{}': {err}",
            path.as_internal_file_string()
        ))
    })
}

fn validate_metadata(
    metadata: &SubtreeMetadata,
    commit_id_length: usize,
) -> Result<CommitId, CommandError> {
    if metadata.version != METADATA_VERSION {
        return Err(user_error(format!(
            "Unsupported subtree metadata version {}",
            metadata.version
        )));
    }
    parse_commit_id(&metadata.source_commit, commit_id_length)
}

fn parse_commit_id(hex: &str, expected_len: usize) -> Result<CommitId, CommandError> {
    let id = CommitId::try_from_hex(hex)
        .filter(|id| id.as_bytes().len() == expected_len)
        .ok_or_else(|| user_error(format!("Invalid subtree source commit ID '{hex}'")))?;
    Ok(id)
}

use std::collections::HashSet;

use anyhow::Context as _;
use camino::Utf8PathBuf;
use futures::{AsyncReadExt as _, StreamExt as _};
use jj_lib::backend::TreeValue;
use jj_lib::conflicts::{MaterializedTreeValue, materialize_tree_value};
use jj_lib::matchers::EverythingMatcher;
use jj_lib::merge::MergedTreeValue;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPath;
use senax_encoder::{Decode, Encode, Pack, Unpack};

/// Resource limits for one diff snapshot. The dedicated UI channel has a
/// 64-MiB frame bound; leave headroom for paths and encoding overhead.
const MAX_FILES: usize = 2_048;
const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_IO_BYTES: usize = 48 * 1024 * 1024;
/// Conservative aggregate bound for every variable-size wire field. The
/// protocol frame limit is 64 MiB; this leaves ample senax overhead.
const MAX_PAYLOAD_BYTES: usize = 40 * 1024 * 1024;
const ENTRY_OVERHEAD_BUDGET: usize = 256;
const MAX_MESSAGE_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct WorkspaceDiffSnapshot {
    /// Exact jj operation from which the manifest was materialized.
    pub operation_id: String,
    /// Immutable working-copy commit the snapshot describes.
    pub commit_id: String,
    pub files: Vec<WorkspaceDiffFile>,
    /// At least one changed path was omitted after [`MAX_FILES`].
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct WorkspaceDiffFile {
    /// Repository-relative path. A rename is represented losslessly as one
    /// deletion and one addition; copy presentation can be layered on later
    /// without changing file contents or edit semantics.
    pub path: Utf8PathBuf,
    pub status: WorkspaceDiffStatus,
    pub base: WorkspaceDiffContent,
    /// Descriptor for the snapshotted current side. Text comes from the live
    /// Zed Project buffer and is deliberately not duplicated on the wire.
    pub target: WorkspaceDiffTarget,
    pub base_executable: Option<bool>,
    pub target_executable: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum WorkspaceDiffStatus {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum WorkspaceDiffContent {
    Absent,
    Text(String),
    Binary { bytes: u64 },
    TooLarge { bytes_at_least: u64 },
    BudgetExhausted,
    Symlink(String),
    GitSubmodule(String),
    AccessDenied(String),
    OtherConflict(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum WorkspaceDiffTarget {
    Absent,
    Text { bytes: u64 },
    Binary { bytes: u64 },
    TooLarge { bytes_at_least: u64 },
    BudgetExhausted,
    Symlink(String),
    GitSubmodule(String),
    Conflict(String),
}

pub struct CapturedDiff {
    repo: std::sync::Arc<jj_lib::repo::ReadonlyRepo>,
    operation_id: jj_lib::op_store::OperationId,
    commit_id: jj_lib::backend::CommitId,
    base_tree: MergedTree,
    target_tree: MergedTree,
}

impl CapturedDiff {
    pub fn commit_id_hex(&self) -> String {
        self.commit_id.hex()
    }
}

pub async fn capture(
    epoch: jj_cli::cli_util::WorkspaceSnapshotEpoch,
) -> anyhow::Result<CapturedDiff> {
    let jj_cli::cli_util::WorkspaceSnapshotEpoch {
        repo,
        workspace_name,
        stats: _,
    } = epoch;
    let commit_id = repo
        .view()
        .get_wc_commit_id(&workspace_name)
        .cloned()
        .context("workspace has no working-copy commit")?;
    let commit = repo
        .store()
        .get_commit_async(&commit_id)
        .await
        .context("load working-copy commit")?;
    let base_tree = commit
        .parent_tree(repo.as_ref())
        .await
        .context("merge working-copy parent trees")?;
    let target_tree = commit.tree();
    Ok(CapturedDiff {
        operation_id: repo.op_id().clone(),
        repo,
        commit_id,
        base_tree,
        target_tree,
    })
}

pub async fn load(
    captured: CapturedDiff,
    include_paths: &[Utf8PathBuf],
) -> anyhow::Result<WorkspaceDiffSnapshot> {
    let CapturedDiff {
        repo: _repo,
        operation_id,
        commit_id,
        base_tree,
        target_tree,
    } = captured;
    let mut stream = base_tree.diff_stream(&target_tree, &EverythingMatcher);
    let mut files = Vec::new();
    let mut truncated = false;
    let mut io_budget = MAX_IO_BYTES;
    let mut payload_budget = MAX_PAYLOAD_BYTES
        .saturating_sub(commit_id.hex().len())
        .saturating_sub(operation_id.hex().len());
    let mut seen_paths = HashSet::new();
    while let Some(entry) = stream.next().await {
        if files.len() == MAX_FILES {
            truncated = true;
            break;
        }
        let values = entry.values.context("read tree diff")?;
        let path = repo_path_to_utf8(entry.path);
        seen_paths.insert(path.clone());
        let Some(file) = materialize_file(
            &base_tree,
            &target_tree,
            path,
            values.before,
            values.after,
            &mut io_budget,
            &mut payload_budget,
        )
        .await?
        else {
            truncated = true;
            break;
        };
        files.push(file);
    }

    let mut include_paths = include_paths.to_vec();
    include_paths.sort();
    include_paths.dedup();
    for path in include_paths {
        if seen_paths.contains(&path) {
            continue;
        }
        if files.len() == MAX_FILES {
            truncated = true;
            break;
        }
        let repo_path = RepoPath::from_internal_string(path.as_str())
            .context("included diff path is not a valid repository path")?;
        let before = base_tree
            .path_value(repo_path)
            .await
            .context("read included path from parent tree")?;
        let after = target_tree
            .path_value(repo_path)
            .await
            .context("read included path from target tree")?;
        let Some(file) = materialize_file(
            &base_tree,
            &target_tree,
            path,
            before,
            after,
            &mut io_budget,
            &mut payload_budget,
        )
        .await?
        else {
            truncated = true;
            break;
        };
        files.push(file);
    }

    Ok(WorkspaceDiffSnapshot {
        operation_id: operation_id.hex(),
        commit_id: commit_id.hex(),
        files,
        truncated,
    })
}

impl WorkspaceDiffTarget {
    fn variable_len(&self) -> usize {
        match self {
            Self::Symlink(value) | Self::GitSubmodule(value) | Self::Conflict(value) => value.len(),
            Self::Absent
            | Self::Text { .. }
            | Self::Binary { .. }
            | Self::TooLarge { .. }
            | Self::BudgetExhausted => 0,
        }
    }
}

async fn materialize_file(
    base_tree: &MergedTree,
    target_tree: &MergedTree,
    path: Utf8PathBuf,
    before: MergedTreeValue,
    after: MergedTreeValue,
    io_budget: &mut usize,
    payload_budget: &mut usize,
) -> anyhow::Result<Option<WorkspaceDiffFile>> {
    let fixed_budget = path.as_str().len().saturating_add(ENTRY_OVERHEAD_BUDGET);
    if fixed_budget > *payload_budget {
        return Ok(None);
    }
    let status = match (before.is_absent(), after.is_absent()) {
        (true, false) => WorkspaceDiffStatus::Added,
        (false, true) => WorkspaceDiffStatus::Deleted,
        _ => WorkspaceDiffStatus::Modified,
    };
    let repo_path = RepoPath::from_internal_string(path.as_str())
        .context("diff produced an invalid repository path")?;
    let mut base = materialize_content(base_tree, repo_path, before, io_budget).await?;
    let target_executable = after.as_normal().and_then(|value| match value {
        TreeValue::File { executable, .. } => Some(*executable),
        _ => None,
    });
    let target = describe_target(target_tree, repo_path, after, io_budget).await?;
    let available = *payload_budget - fixed_budget;
    let mut content_budget = base.content.variable_len() + target.variable_len();
    if content_budget > available {
        base.omit_for_budget();
        content_budget = base.content.variable_len() + target.variable_len();
    }
    if content_budget > available {
        return Ok(None);
    }
    *payload_budget -= fixed_budget + content_budget;
    Ok(Some(WorkspaceDiffFile {
        path,
        status,
        base: base.content,
        target,
        base_executable: base.executable,
        target_executable,
    }))
}

async fn describe_target(
    tree: &MergedTree,
    path: &RepoPath,
    value: MergedTreeValue,
    io_budget: &mut usize,
) -> anyhow::Result<WorkspaceDiffTarget> {
    let Some(value) = value.as_normal() else {
        return Ok(WorkspaceDiffTarget::Conflict(bounded_message(
            value.describe(tree.labels()),
        )));
    };
    Ok(match value {
        TreeValue::File { .. } => {
            if *io_budget == 0 {
                return Ok(WorkspaceDiffTarget::BudgetExhausted);
            }
            let value = materialize_tree_value(
                tree.store(),
                path,
                MergedTreeValue::resolved(Some(value.clone())),
                tree.labels(),
            )
            .await?;
            let MaterializedTreeValue::File(file) = value else {
                return Ok(WorkspaceDiffTarget::Conflict(
                    "target file could not be read".to_owned(),
                ));
            };
            let mut bytes = Vec::new();
            let read_limit = MAX_FILE_BYTES.min(*io_budget).saturating_add(1);
            file.reader
                .take(read_limit as u64)
                .read_to_end(&mut bytes)
                .await?;
            if !charge_io_budget(io_budget, bytes.len()) {
                WorkspaceDiffTarget::BudgetExhausted
            } else if bytes.len() > MAX_FILE_BYTES {
                WorkspaceDiffTarget::TooLarge {
                    bytes_at_least: bytes.len() as u64,
                }
            } else if std::str::from_utf8(&bytes).is_ok() {
                WorkspaceDiffTarget::Text {
                    bytes: bytes.len() as u64,
                }
            } else {
                WorkspaceDiffTarget::Binary {
                    bytes: bytes.len() as u64,
                }
            }
        }
        TreeValue::Symlink(id) => WorkspaceDiffTarget::Symlink(id.hex()),
        TreeValue::GitSubmodule(id) => WorkspaceDiffTarget::GitSubmodule(id.hex()),
        TreeValue::Tree(_) => WorkspaceDiffTarget::Conflict("tree replaced a file".to_owned()),
    })
}

struct MaterializedContent {
    content: WorkspaceDiffContent,
    executable: Option<bool>,
}

impl MaterializedContent {
    fn omit_for_budget(&mut self) {
        if !matches!(self.content, WorkspaceDiffContent::Absent) {
            self.content = WorkspaceDiffContent::BudgetExhausted;
        }
    }
}

impl WorkspaceDiffContent {
    fn variable_len(&self) -> usize {
        match self {
            Self::Text(value)
            | Self::Symlink(value)
            | Self::GitSubmodule(value)
            | Self::AccessDenied(value)
            | Self::OtherConflict(value) => value.len(),
            Self::Absent | Self::Binary { .. } | Self::TooLarge { .. } | Self::BudgetExhausted => 0,
        }
    }
}

async fn materialize_content(
    tree: &MergedTree,
    path: &RepoPath,
    value: MergedTreeValue,
    io_budget: &mut usize,
) -> anyhow::Result<MaterializedContent> {
    let executable = value.as_normal().and_then(|value| match value {
        TreeValue::File { executable, .. } => Some(*executable),
        _ => None,
    });
    if value.as_resolved().is_none() {
        return Ok(MaterializedContent {
            content: WorkspaceDiffContent::OtherConflict(bounded_message(
                value.describe(tree.labels()),
            )),
            executable,
        });
    }
    if *io_budget == 0 && !value.is_absent() {
        return Ok(MaterializedContent {
            content: WorkspaceDiffContent::BudgetExhausted,
            executable,
        });
    }
    let value = materialize_tree_value(tree.store(), path, value, tree.labels()).await?;
    let bytes = match value {
        MaterializedTreeValue::Absent => {
            return Ok(MaterializedContent {
                content: WorkspaceDiffContent::Absent,
                executable: None,
            });
        }
        MaterializedTreeValue::AccessDenied(error) => {
            return Ok(MaterializedContent {
                content: WorkspaceDiffContent::AccessDenied(bounded_message(error.to_string())),
                executable: None,
            });
        }
        MaterializedTreeValue::File(file) => {
            let executable = file.executable;
            let mut bytes = Vec::new();
            let read_limit = MAX_FILE_BYTES.min(*io_budget).saturating_add(1);
            let mut reader = file.reader.take(read_limit as u64);
            reader.read_to_end(&mut bytes).await?;
            if !charge_io_budget(io_budget, bytes.len()) {
                return Ok(MaterializedContent {
                    content: WorkspaceDiffContent::BudgetExhausted,
                    executable: Some(executable),
                });
            }
            if bytes.len() > MAX_FILE_BYTES {
                return Ok(MaterializedContent {
                    content: WorkspaceDiffContent::TooLarge {
                        bytes_at_least: bytes.len() as u64,
                    },
                    executable: Some(executable),
                });
            }
            (bytes, Some(executable))
        }
        MaterializedTreeValue::FileConflict(_) => unreachable!("conflicts handled before reading"),
        MaterializedTreeValue::OtherConflict { id, labels } => {
            return Ok(MaterializedContent {
                content: WorkspaceDiffContent::OtherConflict(bounded_message(id.describe(&labels))),
                executable: None,
            });
        }
        MaterializedTreeValue::Symlink { target, .. } => {
            return Ok(MaterializedContent {
                content: WorkspaceDiffContent::Symlink(bounded_message(target)),
                executable: None,
            });
        }
        MaterializedTreeValue::GitSubmodule(id) => {
            return Ok(MaterializedContent {
                content: WorkspaceDiffContent::GitSubmodule(id.hex()),
                executable: None,
            });
        }
        MaterializedTreeValue::Tree(_) => {
            return Ok(MaterializedContent {
                content: WorkspaceDiffContent::OtherConflict("tree replaced a file".to_owned()),
                executable: None,
            });
        }
    };
    let (bytes, executable) = bytes;
    let content = match String::from_utf8(bytes) {
        Ok(text) => WorkspaceDiffContent::Text(text),
        Err(error) => WorkspaceDiffContent::Binary {
            bytes: error.as_bytes().len() as u64,
        },
    };
    Ok(MaterializedContent {
        content,
        executable,
    })
}

fn charge_io_budget(io_budget: &mut usize, bytes_read: usize) -> bool {
    if bytes_read > *io_budget {
        *io_budget = 0;
        false
    } else {
        *io_budget -= bytes_read;
        true
    }
}

fn bounded_message(mut value: String) -> String {
    if value.len() <= MAX_MESSAGE_BYTES {
        return value;
    }
    let mut end = MAX_MESSAGE_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value.push('…');
    value
}

fn repo_path_to_utf8(path: jj_lib::repo_path::RepoPathBuf) -> Utf8PathBuf {
    Utf8PathBuf::from(path.as_internal_file_string())
}

#[cfg(test)]
mod tests {
    use super::{MAX_FILE_BYTES, MAX_IO_BYTES, charge_io_budget};

    #[test]
    fn oversized_file_probes_consume_the_aggregate_budget() {
        let mut budget = MAX_IO_BYTES;
        let probe = MAX_FILE_BYTES + 1;
        let successful_probes = MAX_IO_BYTES / probe;
        for _ in 0..successful_probes {
            assert!(charge_io_budget(&mut budget, probe));
        }
        assert!(!charge_io_budget(&mut budget, probe));
        assert_eq!(budget, 0);
    }
}

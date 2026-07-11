//! `apply_patch` custom tool adapted from Tau's shell extension.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use rho_core::{ApplyPatchMetadata, ToolFileChange, ToolFileStatus};

const SUMMARY_HEADER: &str = "Success. Updated the following files:";
const MAX_SAFE_FILE_READ_BYTES: u64 = 1024 * 1024;

pub(crate) const APPLY_PATCH_LARK_GRAMMAR: &str = include_str!("apply_patch.lark");

#[allow(dead_code)]
pub(crate) fn apply_patch(patch: &str, cwd: &Path) -> Result<String> {
    let resolve = |path: &Path| resolve_path(cwd, path);
    apply_patch_with_metadata(patch, &resolve).map(|(output, _)| output)
}

pub(crate) fn preview_metadata(patch: &str) -> Result<ApplyPatchMetadata> {
    let hunks = parse_patch(patch).map_err(anyhow::Error::msg)?;
    Ok(ApplyPatchMetadata {
        changes: hunks.iter().map(hunk_file_change).collect(),
    })
}

pub(crate) fn apply_patch_with_metadata(
    patch: &str,
    resolve: &dyn Fn(&Path) -> PathBuf,
) -> Result<(String, ApplyPatchMetadata)> {
    let hunks = parse_patch(patch).map_err(anyhow::Error::msg)?;
    let changes = match apply_hunks(&hunks, resolve) {
        Ok(changes) => changes,
        Err(failure) => {
            let message = if failure.changes.is_empty() {
                failure.message
            } else {
                format!(
                    "{}\n\n{}",
                    failure.message,
                    format_partial_summary(&failure.changes)
                )
            };
            return Err(anyhow::Error::msg(message));
        }
    };
    Ok((
        format_summary(&changes),
        ApplyPatchMetadata {
            changes: changes.iter().map(tool_file_change).collect(),
        },
    ))
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Hunk {
    Add {
        path: PathBuf,
        contents: String,
    },
    Delete {
        path: PathBuf,
    },
    Update {
        path: PathBuf,
        move_path: Option<PathBuf>,
        chunks: Vec<UpdateChunk>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct UpdateChunk {
    change_context: Option<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    is_end_of_file: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChangeStatus {
    Add,
    Modify,
    Delete,
}

fn tool_file_change(change: &AppliedChange) -> ToolFileChange {
    ToolFileChange {
        path: change.display_path.clone(),
        status: match change.status {
            ChangeStatus::Add => ToolFileStatus::Added,
            ChangeStatus::Modify => ToolFileStatus::Modified,
            ChangeStatus::Delete => ToolFileStatus::Deleted,
        },
    }
}

fn hunk_file_change(hunk: &Hunk) -> ToolFileChange {
    match hunk {
        Hunk::Add { path, .. } => ToolFileChange {
            path: render_path(path),
            status: ToolFileStatus::Added,
        },
        Hunk::Delete { path } => ToolFileChange {
            path: render_path(path),
            status: ToolFileStatus::Deleted,
        },
        Hunk::Update {
            path, move_path, ..
        } => ToolFileChange {
            path: move_path
                .as_ref()
                .map_or_else(|| render_path(path), |path| render_path(path)),
            status: ToolFileStatus::Modified,
        },
    }
}

impl ChangeStatus {
    fn short_name(self) -> &'static str {
        match self {
            Self::Add => "A",
            Self::Modify => "M",
            Self::Delete => "D",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AppliedChange {
    display_path: String,
    status: ChangeStatus,
}

#[derive(Debug, Eq, PartialEq)]
struct ApplyPatchFailure {
    message: String,
    changes: Vec<AppliedChange>,
}

impl ApplyPatchFailure {
    fn new(message: impl Into<String>, changes: &[AppliedChange]) -> Self {
        Self {
            message: message.into(),
            changes: changes.to_vec(),
        }
    }
}

fn apply_hunks(
    hunks: &[Hunk],
    resolve: &dyn Fn(&Path) -> PathBuf,
) -> Result<Vec<AppliedChange>, ApplyPatchFailure> {
    if hunks.is_empty() {
        return Err(ApplyPatchFailure::new("No files were modified.", &[]));
    }

    let mut changes = Vec::with_capacity(hunks.len());

    for hunk in hunks {
        match hunk {
            Hunk::Add { path, contents } => {
                let abs = resolve(path);
                if read_optional_file(&abs)
                    .map_err(|message| ApplyPatchFailure::new(message, &changes))?
                    .is_some()
                {
                    return Err(ApplyPatchFailure::new(
                        format!("Add File target already exists: {}", render_path(&abs)),
                        &changes,
                    ));
                }
                write_file_creating_parent(&abs, contents).map_err(|error| {
                    ApplyPatchFailure::new(
                        format!(
                            "Failed to write file {}: {}",
                            render_path(&abs),
                            render_diagnostic(error)
                        ),
                        &changes,
                    )
                })?;
                changes.push(AppliedChange {
                    display_path: render_path(path),
                    status: ChangeStatus::Add,
                });
            }
            Hunk::Delete { path } => {
                let abs = resolve(path);
                if abs.is_dir() {
                    return Err(ApplyPatchFailure::new(
                        format!("Failed to delete file {}", render_path(&abs)),
                        &changes,
                    ));
                }
                read_to_string_limited(&abs).map_err(|_| {
                    ApplyPatchFailure::new(
                        format!("Failed to delete file {}", render_path(&abs)),
                        &changes,
                    )
                })?;
                fs::remove_file(&abs).map_err(|_| {
                    ApplyPatchFailure::new(
                        format!("Failed to delete file {}", render_path(&abs)),
                        &changes,
                    )
                })?;
                changes.push(AppliedChange {
                    display_path: render_path(path),
                    status: ChangeStatus::Delete,
                });
            }
            Hunk::Update {
                path,
                move_path,
                chunks,
            } => {
                let abs = resolve(path);
                let old_content = read_to_string_limited(&abs).map_err(|error| {
                    ApplyPatchFailure::new(
                        format!(
                            "Failed to read file to update {}: {}",
                            render_path(&abs),
                            render_diagnostic(error)
                        ),
                        &changes,
                    )
                })?;
                let new_content = derive_new_contents_from_chunks(&abs, &old_content, chunks)
                    .map_err(|message| ApplyPatchFailure::new(message, &changes))?;

                if let Some(move_path) = move_path {
                    let dest_abs = resolve(move_path);
                    if read_optional_file(&dest_abs)
                        .map_err(|message| ApplyPatchFailure::new(message, &changes))?
                        .is_some()
                    {
                        return Err(ApplyPatchFailure::new(
                            format!(
                                "Move destination already exists: {}",
                                render_path(&dest_abs)
                            ),
                            &changes,
                        ));
                    }
                    write_file_creating_parent(&dest_abs, &new_content).map_err(|error| {
                        ApplyPatchFailure::new(
                            format!(
                                "Failed to write file {}: {}",
                                render_path(&dest_abs),
                                render_diagnostic(error)
                            ),
                            &changes,
                        )
                    })?;
                    let dest_write_change_index = changes.len();
                    changes.push(AppliedChange {
                        display_path: render_path(move_path),
                        status: ChangeStatus::Add,
                    });
                    if abs.is_dir() {
                        return Err(ApplyPatchFailure::new(
                            format!("Failed to remove original {}", render_path(&abs)),
                            &changes,
                        ));
                    }
                    fs::remove_file(&abs).map_err(|_| {
                        ApplyPatchFailure::new(
                            format!("Failed to remove original {}", render_path(&abs)),
                            &changes,
                        )
                    })?;
                    changes[dest_write_change_index] = AppliedChange {
                        display_path: render_path(move_path),
                        status: ChangeStatus::Modify,
                    };
                } else {
                    fs::write(&abs, new_content.as_bytes()).map_err(|error| {
                        ApplyPatchFailure::new(
                            format!(
                                "Failed to write file {}: {}",
                                render_path(&abs),
                                render_diagnostic(error)
                            ),
                            &changes,
                        )
                    })?;
                    changes.push(AppliedChange {
                        display_path: render_path(path),
                        status: ChangeStatus::Modify,
                    });
                };
            }
        }
    }

    Ok(changes)
}

fn format_partial_summary(changes: &[AppliedChange]) -> String {
    let mut lines = vec!["Partial changes applied before failure:".to_owned()];
    for status in [
        ChangeStatus::Add,
        ChangeStatus::Modify,
        ChangeStatus::Delete,
    ] {
        for change in changes.iter().filter(|change| change.status == status) {
            lines.push(format!(
                "{} {}",
                change.status.short_name(),
                change.display_path
            ));
        }
    }
    lines.join("\n")
}

fn format_summary(changes: &[AppliedChange]) -> String {
    let mut lines = vec![SUMMARY_HEADER.to_owned()];
    for status in [
        ChangeStatus::Add,
        ChangeStatus::Modify,
        ChangeStatus::Delete,
    ] {
        for change in changes.iter().filter(|change| change.status == status) {
            lines.push(format!(
                "{} {}",
                change.status.short_name(),
                change.display_path
            ));
        }
    }
    lines.join("\n")
}

fn render_path(path: &Path) -> String {
    escape_path_text(&path.display().to_string())
}

fn render_diagnostic(error: impl std::fmt::Display) -> String {
    escape_path_text(&error.to_string())
}

fn read_optional_file(path: &Path) -> Result<Option<String>, String> {
    match read_to_string_limited(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(render_diagnostic(error)),
    }
}

fn read_to_string_limited(path: &Path) -> std::io::Result<String> {
    let metadata = fs::metadata(path)?;
    if MAX_SAFE_FILE_READ_BYTES < metadata.len() {
        return Err(std::io::Error::other(format!(
            "file exceeds safe read limit: {} bytes > {}",
            metadata.len(),
            MAX_SAFE_FILE_READ_BYTES
        )));
    }
    fs::read_to_string(path)
}

fn write_file_creating_parent(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents.as_bytes())
}

fn resolve_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn derive_new_contents_from_chunks(
    path: &Path,
    original_contents: &str,
    chunks: &[UpdateChunk],
) -> Result<String, String> {
    let mut original_lines: Vec<String> = original_contents.split('\n').map(String::from).collect();
    if original_lines.last().is_some_and(String::is_empty) {
        original_lines.pop();
    }

    let replacements = compute_replacements(&original_lines, path, chunks)?;
    let mut new_lines = apply_replacements(original_lines, &replacements);
    if !new_lines.last().is_some_and(String::is_empty) {
        new_lines.push(String::new());
    }
    Ok(new_lines.join("\n"))
}

fn compute_replacements(
    original_lines: &[String],
    path: &Path,
    chunks: &[UpdateChunk],
) -> Result<Vec<(usize, usize, Vec<String>)>, String> {
    let mut replacements = Vec::new();
    let mut line_index = 0usize;

    for chunk in chunks {
        if let Some(ctx_line) = &chunk.change_context {
            if let Some(idx) = seek_sequence(
                original_lines,
                std::slice::from_ref(ctx_line),
                line_index,
                false,
            ) {
                line_index = idx + 1;
            } else {
                return Err(format!(
                    "Failed to find context '{}' in {}",
                    ctx_line,
                    render_path(path)
                ));
            }
        }

        if chunk.old_lines.is_empty() {
            let insertion_idx = if original_lines.last().is_some_and(String::is_empty) {
                original_lines.len().saturating_sub(1)
            } else {
                original_lines.len()
            };
            replacements.push((insertion_idx, 0, chunk.new_lines.clone()));
            continue;
        }

        let mut pattern: &[String] = &chunk.old_lines;
        let mut new_slice: &[String] = &chunk.new_lines;
        let mut found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);

        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }
            found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
        }

        if let Some(start_idx) = found {
            replacements.push((start_idx, pattern.len(), new_slice.to_vec()));
            line_index = start_idx + pattern.len();
        } else {
            return Err(format!(
                "Failed to find expected lines in {}:\n{}",
                render_path(path),
                chunk.old_lines.join("\n")
            ));
        }
    }

    replacements.sort_by_key(|(start_idx, _, _)| *start_idx);
    Ok(replacements)
}

fn apply_replacements(
    mut lines: Vec<String>,
    replacements: &[(usize, usize, Vec<String>)],
) -> Vec<String> {
    for (start_idx, old_len, new_segment) in replacements.iter().rev() {
        for _ in 0..*old_len {
            if *start_idx < lines.len() {
                lines.remove(*start_idx);
            }
        }
        for (offset, new_line) in new_segment.iter().enumerate() {
            lines.insert(*start_idx + offset, new_line.clone());
        }
    }
    lines
}

fn seek_sequence(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }

    let search_start = if eof && lines.len() >= pattern.len() {
        lines.len() - pattern.len()
    } else {
        start
    };

    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        if lines[i..i + pattern.len()] == *pattern {
            return Some(i);
        }
    }
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if lines[i + p_idx].trim_end() != pat.trim_end() {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if lines[i + p_idx].trim() != pat.trim() {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }
    None
}

fn parse_patch(patch: &str) -> Result<Vec<Hunk>, String> {
    let trimmed = patch.trim();
    let lines: Vec<&str> = trimmed.lines().collect();
    if lines.first().copied() != Some("*** Begin Patch") {
        return Err("invalid patch: missing '*** Begin Patch' header".to_owned());
    }
    if lines.last().copied() != Some("*** End Patch") {
        return Err("invalid patch: missing '*** End Patch' footer".to_owned());
    }

    let mut index = 1usize;
    let mut hunks = Vec::new();
    while index + 1 < lines.len() {
        let line = lines[index];
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            index += 1;
            let mut contents = Vec::new();
            while index + 1 < lines.len() && !lines[index].starts_with("*** ") {
                let Some(content) = lines[index].strip_prefix('+') else {
                    return Err(format!(
                        "invalid add-file line: {}",
                        escape_path_text(lines[index])
                    ));
                };
                contents.push(content.to_owned());
                index += 1;
            }
            if contents.is_empty() {
                return Err(format!(
                    "Add File hunk for {} must contain at least one line",
                    escape_path_text(path)
                ));
            }
            hunks.push(Hunk::Add {
                path: PathBuf::from(path),
                contents: contents.join("\n") + "\n",
            });
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            hunks.push(Hunk::Delete {
                path: PathBuf::from(path),
            });
            index += 1;
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Update File: ") {
            index += 1;
            let mut move_path = None;
            if index + 1 < lines.len()
                && let Some(dest) = lines[index].strip_prefix("*** Move to: ")
            {
                move_path = Some(PathBuf::from(dest));
                index += 1;
            }

            let mut chunks = Vec::new();
            while index + 1 < lines.len() && !lines[index].starts_with("*** ") {
                let header = lines[index];
                let change_context = if header == "@@" {
                    None
                } else if let Some(context) = header.strip_prefix("@@ ") {
                    Some(context.to_owned())
                } else {
                    return Err(format!(
                        "invalid update hunk header: {}",
                        escape_path_text(header)
                    ));
                };
                index += 1;

                let mut old_lines = Vec::new();
                let mut new_lines = Vec::new();
                let mut is_end_of_file = false;
                while index + 1 < lines.len()
                    && !lines[index].starts_with("@@")
                    && !lines[index].starts_with("*** ")
                {
                    if lines[index] == "*** End of File" {
                        is_end_of_file = true;
                        index += 1;
                        break;
                    }
                    let mut chars = lines[index].chars();
                    match chars.next() {
                        None => {
                            old_lines.push(String::new());
                            new_lines.push(String::new());
                        }
                        Some(' ') => {
                            let rest = chars.as_str().to_owned();
                            old_lines.push(rest.clone());
                            new_lines.push(rest);
                        }
                        Some('-') => old_lines.push(chars.as_str().to_owned()),
                        Some('+') => new_lines.push(chars.as_str().to_owned()),
                        _ => {
                            return Err(format!(
                                "invalid update hunk line: {}",
                                escape_path_text(lines[index])
                            ));
                        }
                    }
                    index += 1;
                }

                if old_lines.is_empty() && new_lines.is_empty() {
                    return Err(format!(
                        "Update File hunk for {} must contain at least one line",
                        escape_path_text(path)
                    ));
                }
                chunks.push(UpdateChunk {
                    change_context,
                    old_lines,
                    new_lines,
                    is_end_of_file,
                });
            }

            if chunks.is_empty() {
                return Err(format!(
                    "Update File hunk for {} must contain at least one chunk",
                    escape_path_text(path)
                ));
            }
            hunks.push(Hunk::Update {
                path: PathBuf::from(path),
                move_path,
                chunks,
            });
            continue;
        }

        return Err(format!(
            "invalid patch operation: {}",
            escape_path_text(line)
        ));
    }

    if hunks.is_empty() {
        return Err("invalid patch: no file operations found".to_owned());
    }
    Ok(hunks)
}

fn escape_path_text(text: &str) -> String {
    let mut escaped = String::new();
    for ch in text.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => escaped.extend(ch.escape_default()),
            ch => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests;

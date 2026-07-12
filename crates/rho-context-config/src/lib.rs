//! Local context configuration discovery.
//!
//! This crate owns bounded loading/discovery for model-visible local context:
//! `AGENTS.md` files and Markdown skills. Rendering stays in `rho-agent`.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use serde_yaml::Value as YamlValue;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: Utf8PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextDiagnostic {
    pub path: PathBuf,
    pub kind: DiagnosticKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticKind {
    Error,
    Warning,
    Collision,
    Skipped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentsFile {
    pub file_path: Utf8PathBuf,
    pub content: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredContext {
    pub agents_files: Vec<AgentsFile>,
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<ContextDiagnostic>,
}

const AGENTS_FILENAME: &str = "AGENTS.md";
const MAX_AGENTS_FILE_BYTES: usize = 32 * 1024;

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;
const MAX_SKILL_DISCOVERY_BYTES: usize = 64 * 1024;
const SKILL_FILENAME: &str = "SKILL.md";

const DISCOVERY_WARNING_AFTER: Duration = Duration::from_millis(100);
const DISCOVERY_ERROR_AFTER: Duration = Duration::from_secs(1);
const BUNDLED_SKILLS_DIR: Option<&str> = option_env!("RHO_BUNDLED_SKILLS_DIR");

impl DiscoveredContext {
    pub fn discover(visible_repo_root: &Utf8Path, checkout_root: &Utf8Path) -> Self {
        let mut budget = DiscoveryBudget::new();
        let config_dir = dirs::config_dir();
        let config_dir = config_dir.as_deref();
        let bundled_skills_dir = BUNDLED_SKILLS_DIR.map(Path::new);
        let agents_candidates = agents_md_candidates(visible_repo_root, checkout_root, config_dir);
        let (agents_files, mut diagnostics) =
            discover_agents_md_from_candidates(&agents_candidates, &mut budget);
        let skill_roots = skill_root_pairs(
            visible_repo_root,
            checkout_root,
            config_dir,
            bundled_skills_dir,
        );
        let (skills, skill_diagnostics) = discover_skills_from_roots(&skill_roots, &mut budget);
        diagnostics.extend(skill_diagnostics);
        Self {
            agents_files,
            skills,
            diagnostics,
        }
    }
}

fn agents_md_candidates(
    visible_repo_root: &Utf8Path,
    checkout_root: &Utf8Path,
    config_dir: Option<&Path>,
) -> Vec<(PathBuf, PathBuf)> {
    let mut candidates = Vec::new();
    if let Some(config_dir) = config_dir {
        let path = config_dir.join("agents").join(AGENTS_FILENAME);
        candidates.push((path.clone(), path));
    }
    candidates.push((
        visible_repo_root.as_std_path().join(AGENTS_FILENAME),
        checkout_root.as_std_path().join(AGENTS_FILENAME),
    ));
    candidates
}

fn discover_agents_md_from_candidates(
    candidates: &[(PathBuf, PathBuf)],
    budget: &mut DiscoveryBudget,
) -> (Vec<AgentsFile>, Vec<ContextDiagnostic>) {
    let mut files = Vec::new();
    let mut diagnostics = Vec::new();

    for (visible_path, read_path) in candidates {
        if !budget.keep_going(visible_path, &mut diagnostics) {
            break;
        }
        let metadata = metadata_following_symlinks(read_path);
        if !budget.keep_going(visible_path, &mut diagnostics) {
            break;
        }
        let Some((metadata, path_to_read)) = metadata else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }

        let file_path = match Utf8PathBuf::from_path_buf(visible_path.clone()) {
            Ok(path) => path,
            Err(path) => {
                diagnostics.push(ContextDiagnostic {
                    path,
                    kind: DiagnosticKind::Skipped,
                    message: "AGENTS.md path is not valid UTF-8".to_owned(),
                });
                continue;
            }
        };

        let content = read_agents_md_file(&path_to_read, &mut diagnostics);
        if !budget.keep_going(visible_path, &mut diagnostics) {
            break;
        }
        let Some(content) = content else {
            continue;
        };
        if content.trim().is_empty() {
            continue;
        }
        files.push(AgentsFile { file_path, content });
    }

    (files, diagnostics)
}

fn read_agents_md_file(path: &Path, diagnostics: &mut Vec<ContextDiagnostic>) -> Option<String> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) => {
            diagnostics.push(ContextDiagnostic {
                path: path.to_owned(),
                kind: DiagnosticKind::Warning,
                message: format!("failed to read: {error}"),
            });
            return None;
        }
    };
    let mut bytes = Vec::new();
    if let Err(error) = file
        .by_ref()
        .take(MAX_AGENTS_FILE_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
    {
        diagnostics.push(ContextDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: format!("failed to read: {error}"),
        });
        return None;
    }

    if MAX_AGENTS_FILE_BYTES < bytes.len() {
        bytes.truncate(MAX_AGENTS_FILE_BYTES);
        diagnostics.push(ContextDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: format!("AGENTS.md exceeds {MAX_AGENTS_FILE_BYTES} bytes; truncating"),
        });
    }

    Some(String::from_utf8_lossy(&bytes).into_owned())
}

struct DiscoveryBudget {
    started_at: Instant,
    warned: bool,
    stopped: bool,
}

impl DiscoveryBudget {
    fn new() -> Self {
        Self {
            started_at: Instant::now(),
            warned: false,
            stopped: false,
        }
    }

    fn keep_going(&mut self, path: &Path, diagnostics: &mut Vec<ContextDiagnostic>) -> bool {
        if self.stopped {
            return false;
        }

        let elapsed = self.started_at.elapsed();
        if elapsed >= DISCOVERY_ERROR_AFTER {
            self.stopped = true;
            diagnostics.push(ContextDiagnostic {
                path: path.to_owned(),
                kind: DiagnosticKind::Error,
                message: "context discovery exceeded 1s time budget; stopping further exploration"
                    .to_owned(),
            });
            return false;
        }

        if elapsed >= DISCOVERY_WARNING_AFTER && !self.warned {
            self.warned = true;
            diagnostics.push(ContextDiagnostic {
                path: path.to_owned(),
                kind: DiagnosticKind::Warning,
                message: "context discovery exceeded 100ms; continuing with 1s hard budget"
                    .to_owned(),
            });
        }

        true
    }
}

fn skill_root_pairs(
    visible_repo_root: &Utf8Path,
    checkout_root: &Utf8Path,
    config_dir: Option<&Path>,
    bundled_skills_dir: Option<&Path>,
) -> Vec<(PathBuf, PathBuf)> {
    let mut roots = Vec::new();
    let visible_project_root = visible_repo_root.as_std_path().join(".agents/skills");
    let read_project_root = checkout_root.as_std_path().join(".agents/skills");
    if read_project_root.is_dir() {
        roots.push((visible_project_root, read_project_root));
    }
    if let Some(config_dir) = config_dir {
        let path = config_dir.join("agents/skills");
        roots.push((path.clone(), path));
    }
    if let Some(path) = bundled_skills_dir {
        roots.push((path.to_owned(), path.to_owned()));
    }
    roots
}

fn has_unclosed_frontmatter(content: &str) -> bool {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let Some(rest) = content.strip_prefix("---") else {
        return false;
    };
    let Some(rest) = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
    else {
        return false;
    };
    find_closing_fence(rest).is_none()
}

fn find_closing_fence(s: &str) -> Option<(usize, usize)> {
    let mut pos = 0;
    for line in s.split_inclusive('\n') {
        let stripped = line.trim_end_matches('\n').trim_end_matches('\r');
        if stripped.trim_end() == "---" {
            return Some((pos, pos + line.len()));
        }
        pos += line.len();
    }
    None
}

struct ParsedFrontmatter {
    fields: BTreeMap<String, String>,
    yaml_error: Option<String>,
}

fn parse_frontmatter_inner(content: &str) -> ParsedFrontmatter {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);

    let Some(rest) = content.strip_prefix("---") else {
        return ParsedFrontmatter {
            fields: BTreeMap::new(),
            yaml_error: None,
        };
    };
    let Some(rest) = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
    else {
        return ParsedFrontmatter {
            fields: BTreeMap::new(),
            yaml_error: None,
        };
    };

    let Some((yaml_end, _body_start)) = find_closing_fence(rest) else {
        return ParsedFrontmatter {
            fields: BTreeMap::new(),
            yaml_error: None,
        };
    };

    let yaml_block = &rest[..yaml_end];

    match serde_yaml::from_str::<YamlValue>(yaml_block) {
        Ok(YamlValue::Mapping(m)) => ParsedFrontmatter {
            fields: m
                .into_iter()
                .filter_map(|(k, v)| {
                    let YamlValue::String(key) = k else {
                        return None;
                    };
                    Some((key, scalar_to_string(&v)?))
                })
                .collect(),
            yaml_error: None,
        },
        Ok(_) => ParsedFrontmatter {
            fields: BTreeMap::new(),
            yaml_error: None,
        },
        Err(err) => ParsedFrontmatter {
            fields: BTreeMap::new(),
            yaml_error: Some(err.to_string()),
        },
    }
}

fn scalar_to_string(v: &YamlValue) -> Option<String> {
    match v {
        YamlValue::String(s) => Some(s.clone()),
        YamlValue::Bool(b) => Some(b.to_string()),
        YamlValue::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

struct NameValidation {
    diagnostics: Vec<ContextDiagnostic>,
    skip: bool,
}

fn validate_name(name: &str, path: &Path) -> NameValidation {
    let mut diagnostics = Vec::new();
    let mut skip = false;

    if name.is_empty() {
        diagnostics.push(ContextDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: "name is required".to_owned(),
        });
        return NameValidation {
            diagnostics,
            skip: true,
        };
    }

    if name.len() > MAX_NAME_LENGTH {
        diagnostics.push(ContextDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: format!("name exceeds {MAX_NAME_LENGTH} characters ({})", name.len()),
        });
        skip = true;
    }

    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        diagnostics.push(ContextDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_owned(),
        });
        skip = true;
    }

    if name.starts_with('-') || name.ends_with('-') {
        diagnostics.push(ContextDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: "name must not start or end with a hyphen".to_owned(),
        });
        skip = true;
    }

    if name.contains("--") {
        diagnostics.push(ContextDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: "name must not contain consecutive hyphens".to_owned(),
        });
        skip = true;
    }

    NameValidation { diagnostics, skip }
}

fn validate_description(description: &str, path: &Path) -> Vec<ContextDiagnostic> {
    let mut diagnostics = Vec::new();
    if MAX_DESCRIPTION_LENGTH < description.len() {
        diagnostics.push(ContextDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: format!(
                "description exceeds {MAX_DESCRIPTION_LENGTH} bytes ({}); truncating",
                description.len()
            ),
        });
    }
    diagnostics
}

fn truncate_with_ellipsis(value: &str, max_bytes: usize) -> Cow<'_, str> {
    if value.len() <= max_bytes {
        return Cow::Borrowed(value);
    }
    let suffix = "…";
    let mut end = max_bytes.saturating_sub(suffix.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = String::from(&value[..end]);
    truncated.push_str(suffix);
    Cow::Owned(truncated)
}

fn truncate_description(description: &str) -> Cow<'_, str> {
    truncate_with_ellipsis(description, MAX_DESCRIPTION_LENGTH)
}

fn load_skill_from_content(
    content: &str,
    file_path: &Path,
) -> (Option<Skill>, Vec<ContextDiagnostic>) {
    let mut diagnostics = Vec::new();
    let parsed = parse_frontmatter_inner(content);
    if let Some(err) = parsed.yaml_error {
        diagnostics.push(ContextDiagnostic {
            path: file_path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: format!("frontmatter YAML failed to parse: {err}"),
        });
        return (None, diagnostics);
    }
    let fm = parsed.fields;

    let name = fm
        .get("name")
        .map(|name| name.trim().to_owned())
        .unwrap_or_default();

    let name_check = validate_name(&name, file_path);
    diagnostics.extend(name_check.diagnostics);
    if name_check.skip {
        return (None, diagnostics);
    }

    let description = fm.get("description").map(|s| s.trim().to_owned());
    let description = match description {
        Some(d) if !d.is_empty() => {
            diagnostics.extend(validate_description(&d, file_path));
            truncate_description(&d).into_owned()
        }
        _ => {
            diagnostics.push(ContextDiagnostic {
                path: file_path.to_owned(),
                kind: DiagnosticKind::Skipped,
                message: "description is required".to_owned(),
            });
            return (None, diagnostics);
        }
    };

    let file_path = match Utf8PathBuf::from_path_buf(file_path.to_owned()) {
        Ok(path) => path,
        Err(path) => {
            diagnostics.push(ContextDiagnostic {
                path,
                kind: DiagnosticKind::Skipped,
                message: "skill path is not valid UTF-8".to_owned(),
            });
            return (None, diagnostics);
        }
    };

    (
        Some(Skill {
            name,
            description,
            file_path,
        }),
        diagnostics,
    )
}

const MAX_SKILL_DISCOVERY_DIRS_PER_ROOT: usize = 1024;
const MAX_SKILL_DISCOVERY_ENTRIES_PER_DIR: usize = 1024;
const MAX_SKILL_DISCOVERY_ENTRIES_PER_ROOT: usize = 8192;
const MAX_SKILL_DISCOVERY_DEPTH: usize = 32;

struct DiscoveryState {
    visited_dir_count: usize,
    inspected_entry_count: usize,
    visited_dirs: HashSet<PathBuf>,
    diagnostics: Vec<ContextDiagnostic>,
    stop_root: bool,
}

impl DiscoveryState {
    fn new() -> Self {
        Self {
            visited_dir_count: 0,
            inspected_entry_count: 0,
            visited_dirs: HashSet::new(),
            diagnostics: Vec::new(),
            stop_root: false,
        }
    }

    fn push_warning(&mut self, path: &Path, message: impl Into<String>) {
        self.diagnostics.push(ContextDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: message.into(),
        });
    }
}

fn discover_skill_paths_with_diagnostics(
    visible_root: &Path,
    read_root: &Path,
    budget: &mut DiscoveryBudget,
) -> (Vec<(PathBuf, PathBuf)>, Vec<ContextDiagnostic>) {
    let mut paths = Vec::new();
    let mut state = DiscoveryState::new();
    discover_skill_paths_inner(visible_root, read_root, 0, &mut paths, &mut state, budget);
    (paths, state.diagnostics)
}

fn discover_skill_paths_inner(
    visible_dir: &Path,
    read_dir: &Path,
    depth: usize,
    out: &mut Vec<(PathBuf, PathBuf)>,
    state: &mut DiscoveryState,
    budget: &mut DiscoveryBudget,
) {
    if state.stop_root {
        return;
    }
    if !budget.keep_going(visible_dir, &mut state.diagnostics) {
        state.stop_root = true;
        return;
    }
    let dir_metadata = metadata_following_symlinks(read_dir);
    if !budget.keep_going(visible_dir, &mut state.diagnostics) {
        state.stop_root = true;
        return;
    }
    let Some((dir_metadata, dir_to_read)) = dir_metadata else {
        return;
    };
    if !dir_metadata.is_dir() {
        return;
    }
    let dir_identity = fs::canonicalize(&dir_to_read).unwrap_or_else(|_| dir_to_read.clone());
    if !state.visited_dirs.insert(dir_identity) {
        return;
    }
    if depth > MAX_SKILL_DISCOVERY_DEPTH {
        state.push_warning(
            visible_dir,
            format!(
                "skipping skill directory: discovery depth budget exceeded (max {MAX_SKILL_DISCOVERY_DEPTH})"
            ),
        );
        return;
    }
    if state.visited_dir_count >= MAX_SKILL_DISCOVERY_DIRS_PER_ROOT {
        state.push_warning(
            visible_dir,
            format!(
                "stopping skill discovery: directory budget exceeded (max {MAX_SKILL_DISCOVERY_DIRS_PER_ROOT} per root)"
            ),
        );
        state.stop_root = true;
        return;
    }
    state.visited_dir_count += 1;

    let entries = fs::read_dir(&dir_to_read);
    if !budget.keep_going(visible_dir, &mut state.diagnostics) {
        state.stop_root = true;
        return;
    }
    let entries = match entries {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut children = Vec::new();
    for entry in entries.flatten() {
        if !budget.keep_going(visible_dir, &mut state.diagnostics) {
            state.stop_root = true;
            return;
        }
        if children.len() >= MAX_SKILL_DISCOVERY_ENTRIES_PER_DIR {
            state.push_warning(
                visible_dir,
                format!(
                    "skipping skill directory: entry budget exceeded (max {MAX_SKILL_DISCOVERY_ENTRIES_PER_DIR} per directory)"
                ),
            );
            return;
        }
        if state.inspected_entry_count >= MAX_SKILL_DISCOVERY_ENTRIES_PER_ROOT {
            state.push_warning(
                visible_dir,
                format!(
                    "stopping skill discovery: entry budget exceeded (max {MAX_SKILL_DISCOVERY_ENTRIES_PER_ROOT} per root)"
                ),
            );
            state.stop_root = true;
            return;
        }
        state.inspected_entry_count += 1;
        children.push(entry);
    }
    children.sort_by_key(|entry| entry.path());

    for entry in &children {
        if !budget.keep_going(visible_dir, &mut state.diagnostics) {
            state.stop_root = true;
            return;
        }
        if entry.file_name() != SKILL_FILENAME {
            continue;
        }
        let read_path = entry.path();
        let metadata = metadata_following_symlinks(&read_path);
        if !budget.keep_going(visible_dir, &mut state.diagnostics) {
            state.stop_root = true;
            return;
        }
        let Some((metadata, file_to_read)) = metadata else {
            continue;
        };
        if metadata.is_file() {
            out.push((visible_dir.join(SKILL_FILENAME), file_to_read));
            return;
        }
    }

    for entry in &children {
        if state.stop_root {
            return;
        }
        if !budget.keep_going(visible_dir, &mut state.diagnostics) {
            state.stop_root = true;
            return;
        }
        let path = entry.path();
        let visible_path = visible_dir.join(entry.file_name());
        let metadata = metadata_following_symlinks(&path);
        if !budget.keep_going(visible_dir, &mut state.diagnostics) {
            state.stop_root = true;
            return;
        }
        let Some((metadata, child_path)) = metadata else {
            continue;
        };
        if metadata.is_dir() {
            discover_skill_paths_inner(&visible_path, &child_path, depth + 1, out, state, budget);
        }
    }
}

fn metadata_following_symlinks(path: &Path) -> Option<(fs::Metadata, PathBuf)> {
    let mut current = path.to_owned();
    let mut seen = HashSet::new();
    for _ in 0..64 {
        let metadata = fs::symlink_metadata(&current).ok()?;
        if !metadata.file_type().is_symlink() {
            return Some((metadata, current));
        }
        if !seen.insert(current.clone()) {
            return None;
        }
        let target = fs::read_link(&current).ok()?;
        let target = if target.is_absolute() {
            target
        } else {
            current
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(target)
        };
        current = target;
    }
    None
}

fn collision_message(name: &str, kept_path: &Utf8Path, ignored_path: &Utf8Path) -> String {
    format!(
        "name \"{name}\" collision — keeping {} over {} (first discovered)",
        kept_path.as_str(),
        ignored_path.as_str()
    )
}

fn read_skill_discovery_content(
    read_path: &Path,
    diagnostic_path: &Path,
) -> Result<String, ContextDiagnostic> {
    let mut file = fs::File::open(read_path).map_err(|error| ContextDiagnostic {
        path: diagnostic_path.to_owned(),
        kind: DiagnosticKind::Warning,
        message: format!("failed to read: {error}"),
    })?;
    let total_bytes = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_SKILL_DISCOVERY_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| ContextDiagnostic {
            path: diagnostic_path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: format!("failed to read: {error}"),
        })?;
    let truncated = MAX_SKILL_DISCOVERY_BYTES < bytes.len();
    if truncated {
        bytes.truncate(MAX_SKILL_DISCOVERY_BYTES);
    }
    let content = String::from_utf8_lossy(&bytes).into_owned();
    if truncated && has_unclosed_frontmatter(&content) {
        return Err(ContextDiagnostic {
            path: diagnostic_path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: format!(
                "frontmatter closing fence was not found before the {MAX_SKILL_DISCOVERY_BYTES} byte discovery read limit; file has {total_bytes} bytes"
            ),
        });
    }
    Ok(content)
}

fn discover_skills_from_roots(
    roots: &[(PathBuf, PathBuf)],
    budget: &mut DiscoveryBudget,
) -> (Vec<Skill>, Vec<ContextDiagnostic>) {
    let mut skills_by_name: BTreeMap<String, Skill> = BTreeMap::new();
    let mut all_diagnostics = Vec::new();

    for (visible_root, read_root) in roots {
        if !budget.keep_going(visible_root, &mut all_diagnostics) {
            break;
        }
        let (paths, discovery_diagnostics) =
            discover_skill_paths_with_diagnostics(visible_root, read_root, budget);
        all_diagnostics.extend(discovery_diagnostics);
        for (visible_path, read_path) in paths {
            if !budget.keep_going(&visible_path, &mut all_diagnostics) {
                break;
            }
            let content = match read_skill_discovery_content(&read_path, &visible_path) {
                Ok(content) => content,
                Err(diagnostic) => {
                    all_diagnostics.push(diagnostic);
                    continue;
                }
            };
            if !budget.keep_going(&visible_path, &mut all_diagnostics) {
                break;
            }
            let (skill, diagnostics) = load_skill_from_content(&content, &visible_path);
            all_diagnostics.extend(diagnostics);
            let Some(skill) = skill else {
                continue;
            };
            if let Some(existing) = skills_by_name.get(&skill.name) {
                all_diagnostics.push(ContextDiagnostic {
                    path: skill.file_path.as_std_path().to_owned(),
                    kind: DiagnosticKind::Collision,
                    message: collision_message(&skill.name, &existing.file_path, &skill.file_path),
                });
            } else {
                skills_by_name.insert(skill.name.clone(), skill);
            }
        }
    }

    (skills_by_name.into_values().collect(), all_diagnostics)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn write_skill(root: &Path, rel: &str, name: &str, description: &str) -> PathBuf {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!("---\nname: {name}\ndescription: {description}\n---\n\nBody for {name}.\n"),
        )
        .unwrap();
        path
    }

    fn skill_root(root: &Path) -> (PathBuf, PathBuf) {
        let root = root.join(".agents/skills");
        (root.clone(), root)
    }

    fn discover_skills_for_test(
        roots: &[(PathBuf, PathBuf)],
    ) -> (Vec<Skill>, Vec<ContextDiagnostic>) {
        let mut budget = DiscoveryBudget::new();
        discover_skills_from_roots(roots, &mut budget)
    }

    fn discover_agents_md_for_test(
        candidates: &[(PathBuf, PathBuf)],
    ) -> (Vec<AgentsFile>, Vec<ContextDiagnostic>) {
        let mut budget = DiscoveryBudget::new();
        discover_agents_md_from_candidates(candidates, &mut budget)
    }

    #[test]
    fn discovery_budget_warns_once_after_soft_limit() {
        let temp = tempfile::tempdir().unwrap();
        let mut budget = DiscoveryBudget {
            started_at: Instant::now() - DISCOVERY_WARNING_AFTER - Duration::from_millis(1),
            warned: false,
            stopped: false,
        };
        let mut diagnostics = Vec::new();

        assert!(budget.keep_going(temp.path(), &mut diagnostics));
        assert!(budget.keep_going(temp.path(), &mut diagnostics));

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].kind, DiagnosticKind::Warning);
        assert!(diagnostics[0].message.contains("100ms"));
    }

    #[test]
    fn discovery_budget_errors_once_after_hard_limit() {
        let temp = tempfile::tempdir().unwrap();
        let mut budget = DiscoveryBudget {
            started_at: Instant::now() - DISCOVERY_ERROR_AFTER - Duration::from_millis(1),
            warned: false,
            stopped: false,
        };
        let mut diagnostics = Vec::new();

        assert!(!budget.keep_going(temp.path(), &mut diagnostics));
        assert!(!budget.keep_going(temp.path(), &mut diagnostics));

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].kind, DiagnosticKind::Error);
        assert!(diagnostics[0].message.contains("1s"));
    }

    #[test]
    fn loads_project_skill() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(
            temp.path(),
            ".agents/skills/demo/SKILL.md",
            "demo",
            "Demo skill",
        );
        let repo = Utf8PathBuf::from_path_buf(temp.path().to_owned()).unwrap();
        let (skills, diagnostics) =
            discover_skills_for_test(&skill_root_pairs(&repo, &repo, None, None));
        assert_eq!(diagnostics, Vec::new());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "demo");
        assert_eq!(skills[0].description, "Demo skill");
    }

    #[test]
    fn discovered_skills_are_sorted_by_name() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(
            temp.path(),
            ".agents/skills/zeta/SKILL.md",
            "zeta",
            "Zeta skill",
        );
        write_skill(
            temp.path(),
            ".agents/skills/alpha/SKILL.md",
            "alpha",
            "Alpha skill",
        );
        let (skills, _) = discover_skills_for_test(&[skill_root(temp.path())]);
        assert_eq!(skills[0].name, "alpha");
        assert_eq!(skills[1].name, "zeta");
    }

    #[test]
    fn project_skills_override_bundled_skills() {
        let project = tempfile::tempdir().unwrap();
        let bundled = tempfile::tempdir().unwrap();
        write_skill(
            project.path(),
            ".agents/skills/demo/SKILL.md",
            "demo",
            "Project skill",
        );
        write_skill(
            bundled.path(),
            ".agents/skills/demo/SKILL.md",
            "demo",
            "Bundled skill",
        );
        let roots = [skill_root(project.path()), skill_root(bundled.path())];
        let (skills, diagnostics) = discover_skills_for_test(&roots);

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "Project skill");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.kind == DiagnosticKind::Collision)
        );
    }

    #[test]
    fn ignores_root_level_markdown_files() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(temp.path(), ".agents/skills/demo.md", "demo", "Demo skill");
        let (skills, _) = discover_skills_for_test(&[skill_root(temp.path())]);
        assert!(skills.is_empty());
    }

    #[test]
    fn requires_explicit_name() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".agents/skills/demo/SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "---\ndescription: Demo skill\n---\n\nBody.\n").unwrap();
        let (skills, diagnostics) = discover_skills_for_test(&[skill_root(temp.path())]);
        assert!(skills.is_empty());
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == "name is required")
        );
    }

    #[cfg(unix)]
    #[test]
    fn follows_symlink_to_skill_dir() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(temp.path(), "real/demo/SKILL.md", "demo", "Demo skill");
        fs::create_dir_all(temp.path().join(".agents/skills")).unwrap();
        std::os::unix::fs::symlink(
            temp.path().join("real/demo"),
            temp.path().join(".agents/skills/demo"),
        )
        .unwrap();

        let (skills, _) = discover_skills_for_test(&[skill_root(temp.path())]);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "demo");
    }

    #[cfg(unix)]
    #[test]
    fn follows_symlinked_skill_root() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(temp.path(), "real/demo/SKILL.md", "demo", "Demo skill");
        let visible_root = temp.path().join("config/agents/skills");
        fs::create_dir_all(visible_root.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(temp.path().join("real"), &visible_root).unwrap();

        let (skills, _) = discover_skills_for_test(&[(visible_root.clone(), visible_root.clone())]);

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "demo");
        assert_eq!(
            skills[0].file_path,
            Utf8PathBuf::from_path_buf(visible_root.join("demo/SKILL.md")).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn follows_chained_symlinked_skill_root() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(temp.path(), "real/demo/SKILL.md", "demo", "Demo skill");
        let link = temp.path().join("linked-skills");
        let visible_root = temp.path().join("config/agents/skills");
        fs::create_dir_all(visible_root.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(temp.path().join("real"), &link).unwrap();
        std::os::unix::fs::symlink(&link, &visible_root).unwrap();

        let (skills, _) = discover_skills_for_test(&[(visible_root.clone(), visible_root.clone())]);

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "demo");
    }

    #[cfg(unix)]
    #[test]
    fn skips_symlinked_skill_directory_cycles() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(
            temp.path(),
            ".agents/skills/demo/SKILL.md",
            "demo",
            "Demo skill",
        );
        std::os::unix::fs::symlink(
            temp.path().join(".agents/skills"),
            temp.path().join(".agents/skills/loop"),
        )
        .unwrap();

        let (skills, _) = discover_skills_for_test(&[skill_root(temp.path())]);

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "demo");
    }

    #[test]
    fn skills_read_from_checkout_root_but_report_visible_repo_path() {
        let temp = tempfile::tempdir().unwrap();
        let visible = Utf8PathBuf::from_path_buf(temp.path().join("visible")).unwrap();
        let checkout = Utf8PathBuf::from_path_buf(temp.path().join("checkout")).unwrap();
        fs::create_dir_all(&visible).unwrap();
        fs::create_dir_all(&checkout).unwrap();
        write_skill(
            checkout.as_std_path(),
            ".agents/skills/demo/SKILL.md",
            "demo",
            "Demo skill",
        );

        let (skills, _) =
            discover_skills_for_test(&skill_root_pairs(&visible, &checkout, None, None));

        assert_eq!(skills.len(), 1);
        assert_eq!(
            skills[0].file_path,
            visible.join(".agents/skills/demo/SKILL.md")
        );
    }

    #[test]
    fn loads_user_then_repo_agents_md() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let repo = Utf8PathBuf::from_path_buf(temp.path().join("repo")).unwrap();
        fs::create_dir_all(config.join("agents")).unwrap();
        fs::create_dir_all(&repo).unwrap();
        fs::write(config.join("agents/AGENTS.md"), "user instructions").unwrap();
        fs::write(repo.join("AGENTS.md"), "repo instructions").unwrap();

        let (files, diagnostics) =
            discover_agents_md_for_test(&agents_md_candidates(&repo, &repo, Some(&config)));

        assert_eq!(diagnostics, Vec::new());
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].content, "user instructions");
        assert_eq!(files[1].content, "repo instructions");
    }

    #[test]
    fn ignores_empty_and_non_exact_agents_md_candidates() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("AGENTS.extra.md"), "extra").unwrap();
        fs::write(temp.path().join("AGENTS.md"), "   \n").unwrap();

        let candidate = temp.path().join("AGENTS.md");
        let (files, _) = discover_agents_md_for_test(&[(candidate.clone(), candidate)]);

        assert!(files.is_empty());
    }

    #[test]
    fn truncates_large_agents_md() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("AGENTS.md"),
            "x".repeat(MAX_AGENTS_FILE_BYTES + 1),
        )
        .unwrap();

        let candidate = temp.path().join("AGENTS.md");
        let (files, diagnostics) = discover_agents_md_for_test(&[(candidate.clone(), candidate)]);

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content.len(), MAX_AGENTS_FILE_BYTES);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("truncating"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn follows_symlinked_agents_md_file() {
        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join("shared.md");
        fs::write(&shared, "linked instructions").unwrap();
        let candidate = temp.path().join("AGENTS.md");
        std::os::unix::fs::symlink(&shared, &candidate).unwrap();

        let (files, _) = discover_agents_md_for_test(&[(candidate.clone(), candidate.clone())]);

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content, "linked instructions");
        assert_eq!(
            files[0].file_path,
            Utf8PathBuf::from_path_buf(candidate).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn follows_chained_symlinked_agents_md_file() {
        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join("shared.md");
        let link = temp.path().join("linked.md");
        fs::write(&shared, "linked instructions").unwrap();
        std::os::unix::fs::symlink(&shared, &link).unwrap();
        let candidate = temp.path().join("AGENTS.md");
        std::os::unix::fs::symlink(&link, &candidate).unwrap();

        let (files, _) = discover_agents_md_for_test(&[(candidate.clone(), candidate.clone())]);

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content, "linked instructions");
    }

    #[test]
    fn agents_md_reads_from_checkout_root_but_reports_visible_repo_path() {
        let temp = tempfile::tempdir().unwrap();
        let visible = Utf8PathBuf::from_path_buf(temp.path().join("visible")).unwrap();
        let checkout = Utf8PathBuf::from_path_buf(temp.path().join("checkout")).unwrap();
        fs::create_dir_all(&visible).unwrap();
        fs::create_dir_all(&checkout).unwrap();
        fs::write(checkout.join("AGENTS.md"), "checkout instructions").unwrap();

        let (files, _) =
            discover_agents_md_for_test(&agents_md_candidates(&visible, &checkout, None));

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content, "checkout instructions");
        assert_eq!(files[0].file_path, visible.join("AGENTS.md"));
    }
}

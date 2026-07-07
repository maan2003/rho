//! Local Markdown skill discovery and metadata loading.
//!
//! Rho intentionally keeps skills file-backed: the model sees skill names,
//! descriptions, and file paths in its system prompt, then reads the files with
//! normal shell tools when a task calls for them.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use camino::{Utf8Path, Utf8PathBuf};
use serde_yaml::Value as YamlValue;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: Utf8PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillDiagnostic {
    pub path: PathBuf,
    pub kind: DiagnosticKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticKind {
    Warning,
    Collision,
    Skipped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredSkills {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<SkillDiagnostic>,
}

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;
const MAX_SKILL_DISCOVERY_BYTES: usize = 64 * 1024;
const SKILL_FILENAME: &str = "SKILL.md";

pub fn discover_for_repo(repo_root: &Path) -> DiscoveredSkills {
    let roots = repo_skill_roots(repo_root, dirs::config_dir().as_deref());
    discover_from_roots(&roots)
}

fn repo_skill_roots(repo_root: &Path, config_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    push_existing_project_skill_root(&mut roots, repo_root.join(".agents/skills"));
    if let Some(config_dir) = config_dir {
        roots.push(config_dir.join("agents/skills"));
    }
    roots
}

fn push_existing_project_skill_root(roots: &mut Vec<PathBuf>, path: PathBuf) {
    if path.is_dir() {
        roots.push(path);
    }
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
    diagnostics: Vec<SkillDiagnostic>,
    skip: bool,
}

fn validate_name(name: &str, path: &Path) -> NameValidation {
    let mut diagnostics = Vec::new();
    let mut skip = false;

    if name.is_empty() {
        diagnostics.push(SkillDiagnostic {
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
        diagnostics.push(SkillDiagnostic {
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
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_owned(),
        });
        skip = true;
    }

    if name.starts_with('-') || name.ends_with('-') {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: "name must not start or end with a hyphen".to_owned(),
        });
        skip = true;
    }

    if name.contains("--") {
        diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: "name must not contain consecutive hyphens".to_owned(),
        });
        skip = true;
    }

    NameValidation { diagnostics, skip }
}

fn validate_description(description: &str, path: &Path) -> Vec<SkillDiagnostic> {
    let mut diagnostics = Vec::new();
    if MAX_DESCRIPTION_LENGTH < description.len() {
        diagnostics.push(SkillDiagnostic {
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
) -> (Option<Skill>, Vec<SkillDiagnostic>) {
    let mut diagnostics = Vec::new();
    let parsed = parse_frontmatter_inner(content);
    if let Some(err) = parsed.yaml_error {
        diagnostics.push(SkillDiagnostic {
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
            diagnostics.push(SkillDiagnostic {
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
            diagnostics.push(SkillDiagnostic {
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
    diagnostics: Vec<SkillDiagnostic>,
    stop_root: bool,
}

impl DiscoveryState {
    fn new() -> Self {
        Self {
            visited_dir_count: 0,
            inspected_entry_count: 0,
            diagnostics: Vec::new(),
            stop_root: false,
        }
    }

    fn push_warning(&mut self, path: &Path, message: impl Into<String>) {
        self.diagnostics.push(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: message.into(),
        });
    }
}

fn discover_skill_paths_with_diagnostics(root: &Path) -> (Vec<PathBuf>, Vec<SkillDiagnostic>) {
    let mut paths = Vec::new();
    let mut state = DiscoveryState::new();
    discover_skill_paths_inner(root, 0, false, &mut paths, &mut state);
    (paths, state.diagnostics)
}

fn discover_skill_paths_inner(
    dir: &Path,
    depth: usize,
    followed_symlink: bool,
    out: &mut Vec<PathBuf>,
    state: &mut DiscoveryState,
) {
    if state.stop_root {
        return;
    }
    let Some((dir_metadata, followed_symlink, dir_to_read)) =
        metadata_following_one_symlink(dir, followed_symlink)
    else {
        return;
    };
    if !dir_metadata.is_dir() {
        return;
    }
    if depth > MAX_SKILL_DISCOVERY_DEPTH {
        state.push_warning(
            dir,
            format!(
                "skipping skill directory: discovery depth budget exceeded (max {MAX_SKILL_DISCOVERY_DEPTH})"
            ),
        );
        return;
    }
    if state.visited_dir_count >= MAX_SKILL_DISCOVERY_DIRS_PER_ROOT {
        state.push_warning(
            dir,
            format!(
                "stopping skill discovery: directory budget exceeded (max {MAX_SKILL_DISCOVERY_DIRS_PER_ROOT} per root)"
            ),
        );
        state.stop_root = true;
        return;
    }
    state.visited_dir_count += 1;

    let entries = match fs::read_dir(&dir_to_read) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut children = Vec::new();
    for entry in entries.flatten() {
        if children.len() >= MAX_SKILL_DISCOVERY_ENTRIES_PER_DIR {
            state.push_warning(
                dir,
                format!(
                    "skipping skill directory: entry budget exceeded (max {MAX_SKILL_DISCOVERY_ENTRIES_PER_DIR} per directory)"
                ),
            );
            return;
        }
        if state.inspected_entry_count >= MAX_SKILL_DISCOVERY_ENTRIES_PER_ROOT {
            state.push_warning(
                dir,
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

    if let Some(entry) = children.iter().find(|e| {
        e.file_name() == SKILL_FILENAME
            && metadata_following_one_symlink(&e.path(), followed_symlink)
                .map(|(metadata, _, _)| metadata.is_file())
                .unwrap_or(false)
    }) {
        out.push(entry.path());
        return;
    }

    for entry in &children {
        if state.stop_root {
            return;
        }
        let path = entry.path();
        let Some((metadata, child_followed_symlink, child_path)) =
            metadata_following_one_symlink(&path, followed_symlink)
        else {
            continue;
        };
        if metadata.is_dir() {
            discover_skill_paths_inner(&child_path, depth + 1, child_followed_symlink, out, state);
        }
    }
}

fn metadata_following_one_symlink(
    path: &Path,
    followed_symlink: bool,
) -> Option<(fs::Metadata, bool, PathBuf)> {
    let link_metadata = fs::symlink_metadata(path).ok()?;
    if link_metadata.file_type().is_symlink() {
        if followed_symlink {
            return None;
        }
        let target = fs::read_link(path).ok()?;
        let target = if target.is_absolute() {
            target
        } else {
            path.parent().unwrap_or_else(|| Path::new("")).join(target)
        };
        let target_metadata = fs::symlink_metadata(&target).ok()?;
        if target_metadata.file_type().is_symlink() {
            return None;
        }
        return Some((target_metadata, true, target));
    }
    Some((link_metadata, followed_symlink, path.to_owned()))
}

fn collision_message(name: &str, kept_path: &Utf8Path, ignored_path: &Utf8Path) -> String {
    format!(
        "name \"{name}\" collision — keeping {} over {} (first discovered)",
        kept_path.as_str(),
        ignored_path.as_str()
    )
}

fn read_skill_discovery_content(path: &Path) -> Result<String, SkillDiagnostic> {
    let mut file = fs::File::open(path).map_err(|error| SkillDiagnostic {
        path: path.to_owned(),
        kind: DiagnosticKind::Warning,
        message: format!("failed to read: {error}"),
    })?;
    let total_bytes = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_SKILL_DISCOVERY_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: format!("failed to read: {error}"),
        })?;
    let truncated = MAX_SKILL_DISCOVERY_BYTES < bytes.len();
    if truncated {
        bytes.truncate(MAX_SKILL_DISCOVERY_BYTES);
    }
    let content = String::from_utf8_lossy(&bytes).into_owned();
    if truncated && has_unclosed_frontmatter(&content) {
        return Err(SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Skipped,
            message: format!(
                "frontmatter closing fence was not found before the {MAX_SKILL_DISCOVERY_BYTES} byte discovery read limit; file has {total_bytes} bytes"
            ),
        });
    }
    Ok(content)
}

fn discover_from_roots(roots: &[PathBuf]) -> DiscoveredSkills {
    let mut skills_by_name: BTreeMap<String, Skill> = BTreeMap::new();
    let mut all_diagnostics = Vec::new();

    for root in roots {
        let (paths, discovery_diagnostics) = discover_skill_paths_with_diagnostics(root);
        all_diagnostics.extend(discovery_diagnostics);
        for path in paths {
            let content = match read_skill_discovery_content(&path) {
                Ok(content) => content,
                Err(diagnostic) => {
                    all_diagnostics.push(diagnostic);
                    continue;
                }
            };
            let (skill, diagnostics) = load_skill_from_content(&content, &path);
            all_diagnostics.extend(diagnostics);
            let Some(skill) = skill else {
                continue;
            };
            if let Some(existing) = skills_by_name.get(&skill.name) {
                all_diagnostics.push(SkillDiagnostic {
                    path: skill.file_path.as_std_path().to_owned(),
                    kind: DiagnosticKind::Collision,
                    message: collision_message(&skill.name, &existing.file_path, &skill.file_path),
                });
            } else {
                skills_by_name.insert(skill.name.clone(), skill);
            }
        }
    }

    DiscoveredSkills {
        skills: skills_by_name.into_values().collect(),
        diagnostics: all_diagnostics,
    }
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

    #[test]
    fn loads_project_skill() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(
            temp.path(),
            ".agents/skills/demo/SKILL.md",
            "demo",
            "Demo skill",
        );
        let result = discover_from_roots(&repo_skill_roots(temp.path(), None));
        assert_eq!(result.diagnostics, Vec::new());
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].name, "demo");
        assert_eq!(result.skills[0].description, "Demo skill");
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
        let result = discover_from_roots(&[temp.path().join(".agents/skills")]);
        assert_eq!(result.skills[0].name, "alpha");
        assert_eq!(result.skills[1].name, "zeta");
    }

    #[test]
    fn ignores_root_level_markdown_files() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(temp.path(), ".agents/skills/demo.md", "demo", "Demo skill");
        let result = discover_from_roots(&[temp.path().join(".agents/skills")]);
        assert!(result.skills.is_empty());
    }

    #[test]
    fn requires_explicit_name() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".agents/skills/demo/SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "---\ndescription: Demo skill\n---\n\nBody.\n").unwrap();
        let result = discover_from_roots(&[temp.path().join(".agents/skills")]);
        assert!(result.skills.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == "name is required")
        );
    }

    #[cfg(unix)]
    #[test]
    fn follows_one_symlink_to_skill_dir() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(temp.path(), "real/demo/SKILL.md", "demo", "Demo skill");
        fs::create_dir_all(temp.path().join(".agents/skills")).unwrap();
        std::os::unix::fs::symlink(
            temp.path().join("real/demo"),
            temp.path().join(".agents/skills/demo"),
        )
        .unwrap();

        let result = discover_from_roots(&[temp.path().join(".agents/skills")]);
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].name, "demo");
    }
}

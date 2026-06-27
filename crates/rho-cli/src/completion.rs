use std::path::{Path, PathBuf};

use rho_cli_term_raw::Candidate;

use crate::slash_commands::slash_completion;

pub(crate) fn completion_candidates(buffer: &str, cursor: usize) -> Vec<Candidate> {
    if let slash @ [_, ..] = slash_completion(buffer, cursor).as_slice() {
        return slash.to_vec();
    }
    path_completion(buffer, cursor)
}

fn path_completion(buffer: &str, cursor: usize) -> Vec<Candidate> {
    if cursor > buffer.len() {
        return Vec::new();
    }
    let before_cursor = &buffer[..cursor];
    let token_start = before_cursor
        .rfind(char::is_whitespace)
        .map_or(0, |index| index + 1);
    let token = &before_cursor[token_start..];
    let first_token = buffer[..token_start].trim().is_empty();
    if first_token && token.starts_with('/') {
        return Vec::new();
    }
    let Some((base_dir, typed_prefix, display_prefix)) = path_parts(token) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(base_dir) else {
        return Vec::new();
    };
    let mut candidates = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_name = entry.file_name().into_string().ok()?;
            if !file_name.starts_with(typed_prefix) {
                return None;
            }
            let is_dir = entry.file_type().ok()?.is_dir();
            let suffix = if is_dir { "/" } else { "" };
            let replacement = format!(
                "{}{}{}{}",
                &buffer[..token_start],
                display_prefix,
                file_name,
                suffix
            );
            Some(Candidate {
                label: format!("{display_prefix}{file_name}{suffix}"),
                description: if is_dir { "directory" } else { "file" }.to_owned(),
                replacement,
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| a.label.cmp(&b.label));
    candidates
}

fn path_parts(token: &str) -> Option<(PathBuf, &str, String)> {
    if token == "~" {
        return Some((dirs::home_dir()?, "", "~/".to_owned()));
    }
    if let Some(rest) = token.strip_prefix("~/") {
        return split_path_completion(dirs::home_dir()?, "~/", rest);
    }
    if let Some(rest) = token.strip_prefix("./") {
        return split_path_completion(PathBuf::from("."), "./", rest);
    }
    if let Some(rest) = token.strip_prefix("../") {
        return split_path_completion(PathBuf::from(".."), "../", rest);
    }
    if token.starts_with('/') {
        return split_path_completion(PathBuf::from("/"), "/", &token[1..]);
    }
    None
}

fn split_path_completion<'a>(
    root: PathBuf,
    root_display: &str,
    rest: &'a str,
) -> Option<(PathBuf, &'a str, String)> {
    let (dir_rest, typed_prefix) = rest.rsplit_once('/').unwrap_or(("", rest));
    let base_dir = if dir_rest.is_empty() {
        root
    } else {
        root.join(dir_rest)
    };
    let display_prefix = if dir_rest.is_empty() {
        root_display.to_owned()
    } else {
        format!("{root_display}{dir_rest}/")
    };
    Some((normalize_dir(base_dir), typed_prefix, display_prefix))
}

fn normalize_dir(path: PathBuf) -> PathBuf {
    if path.as_os_str().is_empty() {
        Path::new(".").to_path_buf()
    } else {
        path
    }
}

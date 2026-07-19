use serde::{Deserialize, Serialize};

/// Maximum receive-pack command prefix accepted before pack data is streamed.
pub const MAX_RECEIVE_PACK_COMMAND_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefUpdate {
    pub old: String,
    pub new: String,
    pub reference: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivePackCommands {
    pub end: usize,
    pub updates: Vec<RefUpdate>,
}

/// Parse the bounded command prefix at the start of a receive-pack request.
///
/// `Ok(None)` means that more bytes are needed. The parser is shared by the
/// daemon-side remote helper and the GUI credential holder so neither side
/// relies on the other's validation.
pub fn parse_receive_pack_commands(input: &[u8]) -> Result<Option<ReceivePackCommands>, String> {
    let mut offset = 0;
    let mut first = true;
    let mut updates = Vec::new();
    loop {
        if input.len() < offset + 4 {
            return Ok(None);
        }
        let length_text = std::str::from_utf8(&input[offset..offset + 4])
            .map_err(|_| "invalid receive-pack packet length".to_owned())?;
        let length = usize::from_str_radix(length_text, 16)
            .map_err(|_| "invalid receive-pack packet length".to_owned())?;
        if length == 0 {
            return Ok(Some(ReceivePackCommands {
                end: offset + 4,
                updates,
            }));
        }
        if length < 4 {
            return Err("invalid receive-pack packet length".to_owned());
        }
        if offset.saturating_add(length) > MAX_RECEIVE_PACK_COMMAND_BYTES {
            return Err("git receive-pack command list is too large".to_owned());
        }
        if input.len() < offset + length {
            return Ok(None);
        }
        let mut command = &input[offset + 4..offset + length];
        if command.starts_with(b"shallow ") {
            offset += length;
            continue;
        }
        if command.starts_with(b"push-cert") {
            return Err("client SSH transport does not support signed pushes".to_owned());
        }
        if first {
            let mut parts = command.splitn(2, |byte| *byte == 0);
            command = parts.next().unwrap_or(command);
            if let Some(capabilities) = parts.next()
                && capabilities
                    .split(|byte| byte.is_ascii_whitespace())
                    .any(|capability| capability == b"push-options")
            {
                return Err("client SSH transport does not support push options".to_owned());
            }
            first = false;
        }
        let command = std::str::from_utf8(command)
            .map_err(|_| "receive-pack command is not UTF-8".to_owned())?
            .trim_end_matches('\n');
        let mut fields = command.split_ascii_whitespace();
        let old = fields
            .next()
            .ok_or_else(|| "invalid receive-pack command".to_owned())?;
        let new = fields
            .next()
            .ok_or_else(|| "invalid receive-pack command".to_owned())?;
        let reference = fields
            .next()
            .ok_or_else(|| "invalid receive-pack command".to_owned())?;
        if fields.next().is_some()
            || !valid_object_id(old)
            || !valid_object_id(new)
            || old.len() != new.len()
            || !valid_git_ref(reference)
        {
            return Err("invalid receive-pack command".to_owned());
        }
        updates.push(RefUpdate {
            old: old.to_owned(),
            new: new.to_owned(),
            reference: reference.to_owned(),
        });
        offset += length;
    }
}

fn valid_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_git_ref(reference: &str) -> bool {
    let Some(suffix) = reference.strip_prefix("refs/") else {
        return false;
    };
    !suffix.is_empty()
        && !suffix.starts_with('.')
        && !suffix.ends_with(['/', '.'])
        && !suffix.contains("..")
        && !suffix.contains("@{")
        && !suffix.contains("//")
        && !suffix
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
}

/// Socket shared by Octo API clients, the HTTP Git helper, and Rho daemon.
pub fn socket_path() -> std::io::Result<std::path::PathBuf> {
    dirs::runtime_dir()
        .or_else(dirs::state_dir)
        .map(|base| base.join("rho").join("octo.sock"))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "neither runtime nor state directory is available",
            )
        })
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CiStatusResponse {
    pub pr: PrInfo,
    pub runs: Vec<WorkflowRun>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkflowRunResponse {
    pub run: WorkflowRun,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PrInfo {
    pub number: u64,
    pub branch: String,
    pub state: String,
    pub head_sha: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
    pub name: String,
    pub kind: String,
    pub url: String,
    pub status: String,
    pub conclusion: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PrCreateRequest {
    pub head: String,
    pub base: String,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PrCreateResponse {
    pub number: u64,
    pub url: String,
    pub head: String,
    pub base: String,
    pub draft: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PrSnapshot {
    pub repository_id: u64,
    pub authenticated_user_id: Option<u64>,
    pub pr_author_id: Option<u64>,
    pub number: u64,
    pub url: String,
    pub state: String,
    pub merged: bool,
    pub draft: bool,
    pub mergeable: Option<bool>,
    pub mergeable_state: String,
    pub review_decision: String,
    pub head_sha: String,
    pub legacy_status: Option<String>,
    pub pending_review_ids: Vec<u64>,
    pub feedback: Vec<PrFeedback>,
    pub runs: Vec<WorkflowRun>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PrFeedback {
    /// `issue`, `inline`, or `review`.
    pub surface: String,
    pub id: u64,
    pub updated_at: String,
    pub author: String,
    pub author_id: Option<u64>,
    pub author_type: String,
    pub author_association: String,
    pub body: String,
    pub url: String,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub diff_hunk: Option<String>,
    pub review_id: Option<u64>,
    pub review_state: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PrCommentRequest {
    pub body: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PrCommentResponse {
    pub id: u64,
    pub url: String,
}

#[cfg(test)]
mod git_tests {
    use super::*;

    fn packet(reference: &str, capabilities: &str) -> Vec<u8> {
        let payload = format!(
            "0000000000000000000000000000000000000000 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa {reference}\0{capabilities}\n"
        );
        format!("{:04x}{payload}0000", payload.len() + 4).into_bytes()
    }

    #[test]
    fn parses_exact_ref_updates() {
        let input = packet("refs/heads/main", "report-status");
        let commands = parse_receive_pack_commands(&input).unwrap().unwrap();
        assert_eq!(commands.end, input.len());
        assert_eq!(commands.updates[0].reference, "refs/heads/main");
    }

    #[test]
    fn waits_for_a_complete_packet() {
        let input = packet("refs/heads/rho/test", "report-status");
        assert_eq!(parse_receive_pack_commands(&input[..12]).unwrap(), None);
    }

    #[test]
    fn rejects_unsafe_refs_and_push_options() {
        for reference in ["refs/heads/../main", "refs/heads/a.lock", "HEAD"] {
            assert!(parse_receive_pack_commands(&packet(reference, "report-status")).is_err());
        }
        assert!(
            parse_receive_pack_commands(&packet(
                "refs/heads/rho/test",
                "report-status push-options"
            ))
            .is_err()
        );
    }
}

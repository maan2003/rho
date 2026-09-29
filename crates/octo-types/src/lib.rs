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
/// host-side remote helper and the GUI credential holder so neither side
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

pub fn valid_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn valid_git_ref(reference: &str) -> bool {
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

pub fn valid_ssh_repository(host: &str, value: &str) -> bool {
    let value = match host {
        "github.com" => value,
        "git.sr.ht" => {
            let Some(value) = value.strip_prefix('~') else {
                return false;
            };
            value
        }
        _ => return false,
    };
    let mut components = value.split('/');
    let valid = |component: &str| {
        !matches!(component, "" | "." | "..")
            && component
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    };
    components.next().is_some_and(valid)
        && components.next().is_some_and(valid)
        && components.next().is_none()
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

    #[test]
    fn validates_allowlisted_repository_shapes() {
        assert!(valid_ssh_repository("github.com", "acme/project"));
        assert!(valid_ssh_repository(
            "github.com",
            "fedibtc/decentralized-federations"
        ));
        assert!(valid_ssh_repository("github.com", "acme/project_name"));
        assert!(valid_ssh_repository("github.com", "acme/.github"));
        assert!(valid_ssh_repository("git.sr.ht", "~alice/project"));
        for (host, repository) in [
            ("github.com", "acme/.."),
            ("github.com", "acme/project name"),
            ("github.com", "acme/project@name"),
            ("git.sr.ht", "alice/project"),
            ("git.sr.ht", "~alice/project:name"),
        ] {
            assert!(!valid_ssh_repository(host, repository));
        }
    }
}

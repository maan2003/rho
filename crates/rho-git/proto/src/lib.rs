//! The mirror store's wire protocol and naming, shared by the keeper the
//! daemon runs (`rho-git-server`) and its clients (`rho-git-client`, the
//! `git` wrapper agents run).
//!
//! A client connects to the keeper's unix socket, writes one request line
//! and reads one reply line:
//!
//! ```text
//! ensure <url>     init the mirror if missing, fetch it if stale
//! refresh <url>    fetch now (subject to the keeper's debounce)
//! ok <mirror>      the mirror's bare git directory
//! error <message>
//! ```
//!
//! Both requests block until the mirror is current. Everything is plain
//! text on one line, so a client needs nothing but a socket.

use std::fmt::Write as _;
use std::path::PathBuf;

/// Environment variable naming the keeper's socket. Absent, clients behave
/// as plain git.
pub const SOCKET_ENV: &str = "RHO_GIT_STORE_SOCKET";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Ensure { url: String },
    Refresh { url: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Ok { mirror: PathBuf },
    Error { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolError(pub String);

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProtocolError {}

impl Request {
    pub fn url(&self) -> &str {
        match self {
            Self::Ensure { url } | Self::Refresh { url } => url,
        }
    }

    /// The request as one line, newline included.
    pub fn encode(&self) -> Result<String, ProtocolError> {
        let (verb, url) = match self {
            Self::Ensure { url } => ("ensure", url),
            Self::Refresh { url } => ("refresh", url),
        };
        let url = url.trim();
        if url.is_empty() || url.chars().any(char::is_whitespace) {
            return Err(ProtocolError(format!("invalid remote URL {url:?}")));
        }
        Ok(format!("{verb} {url}\n"))
    }

    pub fn decode(line: &str) -> Result<Self, ProtocolError> {
        let line = line.trim_end_matches(['\r', '\n']);
        match line.split_once(' ') {
            Some((verb @ ("ensure" | "refresh"), url)) if !url.trim().is_empty() => {
                let url = url.trim().to_owned();
                Ok(if verb == "ensure" {
                    Self::Ensure { url }
                } else {
                    Self::Refresh { url }
                })
            }
            _ => Err(ProtocolError(format!("unrecognized request {line:?}"))),
        }
    }
}

impl Response {
    /// The reply as one line, newline included.
    pub fn encode(&self) -> String {
        match self {
            Self::Ok { mirror } => format!("ok {}\n", mirror.display()),
            Self::Error { message } => {
                format!("error {}\n", message.replace(['\r', '\n'], " "))
            }
        }
    }

    pub fn decode(line: &str) -> Result<Self, ProtocolError> {
        let line = line.trim_end_matches(['\r', '\n']);
        match line.split_once(' ') {
            Some(("ok", path)) if !path.is_empty() => Ok(Self::Ok {
                mirror: PathBuf::from(path),
            }),
            Some(("error", message)) => Ok(Self::Error {
                message: message.to_owned(),
            }),
            _ => Err(ProtocolError(format!(
                "the store closed the connection ({line:?})"
            ))),
        }
    }
}

/// A remote URL with the spellings that name the same repository folded:
/// surrounding whitespace, trailing slashes and a `.git` suffix.
pub fn normalize_remote_url(url: &str) -> String {
    let mut url = url.trim();
    while let Some(stripped) = url.strip_suffix('/') {
        url = stripped;
    }
    url.strip_suffix(".git").unwrap_or(url).to_owned()
}

/// The directory name of `url`'s mirror under the store root: a readable
/// slug from the URL's last component plus a hash of the normalized URL,
/// so distinct remotes never collide and one remote spelled two ways lands
/// in one mirror.
pub fn store_key(url: &str) -> String {
    let normalized = normalize_remote_url(url);
    let slug: String = last_component(&normalized)
        .chars()
        .take(40)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let slug = slug.trim_start_matches(['.', '-']);
    let slug = if slug.is_empty() { "repo" } else { slug };
    let mut hex = String::with_capacity(16);
    write!(hex, "{:016x}", fnv1a(normalized.as_bytes())).unwrap();
    format!("{slug}-{hex}")
}

/// The repository name git would derive for a clone of `url`.
pub fn repo_name(url: &str) -> Option<String> {
    let normalized = normalize_remote_url(url);
    let name = last_component(&normalized);
    (!name.is_empty()).then(|| name.to_owned())
}

fn last_component(normalized: &str) -> &str {
    normalized
        .rsplit(['/', ':', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or("")
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        let request = Request::Ensure {
            url: "https://example.com/org/repo.git".to_owned(),
        };
        let line = request.encode().unwrap();
        assert_eq!(line, "ensure https://example.com/org/repo.git\n");
        assert_eq!(Request::decode(&line).unwrap(), request);
        let refresh = Request::Refresh {
            url: "/tmp/remote.git".to_owned(),
        };
        assert_eq!(
            Request::decode(&refresh.encode().unwrap()).unwrap(),
            refresh
        );
        assert!(Request::decode("ensure\n").is_err());
        assert!(Request::decode("fetch x\n").is_err());
        assert!(
            Request::Ensure {
                url: "a b".to_owned()
            }
            .encode()
            .is_err()
        );
    }

    #[test]
    fn responses_round_trip() {
        let ok = Response::Ok {
            mirror: PathBuf::from("/state/stores/repo-1/git"),
        };
        assert_eq!(Response::decode(&ok.encode()).unwrap(), ok);
        let error = Response::Error {
            message: "no\nway".to_owned(),
        };
        assert_eq!(
            Response::decode(&error.encode()).unwrap(),
            Response::Error {
                message: "no way".to_owned()
            }
        );
        assert!(Response::decode("").is_err());
    }

    #[test]
    fn store_keys_are_stable_and_normalized() {
        let a = store_key("https://github.com/example/repo.git");
        assert_eq!(a, store_key("https://github.com/example/repo"));
        assert_eq!(a, store_key(" https://github.com/example/repo/ "));
        assert!(a.starts_with("repo-"), "{a}");
        assert_ne!(a, store_key("https://github.com/other/repo"));
        assert_ne!(a, store_key("git@github.com:example/repo.git"));
        let weird = store_key("/tmp/some dir/x y.git");
        assert!(weird.starts_with("x_y-"), "{weird}");
        assert!(store_key("").starts_with("repo-"));
    }

    #[test]
    fn repo_names_follow_the_url() {
        assert_eq!(
            repo_name("https://github.com/org/repo.git").unwrap(),
            "repo"
        );
        assert_eq!(repo_name("git@github.com:org/repo").unwrap(), "repo");
        assert_eq!(repo_name("/tmp/remote.git/").unwrap(), "remote");
        assert!(repo_name("").is_none());
    }
}

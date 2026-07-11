use std::env;
use std::os::unix::fs::FileTypeExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use reqwest::Url;

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let remote = args.next().context("missing remote name")?;
    let url = args.next().context("missing Octo remote URL")?;
    let (owner, repo) = parse_remote(&url)?;
    let socket = env::var("OCTO_SOCKET").context("OCTO_SOCKET environment variable not set")?;
    let socket_path = Path::new(&socket);
    if !socket_path.is_absolute() {
        anyhow::bail!("OCTO_SOCKET must be an absolute path");
    }
    let socket_type = socket_path
        .metadata()
        .context("OCTO_SOCKET is unavailable")?
        .file_type();
    if !socket_type.is_socket() {
        anyhow::bail!("OCTO_SOCKET does not refer to a Unix socket");
    }
    let remote_http: PathBuf = env::var_os("OCTO_REMOTE_HTTP")
        .map(PathBuf::from)
        .or_else(|| option_env!("OCTO_REMOTE_HTTP").map(PathBuf::from))
        .context("git-remote-octo was built without Rho's patched git-remote-http")?;
    if !Path::new(&remote_http).is_file() {
        anyhow::bail!(
            "Rho's patched git-remote-http was not found at {}",
            Path::new(&remote_http).display()
        );
    }

    let http_url = format!("http://localhost/git/{owner}/{repo}.git");
    let status = Command::new(remote_http)
        .arg(remote)
        .arg(http_url)
        .env("GIT_HTTP_UNIX_SOCKET", socket)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run Git's remote-http helper")?;

    if !status.success() {
        anyhow::bail!("git-remote-http exited with {status}");
    }
    Ok(())
}

fn parse_remote(value: &str) -> Result<(String, String)> {
    let url =
        Url::parse(value).context("Octo remote must be octo://github.com/OWNER/REPOSITORY")?;
    if url.scheme() != "octo"
        || url.host_str() != Some("github.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        anyhow::bail!("Octo remote must be octo://github.com/OWNER/REPOSITORY");
    }
    let parts: Vec<_> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() != 2 {
        anyhow::bail!("Octo remote must be octo://github.com/OWNER/REPOSITORY");
    }
    let owner = parts[0];
    let repo = parts[1].trim_end_matches(".git");
    if !valid_owner(owner) || !valid_repo(repo) {
        anyhow::bail!("Octo remote must be octo://github.com/OWNER/REPOSITORY");
    }
    Ok((owner.to_owned(), repo.to_owned()))
}

fn valid_owner(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn valid_repo(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_github_path_segments() {
        assert!(valid_owner("acme-inc"));
        assert!(valid_repo("some_repo.rs"));
        assert!(!valid_owner("acme?service=elsewhere"));
        assert!(!valid_repo("../elsewhere"));
        assert!(!valid_repo("repo/name"));
    }

    #[test]
    fn parses_explicit_github_remote() {
        assert_eq!(
            parse_remote("octo://github.com/acme/library.git").unwrap(),
            ("acme".to_owned(), "library".to_owned())
        );
        for value in [
            "octo::acme/library",
            "octo://gitlab.com/acme/library",
            "octo://github.com/acme/library/extra",
            "octo://github.com/acme/library?other=true",
        ] {
            assert!(parse_remote(value).is_err(), "accepted {value}");
        }
    }
}

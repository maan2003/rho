use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, bail};
use rho_ui_proto::{ClientMessage, PrCommand, ServerMessage};

use crate::{PrArgs, PrCliCommand, connect_or_start_daemon, default_socket_path};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) async fn run(args: PrArgs) -> anyhow::Result<()> {
    if matches!(&args.command, PrCliCommand::Init) {
        return init(args).await;
    }
    let response_run_id = match &args.command {
        PrCliCommand::Logs { run_id, .. } => Some(*run_id),
        _ => None,
    };
    let watch_interval = match &args.command {
        PrCliCommand::Checks {
            watch: true,
            interval,
            ..
        } => {
            anyhow::ensure!(*interval > 0, "--interval must be at least one second");
            Some(std::time::Duration::from_secs(*interval))
        }
        _ => None,
    };
    let command = command(args.command)?;
    let socket_path = args.socket_path.unwrap_or(default_socket_path()?);
    let mut daemon = connect_or_start_daemon(&socket_path, args.auth.as_deref()).await?;
    loop {
        let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        daemon
            .send(&ClientMessage::PrCommand {
                request_id,
                agent_id: None,
                command: command.clone(),
            })
            .await?;
        loop {
            if let ServerMessage::PrCommandResult {
                request_id: response_id,
                output,
                data,
                is_error,
            } = daemon.recv().await?
                && response_id == request_id
            {
                if is_error {
                    bail!(output);
                }
                if data.is_empty() {
                    println!("{output}");
                } else {
                    extract_logs(
                        &data,
                        response_run_id.context("binary response for non-log command")?,
                    )?;
                }
                if let Some(interval) = watch_interval {
                    if !checks_pending(&output)? {
                        return Ok(());
                    }
                    tokio::time::sleep(interval).await;
                    break;
                }
                return Ok(());
            }
        }
    }
}

fn command(command: PrCliCommand) -> anyhow::Result<PrCommand> {
    Ok(match command {
        PrCliCommand::Init => unreachable!("handled before command conversion"),
        PrCliCommand::Create {
            head,
            base,
            title,
            body,
        } => {
            let (owner, repo) = resolve_repo()?;
            PrCommand::Create {
                owner,
                repo,
                head,
                base: match base {
                    Some(base) => base,
                    None => resolve_default_base_branch()?,
                },
                title,
                body,
                review_bots: Vec::new(),
            }
        }
        PrCliCommand::Status { url } => PrCommand::Status { url },
        PrCliCommand::Edit {
            url,
            base,
            title,
            body,
        } => {
            anyhow::ensure!(
                base.is_some() || title.is_some() || body.is_some(),
                "provide --base, --title, or --body"
            );
            PrCommand::Edit {
                url,
                base,
                title,
                body,
            }
        }
        PrCliCommand::Comment {
            url,
            reply_comment,
            body,
        } => PrCommand::Comment {
            url,
            reply_comment,
            body,
        },
        PrCliCommand::Comments { url } => PrCommand::Comments { url },
        PrCliCommand::Checks { url, .. } => PrCommand::Checks { url },
        PrCliCommand::Rerun { url, run_id } => PrCommand::Rerun { url, run_id },
        PrCliCommand::Logs { url, run_id } => PrCommand::Logs { url, run_id },
    })
}

fn checks_pending(output: &str) -> anyhow::Result<bool> {
    serde_json::from_str::<serde_json::Value>(output)?
        .get("pending")
        .and_then(serde_json::Value::as_bool)
        .context("CI checks response is missing pending state")
}

async fn init(args: PrArgs) -> anyhow::Result<()> {
    let token = prompt_token("GitHub token (ghp_/github_pat_/...): ")?;
    let socket_path = args.socket_path.unwrap_or(default_socket_path()?);
    let mut daemon = connect_or_start_daemon(&socket_path, args.auth.as_deref()).await?;
    daemon
        .send(&ClientMessage::PlatformSecretsSet {
            secrets: vec![("GITHUB_TOKEN".to_owned(), token)],
            coordinator_repo: None,
        })
        .await?;
    loop {
        match daemon.recv().await? {
            ServerMessage::PlatformStatus {
                running: true,
                detail,
            } => {
                eprintln!("GitHub configured: {detail}");
                return Ok(());
            }
            ServerMessage::PlatformStatus {
                running: false,
                detail,
            }
            | ServerMessage::Error { message: detail } => bail!(detail),
            _ => {}
        }
    }
}

fn prompt_token(prompt: &str) -> anyhow::Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut token = String::new();
    std::io::stdin().read_line(&mut token)?;
    let token = token.trim().to_owned();
    anyhow::ensure!(!token.is_empty(), "no token entered");
    Ok(token)
}

fn resolve_repo() -> anyhow::Result<(String, String)> {
    let output = std::process::Command::new("jj")
        .args(["git", "remote", "list"])
        .output()
        .context("failed to run jj")?;
    anyhow::ensure!(output.status.success(), "jj git remote list failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let remote = stdout
        .lines()
        .find(|line| line.starts_with("origin"))
        .and_then(|line| line.split_whitespace().nth(1))
        .context("origin remote not found")?;
    parse_github_remote(remote)
}

fn parse_github_remote(remote: &str) -> anyhow::Result<(String, String)> {
    let path = remote
        .split_once("github.com:")
        .or_else(|| remote.split_once("github.com/"))
        .map(|(_, path)| path)
        .context("origin is not a GitHub remote")?
        .trim_end_matches(".git");
    let (owner, repo) = path
        .split_once('/')
        .context("GitHub remote must contain OWNER/REPO")?;
    anyhow::ensure!(
        !owner.is_empty() && !repo.is_empty(),
        "invalid GitHub remote"
    );
    Ok((owner.to_owned(), repo.to_owned()))
}

fn resolve_default_base_branch() -> anyhow::Result<String> {
    let output = std::process::Command::new("jj")
        .args([
            "log",
            "-r",
            "trunk()",
            "--no-graph",
            "-T",
            "bookmarks.first().name()",
        ])
        .output()
        .context("failed to run jj")?;
    anyhow::ensure!(output.status.success(), "jj log -r trunk() failed");
    let bookmark = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    anyhow::ensure!(!bookmark.is_empty(), "no bookmark found for trunk()");
    Ok(bookmark)
}

fn extract_logs(bytes: &[u8], run_id: u64) -> anyhow::Result<()> {
    const MAX_FILES: usize = 1_000;
    const MAX_ENTRY_BYTES: u64 = 16 * 1024 * 1024;
    const MAX_EXTRACTED_BYTES: u64 = 128 * 1024 * 1024;

    let base = dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("rho/pr-logs");
    let path = base.join(run_id.to_string());
    if path.exists() {
        std::fs::remove_dir_all(&path)?;
    }
    std::fs::create_dir_all(&path)?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    anyhow::ensure!(
        archive.len() <= MAX_FILES,
        "CI log archive contains more than {MAX_FILES} entries"
    );
    let mut jobs: BTreeMap<String, Vec<(String, bool)>> = BTreeMap::new();
    let mut extracted_bytes = 0_u64;
    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        let Some(relative) = file.enclosed_name() else {
            continue;
        };
        let output = path.join(&relative);
        if file.is_dir() {
            std::fs::create_dir_all(&output)?;
            continue;
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        anyhow::ensure!(
            file.size() <= MAX_ENTRY_BYTES,
            "CI log entry exceeds the 16 MiB extraction limit: {}",
            relative.display()
        );
        let mut contents = Vec::with_capacity(file.size() as usize);
        file.by_ref()
            .take(MAX_ENTRY_BYTES + 1)
            .read_to_end(&mut contents)?;
        anyhow::ensure!(
            contents.len() as u64 <= MAX_ENTRY_BYTES,
            "CI log entry expanded beyond the 16 MiB extraction limit: {}",
            relative.display()
        );
        extracted_bytes = extracted_bytes.saturating_add(contents.len() as u64);
        anyhow::ensure!(
            extracted_bytes <= MAX_EXTRACTED_BYTES,
            "CI log archive exceeds the 128 MiB aggregate extraction limit"
        );
        std::fs::write(&output, &contents)?;
        let name = relative.to_string_lossy().into_owned();
        let (job, step) = name
            .split_once('/')
            .map(|(job, step)| (job.to_owned(), step.to_owned()))
            .unwrap_or_else(|| ("(root)".to_owned(), name));
        let text = String::from_utf8_lossy(&contents);
        let errors =
            text.contains("##[error]") || text.contains("Process completed with exit code");
        jobs.entry(job).or_default().push((step, errors));
    }
    println!("Logs extracted to {}/", path.display());
    for (job, steps) in jobs {
        println!("{job} ({} steps)", steps.len());
        for (step, errors) in steps {
            println!("  {step}{}", if errors { "  ← has errors" } else { "" });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_github_remotes() {
        assert_eq!(
            parse_github_remote("git@github.com:acme/widgets.git").unwrap(),
            ("acme".into(), "widgets".into())
        );
        assert_eq!(
            parse_github_remote("octo://github.com/acme/widgets").unwrap(),
            ("acme".into(), "widgets".into())
        );
    }
}

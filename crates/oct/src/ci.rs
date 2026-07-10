use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use octo_types::{CiStatusResponse, WorkflowRunResponse};
use regex::Regex;

use crate::octo_client::OctoClient;
use crate::repo::resolve_repo;

static PR_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^https://github\.com/([^/]+)/([^/]+)/pull/(\d+)(?:/.*)?$").unwrap()
});

struct ResolvedPrSpec {
    owner: String,
    repo: String,
    pr: String,
}

fn resolve_pr_spec(pr_spec: &str) -> Result<ResolvedPrSpec> {
    if let Some(caps) = PR_URL_RE.captures(pr_spec) {
        return Ok(ResolvedPrSpec {
            owner: caps[1].to_string(),
            repo: caps[2].to_string(),
            pr: caps[3].to_string(),
        });
    }

    let (owner, repo) = resolve_repo()?;
    Ok(ResolvedPrSpec {
        owner,
        repo,
        pr: pr_spec.to_string(),
    })
}

#[derive(Parser)]
#[command(about = "Show CI status for a PR")]
pub struct StatusArgs {
    /// Branch name, PR number, or full GitHub URL
    pub pr_spec: String,
}

async fn fetch_status(client: &OctoClient, pr_spec: &str) -> Result<CiStatusResponse> {
    let resolved = resolve_pr_spec(pr_spec)?;
    client
        .get_with_query(
            &format!("/ci/status/{}/{}", resolved.owner, resolved.repo),
            &[("pr", resolved.pr.as_str())],
        )
        .await
}

fn print_status(resp: &CiStatusResponse) {
    println!(
        "PR #{}: {} ({})",
        resp.pr.number, resp.pr.branch, resp.pr.state
    );
    println!();

    for run in &resp.runs {
        let label = run.conclusion.as_deref().unwrap_or(&run.status);
        println!("Run {}  {:<25} {}", run.id, run.name, label);
    }
}

fn run_failed(conclusion: Option<&str>) -> bool {
    conclusion.is_some_and(|c| !matches!(c, "success" | "neutral" | "skipped"))
}

pub async fn status(args: StatusArgs) -> Result<()> {
    let client = OctoClient::from_env()?;
    let resp = fetch_status(&client, &args.pr_spec).await?;

    print_status(&resp);

    Ok(())
}

#[derive(Parser)]
#[command(about = "Wait for CI to finish")]
pub struct WaitArgs {
    /// Workflow run ID
    pub run_id: u64,

    /// Poll interval in seconds
    #[arg(long, default_value_t = 30)]
    pub poll_seconds: u64,
}

pub async fn wait(args: WaitArgs) -> Result<()> {
    let (owner, repo) = resolve_repo()?;
    let client = OctoClient::from_env()?;

    loop {
        let resp: WorkflowRunResponse = client
            .get(&format!("/ci/run/{}/{}/{}", owner, repo, args.run_id))
            .await?;
        let run = resp.run;

        let label = run.conclusion.as_deref().unwrap_or(&run.status);
        println!("Run {}  {:<25} {}", run.id, run.name, label);

        if run.status.eq_ignore_ascii_case("completed") {
            if run_failed(run.conclusion.as_deref()) {
                anyhow::bail!("workflow run completed with failure");
            }
            println!();
            println!("Workflow run completed successfully.");
            return Ok(());
        }

        println!();
        println!(
            "Workflow run still in progress. Polling again in {}s.",
            args.poll_seconds
        );
        tokio::time::sleep(Duration::from_secs(args.poll_seconds)).await;
    }
}

#[derive(Parser)]
#[command(about = "Download and extract CI logs")]
pub struct LogsArgs {
    /// Workflow run ID
    pub run_id: u64,
}

pub async fn logs(args: LogsArgs) -> Result<()> {
    let (owner, repo) = resolve_repo()?;
    let client = OctoClient::from_env()?;

    let bytes = client
        .get_bytes(&format!("/ci/logs/{}/{}/{}", owner, repo, args.run_id))
        .await?;

    let base_dir = match std::env::var("XDG_RUNTIME_DIR") {
        Ok(dir) => PathBuf::from(dir).join("oct/ci-logs"),
        Err(_) => std::env::temp_dir().join("oct/ci-logs"),
    };
    let extract_path = base_dir.join(args.run_id.to_string());

    if extract_path.exists() {
        std::fs::remove_dir_all(&extract_path)?;
    }
    std::fs::create_dir_all(&extract_path)?;

    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)?;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let name = file.name().to_string();
        let out_path = extract_path.join(&name);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !file.is_dir() {
            let mut out = std::fs::File::create(&out_path)?;
            std::io::copy(&mut file, &mut out)?;
        }
    }

    println!("Logs extracted to {}/", extract_path.display());
    println!();

    // Build a job -> files mapping
    let mut jobs: BTreeMap<String, Vec<(String, bool)>> = BTreeMap::new();
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        if file.is_dir() {
            continue;
        }
        let name = file.name().to_string();
        let parts: Vec<&str> = name.splitn(2, '/').collect();
        let (job, step) = if parts.len() == 2 {
            (parts[0].to_string(), parts[1].to_string())
        } else {
            ("(root)".to_string(), name.clone())
        };

        let mut contents = String::new();
        let _ = file.read_to_string(&mut contents);
        let has_errors =
            contents.contains("##[error]") || contents.contains("Process completed with exit code");

        jobs.entry(job).or_default().push((step, has_errors));
    }

    for (job, files) in &jobs {
        println!("{} ({} steps)", job, files.len());
        for (file, has_errors) in files {
            if *has_errors {
                println!("  {}  ← has errors", file);
            } else {
                println!("  {}", file);
            }
        }
    }

    Ok(())
}

#[derive(Parser)]
#[command(about = "Rerun a CI workflow")]
pub struct RerunArgs {
    /// Workflow run ID
    pub run_id: u64,
}

pub async fn rerun(args: RerunArgs) -> Result<()> {
    let (owner, repo) = resolve_repo()?;
    let client = OctoClient::from_env()?;

    client
        .post(&format!("/ci/rerun/{}/{}/{}", owner, repo, args.run_id))
        .await?;

    println!("Rerun triggered for workflow run {}", args.run_id);

    Ok(())
}

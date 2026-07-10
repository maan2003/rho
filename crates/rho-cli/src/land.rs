//! Local `rho land` implementation: the CLI owns jj
//! prepare/rebase/check/publish; the daemon only coordinates leases/status.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use camino::Utf8PathBuf;
use rho_ci::config::{self, MergeMode};
use rho_ci::{Candidate, CheckEvent, CheckOptions, run_check};
use rho_ui_proto::client::Client as UiClient;
use rho_ui_proto::{AgentId, ClientMessage, LandStatus, ServerMessage};

use crate::{LandArgs, connect_or_start_daemon};

pub(crate) async fn run(args: LandArgs) -> Result<()> {
    let checkout = args
        .path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", args.path.display()))?;
    let workspace_root = jj_stdout(&checkout, &["root"])
        .context("detect jj repo root")?
        .trim()
        .to_owned();
    let workspace_name = current_jj_workspace(&checkout)?;
    let workspace_root = PathBuf::from(workspace_root);
    let repo_root =
        rho_workspaces::resolve_repo_root(&workspace_root).context("resolve origin repo")?;
    let agent_id = current_agent_id()?;
    let mut lease = LandLease::acquire(
        repo_root.clone(),
        agent_id,
        &args.auth,
        args.socket_path.as_deref(),
    )
    .await?;

    let Some(config) = config::read_config(repo_root.as_std_path())? else {
        lease.status(LandStatus::Bounced).await.ok();
        return land_bail(&workspace_root, "no .config/selfci/ci.yaml in repo", "");
    };
    let Some(base_branch) = config.mq.as_ref().and_then(|mq| mq.base_branch.clone()) else {
        lease.status(LandStatus::Bounced).await.ok();
        return land_bail(&workspace_root, "selfci config has no mq.base-branch", "");
    };
    if config
        .mq
        .as_ref()
        .is_some_and(|mq| mq.merge_mode == MergeMode::Merge)
    {
        lease.status(LandStatus::Bounced).await.ok();
        return land_bail(
            &workspace_root,
            "selfci merge-mode merge is not supported",
            "",
        );
    }

    eprintln!("landing {workspace_name}@ in {}", repo_root);
    lease.status(LandStatus::Preparing).await.ok();
    let prepared = match prepare_local_land(&workspace_root, &base_branch).await {
        Ok(prepared) => prepared,
        Err(error) => {
            lease.status(LandStatus::Bounced).await.ok();
            return land_bail(
                &workspace_root,
                "land prepare failed",
                &format!("{error:#}"),
            );
        }
    };
    let candidate = Candidate {
        commit_id: prepared.top_commit.clone(),
        change_id: prepared.top_change.clone(),
        display: format!("{workspace_name}@"),
    };
    lease.status(LandStatus::Checking).await.ok();
    let result = run_check(
        CheckOptions {
            job: &config.job,
            candidate_dir: &workspace_root,
            candidate,
        },
        |event| print_check_event(&event),
    )?;
    if !result.passed {
        lease.status(LandStatus::Bounced).await.ok();
        return land_bail(&workspace_root, "checks failed", &result.output);
    }

    lease.status(LandStatus::Publishing).await.ok();
    if let Err(error) = publish_local_land(&workspace_root, &prepared).await {
        lease.status(LandStatus::Bounced).await.ok();
        return land_bail(
            &workspace_root,
            "land publish failed",
            &format!("{error:#}"),
        );
    }
    let final_state = land_success_state(&workspace_root, &prepared).await;
    lease.status(LandStatus::Landed).await.ok();
    lease.release().await.ok();
    println!(
        "landed {} commit(s) on {}: {}",
        prepared.commits, prepared.base_branch, prepared.top_commit
    );
    match final_state {
        Ok(state) => {
            println!("{}: {}", prepared.base_branch, state.base);
            println!("working copy @: {}", state.working_copy);
        }
        Err(error) => {
            eprintln!("warning: could not read final jj state: {error:#}");
        }
    }
    Ok(())
}

struct LandLease {
    client: UiClient,
    repo: Utf8PathBuf,
    agent_id: Option<AgentId>,
}

impl LandLease {
    async fn acquire(
        repo: Utf8PathBuf,
        agent_id: Option<AgentId>,
        auth: &str,
        socket_path: Option<&Path>,
    ) -> Result<Self> {
        let socket_path = match socket_path {
            Some(path) => path.to_owned(),
            None => rho_daemon::default_socket_path()?,
        };
        let mut client = connect_or_start_daemon(&socket_path, auth).await?;
        eprintln!("queued for land lease");
        client
            .send(&ClientMessage::AcquireLandLease {
                repo: repo.clone(),
                agent_id,
            })
            .await
            .context("request land lease")?;
        loop {
            match client.recv().await.context("wait for land lease")? {
                ServerMessage::LandLeaseGranted { repo: granted } if granted == repo => {
                    eprintln!("land lease granted");
                    return Ok(Self {
                        client,
                        repo,
                        agent_id,
                    });
                }
                ServerMessage::LandLeaseQueued {
                    repo: queued,
                    holder,
                } if queued == repo => match holder {
                    Some(holder) => {
                        if let Some(pid) = holder.pid {
                            eprintln!(
                                "land lease held by pid {pid} uid {} gid {}; waiting",
                                holder.uid, holder.gid
                            );
                        } else {
                            eprintln!(
                                "land lease held by uid {} gid {}; waiting",
                                holder.uid, holder.gid
                            );
                        }
                    }
                    None => {
                        eprintln!("land lease is currently held; waiting");
                    }
                },
                ServerMessage::Error { message } => anyhow::bail!("{message}"),
                _ => {}
            }
        }
    }

    async fn status(&mut self, status: LandStatus) -> Result<()> {
        self.client
            .send(&ClientMessage::LandStatus {
                repo: self.repo.clone(),
                agent_id: self.agent_id,
                status,
            })
            .await
    }

    async fn release(&mut self) -> Result<()> {
        self.client
            .send(&ClientMessage::ReleaseLandLease {
                repo: self.repo.clone(),
                agent_id: self.agent_id,
            })
            .await
    }
}

struct LocalPreparedLand {
    base_branch: String,
    base_commit: String,
    top_commit: String,
    top_change: String,
    commits: usize,
}

struct LandSuccessState {
    base: String,
    working_copy: String,
}

async fn prepare_local_land(checkout: &Path, base_branch: &str) -> Result<LocalPreparedLand> {
    let wc_facts = jj_lines(
        checkout,
        "@",
        r#"commit_id ++ " " ++ change_id ++ " " ++ if(empty, "1", "0") ++ " " ++ if(description, "1", "0") ++ "\n""#,
        true,
    )
    .await
    .context("inspect working copy")?;
    let [wc_line] = wc_facts.as_slice() else {
        anyhow::bail!("expected one working-copy commit, got {wc_facts:?}");
    };
    let mut fields = wc_line.split_whitespace();
    let (Some(wc_commit), Some(wc_change), Some(wc_empty), Some(wc_described)) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        anyhow::bail!("malformed working-copy facts: {wc_line}");
    };
    let discardable = wc_empty == "1" && wc_described == "0";

    let (top_commit, top_change, needs_seal) = if discardable {
        let parents = jj_lines(
            checkout,
            "@-",
            r#"commit_id ++ " " ++ change_id ++ "\n""#,
            false,
        )
        .await
        .context("inspect working-copy parent")?;
        let [parent] = parents.as_slice() else {
            anyhow::bail!("cannot land from a merge working copy");
        };
        let (commit, change) = parent
            .split_once(' ')
            .with_context(|| format!("malformed parent facts: {parent}"))?;
        (commit.to_owned(), change.to_owned(), false)
    } else {
        (wc_commit.to_owned(), wc_change.to_owned(), true)
    };

    let base = jj_lines(
        checkout,
        &format!("present({base_branch})"),
        r#"commit_id ++ "\n""#,
        false,
    )
    .await
    .context("resolve base bookmark")?;
    let Some(base_commit) = base.first().cloned() else {
        anyhow::bail!("base bookmark does not exist: {base_branch}");
    };

    let range = format!("{base_commit}..{top_commit}");
    let set = jj_lines(checkout, &range, r#"commit_id ++ "\n""#, false)
        .await
        .context("compute land set")?;
    if set.is_empty() {
        anyhow::bail!("nothing to land");
    }

    let foreign = jj_lines(
        checkout,
        &format!("(({range}) & working_copies()) ~ {wc_commit}"),
        r#"change_id ++ "\n""#,
        false,
    )
    .await
    .context("scan for foreign working copies")?;
    if !foreign.is_empty() {
        anyhow::bail!(
            "land set contains foreign working copies: {}",
            foreign.join(", ")
        );
    }

    let undescribed = jj_lines(
        checkout,
        &format!(r#"({range}) & description(exact:"")"#),
        r#"change_id ++ "\n""#,
        false,
    )
    .await
    .context("scan for undescribed commits")?;
    if !undescribed.is_empty() {
        anyhow::bail!(
            "land set contains undescribed commits: {}",
            undescribed.join(", ")
        );
    }

    if needs_seal {
        run_jj(checkout, &["new"])
            .await
            .context("seal working copy")?;
    }

    run_jj(
        checkout,
        &[
            "rebase",
            "-s",
            &format!("roots({range})"),
            "-d",
            &base_commit,
        ],
    )
    .await
    .context("rebase onto base")?;

    let tops = jj_lines(checkout, &top_change, r#"commit_id ++ "\n""#, false)
        .await
        .context("resolve rebased top")?;
    let [top_commit] = tops.as_slice() else {
        anyhow::bail!("candidate change {top_change} is divergent: {tops:?}");
    };
    let top_commit = top_commit.clone();

    let conflicted = jj_lines(
        checkout,
        &format!("({base_commit}..{top_commit}) & conflicts()"),
        r#"change_id ++ "\n""#,
        false,
    )
    .await
    .context("scan for conflicts")?;
    if !conflicted.is_empty() {
        anyhow::bail!("rebase conflicted in changes: {}", conflicted.join(", "));
    }

    Ok(LocalPreparedLand {
        base_branch: base_branch.to_owned(),
        base_commit,
        top_commit,
        top_change,
        commits: set.len(),
    })
}

async fn publish_local_land(checkout: &Path, prepared: &LocalPreparedLand) -> Result<()> {
    let base = jj_lines(
        checkout,
        &format!("present({})", prepared.base_branch),
        r#"commit_id ++ "\n""#,
        true,
    )
    .await
    .context("re-resolve base bookmark")?;
    match base.first() {
        Some(commit) if *commit == prepared.base_commit => {}
        Some(commit) => {
            anyhow::bail!(
                "base bookmark moved during land: expected {}, found {}",
                prepared.base_commit,
                commit
            );
        }
        None => anyhow::bail!("base bookmark {} disappeared", prepared.base_branch),
    }

    let tops = jj_lines(
        checkout,
        &prepared.top_change,
        r#"commit_id ++ "\n""#,
        false,
    )
    .await
    .context("re-resolve candidate change")?;
    if tops.as_slice() != [prepared.top_commit.clone()] {
        anyhow::bail!(
            "candidate changed during land: checked {}, found {:?}",
            prepared.top_commit,
            tops.first()
        );
    }

    run_jj(
        checkout,
        &[
            "bookmark",
            "set",
            &prepared.base_branch,
            "-r",
            &prepared.top_commit,
        ],
    )
    .await
    .context("move base bookmark")?;
    Ok(())
}

async fn land_success_state(
    checkout: &Path,
    prepared: &LocalPreparedLand,
) -> Result<LandSuccessState> {
    let base = jj_lines(
        checkout,
        &format!("present({})", prepared.base_branch),
        r#"commit_id.short() ++ " " ++ description.first_line() ++ "\n""#,
        true,
    )
    .await
    .context("read landed bookmark state")?
    .into_iter()
    .next()
    .context("landed bookmark is missing")?;
    let working_copy = jj_lines(
        checkout,
        "@",
        r#"commit_id.short() ++ " " ++ if(empty, "(empty) ", "") ++ if(description, description.first_line(), "(no description)") ++ "\n""#,
        true,
    )
    .await
    .context("read working-copy state")?
    .into_iter()
    .next()
    .context("working copy is missing")?;
    Ok(LandSuccessState { base, working_copy })
}

async fn jj_lines(
    checkout: &Path,
    revset: &str,
    template: &str,
    snapshot: bool,
) -> Result<Vec<String>> {
    let mut args = Vec::new();
    if !snapshot {
        args.push("--ignore-working-copy");
    }
    args.extend(["log", "--no-graph", "-r", revset, "-T", template]);
    let stdout = run_jj(checkout, &args).await?;
    Ok(stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

async fn run_jj(checkout: &Path, args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new("jj")
        .current_dir(checkout)
        .args(args)
        .output()
        .await
        .with_context(|| format!("spawn jj {args:?}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "jj {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("jj output is not UTF-8")
}

fn current_jj_workspace(checkout: &Path) -> Result<String> {
    let output = jj_stdout(
        checkout,
        &[
            "log",
            "--no-graph",
            "-r",
            "@",
            "-T",
            r#"working_copies.join(",") ++ "\n""#,
        ],
    )
    .context("detect current jj workspace")?;
    let output = output.trim();
    if output.is_empty() {
        return current_jj_workspace_from_root(checkout);
    }
    let mut names = output.split(',').filter(|name| !name.is_empty());
    let name = names
        .next()
        .context("current commit has no working copy name")?;
    anyhow::ensure!(
        names.next().is_none(),
        "current commit has multiple working copies: {output}"
    );
    name.strip_suffix('@')
        .map(str::to_owned)
        .with_context(|| format!("malformed working copy name: {name}"))
}

fn current_agent_id() -> Result<Option<AgentId>> {
    let Some(id) = std::env::var_os("RHO_AGENT_ID") else {
        return Ok(None);
    };
    let id = id
        .into_string()
        .map_err(|_| anyhow::anyhow!("RHO_AGENT_ID is not valid UTF-8"))?;
    AgentId::from_encoded(&id)
        .map(Some)
        .context("parse RHO_AGENT_ID")
}

fn current_jj_workspace_from_root(checkout: &Path) -> Result<String> {
    let root = jj_stdout(checkout, &["root"])
        .context("detect jj workspace root")?
        .trim()
        .to_owned();
    let list = jj_stdout(
        checkout,
        &["workspace", "list", "-T", r#"name ++ " " ++ root ++ "\n""#],
    )
    .context("list jj workspaces")?;
    let mut matches = list.lines().filter_map(|line| {
        let (name, path) = line.split_once(' ')?;
        (path == root).then_some(name)
    });
    let name = matches
        .next()
        .context("could not find current workspace in jj workspace list")?;
    anyhow::ensure!(
        matches.next().is_none(),
        "multiple jj workspaces point at {root}"
    );
    Ok(name.to_owned())
}

fn jj_stdout(checkout: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("jj")
        .current_dir(checkout)
        .args(args)
        .output()
        .with_context(|| format!("spawn jj {args:?}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "jj {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("jj output is not UTF-8")
}

fn land_bail<T>(checkout: &Path, reason: &str, details: &str) -> Result<T> {
    let report = write_failure_report(checkout, reason, details)?;
    eprintln!(
        "land failed: {reason}\nreport: {} ({} lines)",
        report.path.display(),
        report.lines
    );
    if !report.tail.is_empty() {
        eprintln!("last {} line(s):", report.tail.len());
        for line in &report.tail {
            eprintln!("{line}");
        }
    }
    anyhow::bail!("{reason}")
}

struct FailureReport {
    path: PathBuf,
    lines: usize,
    tail: Vec<String>,
}

fn write_failure_report(checkout: &Path, reason: &str, details: &str) -> Result<FailureReport> {
    let dir = checkout.join(".rho").join("log");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(dir.join(".gitignore"), "/*\n")
        .with_context(|| format!("write {}", dir.join(".gitignore").display()))?;
    let path = dir.join("land-failure.txt");
    let mut body = String::new();
    body.push_str("rho land failed\n");
    body.push_str("reason: ");
    body.push_str(reason);
    body.push_str("\n\n");
    if !details.is_empty() {
        body.push_str(details);
        if !details.ends_with('\n') {
            body.push('\n');
        }
    }
    std::fs::write(&path, &body).with_context(|| format!("write {}", path.display()))?;
    let lines: Vec<String> = body.lines().map(str::to_owned).collect();
    let tail = lines
        .iter()
        .rev()
        .take(5)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Ok(FailureReport {
        path,
        lines: lines.len(),
        tail,
    })
}

fn print_check_event(event: &CheckEvent) {
    match event {
        CheckEvent::JobStarted { job } => eprintln!("check: started job {job}"),
        CheckEvent::StepStarted { job, step } => eprintln!("check: started step {job}/{step}"),
        CheckEvent::StepFinished {
            job,
            step,
            status,
            duration,
        } => eprintln!(
            "check: finished step {job}/{step} {status:?} ({:.3}s)",
            duration.as_secs_f64()
        ),
        CheckEvent::JobFinished {
            job,
            status,
            duration,
        } => eprintln!(
            "check: finished job {job} {status:?} ({:.3}s)",
            duration.as_secs_f64()
        ),
    }
}

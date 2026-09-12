//! Moving an agent that predates worksets into one, on the running
//! daemon: the mirror store, the octo transport and the database are all
//! the daemon's, so this is where a clone of the agent's repository can be
//! made and its record appended to.

use anyhow::Context as _;
use rho_agent::db::{AgentReadTxnExt as _, AgentWriteTxnExt as _};
use rho_fs_view::{WorksetMode, WorkspaceInfo};

use crate::Services;

/// Runs a command to completion for its trimmed stdout.
async fn output(mut command: tokio::process::Command) -> anyhow::Result<String> {
    let output = command
        .output()
        .await
        .with_context(|| format!("run {:?}", command.as_std()))?;
    anyhow::ensure!(
        output.status.success(),
        "{:?} failed ({}): {}",
        command.as_std(),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// Clones the repository's origin into a new workset, fetches the old jj
/// workspace's commits from the shared git store without materializing
/// anything, checks out the working copy's parent (detached) with the
/// working copy's changes staged, appends a `WorkdirMigrated` event that
/// replaces the agent's first workdir, and drops the loaded agent so its
/// next load reads the new place. The old workspace is left as it is.
/// Returns what was done, for a person.
pub(crate) async fn migrate_agent(
    services: &Services,
    agent: &str,
    origin: Option<String>,
    mode: WorksetMode,
) -> anyhow::Result<String> {
    let agent_id = services.resolve_display_agent_id(agent).await?;
    let head = services.db.read().get_agent(agent_id);
    let (repo, workspace) = match head.config.workdirs.first() {
        Some(WorkspaceInfo::Workspace { repo, id } | WorkspaceInfo::Sandbox { repo, id }) => {
            (repo.clone(), format!("ws-{}", id.encoded()))
        }
        Some(WorkspaceInfo::Workset { workset, .. }) => {
            anyhow::bail!("{agent} is already in workset {workset}")
        }
        Some(WorkspaceInfo::UserCheckout { repo }) => {
            anyhow::bail!("{agent} works in the user's own checkout {repo}; nothing to migrate")
        }
        None => anyhow::bail!("{agent} has no working directory"),
    };
    let worksets = services.pool.worksets();

    // The old workspace's commits, read from the repository without
    // materializing (or snapshotting) any checkout.
    let jj = |args: &[&str]| {
        let mut command = worksets.command("jj");
        command
            .arg("-R")
            .arg(&repo)
            .arg("--ignore-working-copy")
            .args(args);
        command
    };
    let log =
        |revset: String, template: &str| jj(&["log", "--no-graph", "-r", &revset, "-T", template]);
    let tip = output(log(format!("{workspace}@"), "commit_id")).await?;
    let base = output(log(format!("{workspace}@-"), "commit_id")).await?;
    anyhow::ensure!(
        tip.len() == 40 && base.len() == 40,
        "{workspace} in {repo} does not resolve to one working-copy commit and one parent"
    );
    let empty = output(log(format!("{workspace}@"), r#"if(empty, "1", "0")"#)).await? == "1";
    let description = output(log(format!("{workspace}@"), "description")).await?;
    let git_dir = output(jj(&["git", "root"])).await?;
    let origin = match origin {
        Some(origin) => origin,
        None => output(jj(&["git", "remote", "list"]))
            .await?
            .lines()
            .find_map(|line| line.strip_prefix("origin "))
            .map(|url| url.trim().to_owned())
            .with_context(|| format!("{repo} has no origin remote; pass --origin"))?,
    };
    let name = repo
        .file_name()
        .with_context(|| format!("{repo} has no name"))?
        .to_owned();

    let workset = worksets.create().await?;
    let checkout = workset.clone_repo(&origin, Some(&name)).await?;
    let git = |args: &[&str]| {
        let mut command = worksets.command(rho_fs_view::GIT);
        command.current_dir(&checkout).args(args);
        command
    };
    // Every commit the workspace had, straight from the shared git store:
    // the working copy is a commit there too, and the parent comes with it.
    output(git(&[
        "-c",
        "uploadpack.allowAnySHA1InWant=true",
        "fetch",
        "-q",
        &git_dir,
        &tip,
    ]))
    .await?;
    output(git(&["checkout", "-q", "--detach", &base])).await?;
    if !empty {
        // Tip's tree in the index and working tree, HEAD at the parent:
        // the working copy's changes, staged.
        output(git(&["reset", "-q", "--hard", &tip])).await?;
        output(git(&["reset", "-q", "--soft", &base])).await?;
    }

    let info = WorkspaceInfo::Workset {
        workset: workset.id().to_owned(),
        cwd: camino::Utf8PathBuf::from(rho_fs_view::MOUNT_ROOT).join(&name),
        mode,
        origin: Some(camino::Utf8PathBuf::from(&origin)),
    };
    let mut write = services.db.write().await;
    write.append_agent_event(
        agent_id,
        &rho_agent::AgentEvent::WorkdirMigrated {
            workdir: info,
            at: rho_core::UnixMs::now(),
        },
    );
    write.commit();
    let unloaded = services.pool.unload(agent_id).await;

    let mut report = format!(
        "{agent}: {workspace} in {repo} -> workset {} at {checkout}\n  origin {origin}\n  mode {mode:?}\n  checked out {base}\n",
        workset.id()
    );
    if !empty {
        report.push_str(&format!("  working copy {tip} staged on top\n"));
    }
    if !description.trim().is_empty() {
        report.push_str(&format!(
            "  the working copy's description was not carried over:\n    {}\n",
            description.trim().replace('\n', "\n    ")
        ));
    }
    if unloaded {
        report.push_str("  the loaded agent was dropped; reselect it to load the new place\n");
    }
    Ok(report)
}

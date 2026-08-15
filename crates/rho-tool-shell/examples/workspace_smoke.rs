//! End-to-end smoke test for clone-store worksets and namespaced shell tools.
//! Run with `cargo run -p rho-tool-shell --example workspace_smoke`.

use std::time::Duration;

use rho_core::{ToolCall, ToolCallId, ToolName, ToolType};
use rho_tool_shell::{EXEC_COMMAND_TOOL_NAME, ShellTools};
use rho_workset::{Mode, PathOverrides, UserEnvironment, Worksets};

fn shell_call(command: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from("call-1").unwrap(),
        name: ToolName::try_from(EXEC_COMMAND_TOOL_NAME).unwrap(),
        tool_type: ToolType::Function,
        arguments: serde_json::json!({ "command": command }).to_string(),
    }
}

fn run(command: &mut std::process::Command) -> anyhow::Result<()> {
    let status = command.status()?;
    anyhow::ensure!(status.success(), "command failed: {command:?}");
    Ok(())
}

fn view_roots() -> anyhow::Result<std::collections::BTreeSet<std::path::PathBuf>> {
    Ok(std::fs::read_dir(std::env::temp_dir())?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("rho-workset-view-")
        })
        .map(|entry| entry.path())
        .collect())
}

fn main() -> anyhow::Result<()> {
    // SAFETY: called before the runtime starts any worker threads.
    unsafe { rho_workset::init_daemon_namespace()? };
    tokio::runtime::Runtime::new()?.block_on(async {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        std::fs::create_dir(&source)?;
        run(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&source))?;
        std::fs::write(source.join("file.txt"), "origin\n")?;
        run(std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&source))?;
        run(std::process::Command::new("git")
            .args([
                "-c",
                "user.name=Rho Smoke",
                "-c",
                "user.email=rho@example.invalid",
                "commit",
                "-qm",
                "initial",
            ])
            .current_dir(&source))?;

        let mut environment_vars: Vec<_> = std::env::vars_os().collect();
        if let Some((_, jj)) = environment_vars
            .iter_mut()
            .find(|(name, _)| name == "RHO_JJ")
            && std::path::Path::new(jj).is_relative()
        {
            *jj = std::fs::canonicalize(&*jj)?.into_os_string();
        }
        let environment = UserEnvironment::new(environment_vars);
        let direnv = std::process::Command::new("which").arg("direnv").output()?;
        let direnv = String::from_utf8(direnv.stdout)?;
        let direnv = std::fs::canonicalize(direnv.trim())?;
        let path_overrides = PathOverrides {
            before: vec![direnv.parent().unwrap().to_owned()],
            after: Vec::new(),
        };
        let worksets = Worksets::open(
            temp.path().join("rho"),
            rho_db::RhoDb::open(temp.path().join("rho.redb")),
            environment,
            path_overrides,
        )?;
        let parent_set = worksets.create().await?;
        let parent = parent_set
            .clone("source", source.to_str().unwrap(), Some("project"), None)
            .await?;
        std::fs::write(parent.checkout().join("file.txt"), "parent\n")?;

        let child_set = worksets.create().await?;
        let view_roots_before = view_roots()?;
        let _child = child_set.fork_from(&parent, Some("project")).await?;
        let namespace = child_set
            .enter(Mode::View {
                home_skeleton: None,
            })
            .await?;
        let host_probe = temp.path().join("host-probe");
        std::fs::write(&host_probe, "host\n")?;
        anyhow::ensure!(
            tokio::task::spawn_blocking(move || host_probe.is_file()).await?,
            "namespace construction contaminated a runtime blocking-pool thread"
        );
        let second = child_set
            .clone("source", source.to_str().unwrap(), Some("second"), None)
            .await?;
        namespace
            .refresh(rho_workset::Mounts {
                stores: Vec::new(),
                workspaces: vec![rho_workset::WorkspaceMount {
                    name: "second".to_owned(),
                    source: second.checkout().as_std_path().to_owned(),
                }],
            })
            .await?;
        let mut refreshed_pwd = tokio::process::Command::new("pwd");
        namespace.prepare_command(
            &mut refreshed_pwd,
            Some(camino::Utf8Path::new("/src/second")),
        )?;
        let refreshed_pwd = refreshed_pwd.output().await?;
        anyhow::ensure!(refreshed_pwd.status.success());
        anyhow::ensure!(
            String::from_utf8(refreshed_pwd.stdout)?.trim() == "/src/second",
            "namespace refresh did not mount the new checkout"
        );
        let tools = ShellTools::new(Duration::from_secs(30), namespace);
        let result = tools
            .call(shell_call("test \"$PWD\" = /src/project && cat file.txt"))
            .await;
        print!("{}", result.output);
        anyhow::ensure!(
            result.output.contains("parent"),
            "forked content was not visible"
        );
        drop(tools);
        anyhow::ensure!(
            view_roots()? == view_roots_before,
            "dropping the namespace leaked a rho-workset-view directory"
        );
        Ok(())
    })
}

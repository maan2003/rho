//! End-to-end test for `rho agent` against a real `rho-agent-host`, real
//! `rho-agent-worker`, and `rho-fake-model`.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::Context as _;
use serde_json::Value;

const WORKER: &str = "rho-agent-worker";

fn main() -> anyhow::Result<()> {
    // The test binary doubles as the workset worker under this name.
    if std::env::args_os()
        .next()
        .is_some_and(|program| Path::new(&program).ends_with(WORKER))
    {
        return rho_inference::worker_main();
    }
    if std::env::args().any(|arg| arg == "--list") {
        if !std::env::args().any(|arg| arg == "--ignored") {
            println!("e2e: test");
        }
        return Ok(());
    }

    let rig = tempfile::tempdir()?;
    let root = rig.path();
    let worker = root.join(WORKER);
    std::os::unix::fs::symlink(std::env::current_exe()?, &worker)?;

    // Isolate XDG directories before starting the async runtime / host.
    // SAFETY: top of main; single-threaded.
    unsafe {
        std::env::set_var("RHO_AGENT_WORKER", &worker);
        std::env::set_var("XDG_STATE_HOME", root.join("state"));
        std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
        std::env::set_var("XDG_CACHE_HOME", root.join("cache"));
        std::env::set_var("XDG_DATA_HOME", root.join("data"));
        std::env::set_var("SHELL", "bash");
    }

    tokio::runtime::Runtime::new()?.block_on(run_e2e(root))
}

async fn run_e2e(root: &Path) -> anyhow::Result<()> {
    let runtime_dir = root.join("run");
    let paths = rho_rpc::protocol::RuntimePaths::new(Some(&runtime_dir))?;
    let auth = root.join("state/rho/auth.d");
    std::fs::create_dir_all(&auth)?;
    std::fs::write(
        auth.join("default.json"),
        serde_json::to_vec(&serde_json::json!({
            "access_token": "rho-agent-cli-test",
            "expires_at_ms": u64::MAX,
            "account_id": "rho-agent-cli-test",
            "client_secret": vec![0u8; 32],
        }))?,
    )?;

    let workdir = root.join("workdir");
    std::fs::create_dir_all(&workdir)?;
    let policy_dir = root.join("config/rho");
    std::fs::create_dir_all(&policy_dir)?;
    std::fs::write(
        policy_dir.join("init.py"),
        r#"
import _rho
from rho import hooks
from rho.default import on_notebook as _default_on_notebook

class _MailCompat:
    def __init__(self, agent_id):
        self._agent = agent_id

    def send(self, to, text, *, thread=None, reply_to=None, ping=None, replace_last=False):
        _rho.send_chat(self._agent, int(thread), "working...", "flash")
        return _rho.send_chat(self._agent, int(thread), str(text).strip(), "message")

def _on_notebook(agent, ns):
    _default_on_notebook(agent, ns)
    if "mail" not in ns:
        ns["mail"] = _MailCompat(agent.id)

hooks.on_notebook = _on_notebook
"#,
    )?;

    let mut config = rho_fake_model::FakeModelConfig::seeded(77);
    config.scenario = rho_fake_model::Scenario::ClarifyingQuestion;
    config.distribution.rate_limit_bps = 0;
    config.distribution.usage_limit_bps = 0;
    config.distribution.overload_bps = 0;
    config.distribution.disconnect_bps = 0;
    let model = rho_fake_model::FakeModel::start(config).await?;

    let host_task = tokio::spawn(rho_agent_host::run(rho_agent_host::HostArgs {
        runtime_dir: Some(paths.directory().to_owned()),
        claude_config_dir: Some(camino::Utf8PathBuf::from_path_buf(root.join("claude")).unwrap()),
        openai_base_url: Some(model.openai_base_url()),
        anthropic_base_url: None,
        cpu_profile: None,
        extra_before_path: None,
        extra_after_path: None,
        handoff: None,
    }));

    let result = tokio::time::timeout(Duration::from_secs(60), async {
        wait_for_host(&paths).await?;
        exercise_cli(&runtime_dir, &workdir).await
    })
    .await
    .context("rho agent e2e timed out")?;

    if result.is_err() {
        eprintln!("fake model metrics: {:?}", model.metrics());
        if let Ok((out, _)) = rho_cli(&runtime_dir, &["list"], None).await {
            eprintln!("agents:\n{out}");
            if let Some(line) = out.lines().nth(1)
                && let Some(agent) = line.split('\t').next()
                && let Ok((tr, _)) = rho_cli(
                    &runtime_dir,
                    &["transcript", agent, "--from", "0", "--last", "0"],
                    None,
                )
                .await
            {
                let head: Vec<_> = tr.lines().take(25).collect();
                eprintln!("transcript {agent} head:\n{}", head.join("\n"));
            }
        }
        if let Ok((out, _)) = rho_cli(&runtime_dir, &["read", "--thread", "2"], None).await {
            eprintln!("thread 2:\n{out}");
        }
    }

    host_task.abort();
    result?;
    assert!(
        model.metrics().requests >= 3,
        "expected at least 3 fake model requests, got {}",
        model.metrics().requests
    );
    println!("agent_e2e passed");
    Ok(())
}

async fn wait_for_host(paths: &rho_rpc::protocol::RuntimePaths) -> anyhow::Result<()> {
    for _ in 0..200 {
        if let Ok(mut client) = rho_rpc::protocol::client::Client::connect(paths).await {
            let _ = client.shutdown().await;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    anyhow::bail!("agent host did not become ready")
}

async fn rho_cli(
    runtime_dir: &Path,
    args: &[&str],
    stdin_data: Option<&str>,
) -> anyhow::Result<(String, String)> {
    let bin = env!("CARGO_BIN_EXE_rho").to_owned();
    let runtime = runtime_dir.to_owned();
    let owned_args = args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let stdin_owned = stdin_data.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(&bin);
        cmd.arg("agent")
            .arg("--runtime-dir")
            .arg(&runtime)
            .args(&owned_args);
        if stdin_owned.is_some() {
            cmd.stdin(std::process::Stdio::piped());
        } else {
            cmd.stdin(std::process::Stdio::null());
        }
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().context("spawn rho binary")?;
        if let Some(input) = stdin_owned {
            use std::io::Write as _;
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(input.as_bytes())?;
        }
        let output = child.wait_with_output()?;
        let stdout = String::from_utf8(output.stdout)?;
        let stderr = String::from_utf8(output.stderr)?;
        anyhow::ensure!(
            output.status.success(),
            "`rho agent {}` failed ({}):\nstdout: {stdout}\nstderr: {stderr}",
            owned_args.join(" "),
            output.status
        );
        Ok((stdout, stderr))
    })
    .await?
}

async fn rho_json(
    runtime_dir: &Path,
    args: &[&str],
    stdin_data: Option<&str>,
) -> anyhow::Result<Value> {
    let mut full = Vec::with_capacity(args.len() + 1);
    full.push("--json");
    full.extend_from_slice(args);
    let (stdout, _) = rho_cli(runtime_dir, &full, stdin_data).await?;
    serde_json::from_str(stdout.trim())
        .with_context(|| format!("parse JSON from `rho agent {}`: {stdout}", args.join(" ")))
}

async fn exercise_cli(runtime_dir: &Path, workdir: &Path) -> anyhow::Result<()> {
    // 1. Spawn on sol-medium in a host cwd without --wait.
    let cwd_str = workdir.to_str().unwrap();
    let spawned = rho_json(
        runtime_dir,
        &[
            "spawn",
            "--runtime",
            "sol-medium",
            "--cwd",
            cwd_str,
            "--name",
            "worker-one",
            "Turn 1 task",
        ],
        None,
    )
    .await?;
    let agent_handle = spawned["agent"]
        .as_str()
        .context("missing agent")?
        .to_owned();
    let agent_id = spawned["agent_id"]
        .as_str()
        .context("missing agent_id")?
        .to_owned();
    let workset_id = spawned["workset"]
        .as_str()
        .context("missing workset")?
        .to_owned();
    let thread_id = spawned["thread"].as_u64().context("missing thread")?;
    let cursor1 = spawned["cursor"].as_u64().context("missing cursor")?;
    assert_eq!(spawned["runtime"], "sol-medium");
    assert_eq!(spawned["cwd"], cwd_str);

    // 2. Receive the first reply using `--thread` and `--after <cursor>`.
    let thread_str = thread_id.to_string();
    let cursor1_str = cursor1.to_string();
    let reply1 = rho_json(
        runtime_dir,
        &[
            "recv",
            "--thread",
            &thread_str,
            "--after",
            &cursor1_str,
            "--include-flashes",
            "--timeout",
            "30",
        ],
        None,
    )
    .await?;
    let reply1_text = reply1["text"].as_str().context("missing reply1 text")?;
    assert!(
        reply1_text.contains("clarify"),
        "unexpected reply1 text: {reply1_text}"
    );
    let flashes1 = reply1["flashes"].as_array().context("missing flashes1")?;
    assert_eq!(flashes1.len(), 1);
    assert_eq!(flashes1[0]["text"], "working...");
    let cursor2 = reply1["cursor"].as_u64().context("missing reply1 cursor")?;
    assert!(cursor2 > cursor1);

    // Calling recv without --after returns the already-arrived latest reply.
    let caught_up = rho_json(
        runtime_dir,
        &["recv", "--thread", &thread_str, "--timeout", "0"],
        None,
    )
    .await?;
    assert_eq!(caught_up["cursor"], cursor2);
    assert_eq!(caught_up["text"], reply1_text);

    // 3. List agents and verify our agent shows up with expected metadata.
    let list = rho_json(runtime_dir, &["list"], None).await?;
    let agents = list["agents"].as_array().context("missing agents array")?;
    let found = agents
        .iter()
        .find(|entry| entry["agent_id"] == agent_id)
        .context("spawned agent not in list")?;
    assert_eq!(found["workset"], workset_id);
    assert_eq!(found["runtime"], "sol-medium");
    assert_eq!(found["cwd"], cwd_str);
    assert_eq!(found["name"], "worker-one");

    // 4. Status using short prefix!
    let short_prefix = &agent_id[..6];
    let status = rho_json(runtime_dir, &["status", short_prefix], None).await?;
    assert_eq!(status["agent"], agent_handle);
    assert_eq!(status["threads"], serde_json::json!([thread_id]));

    // 5. Send a follow-up via `--agent` (reusing its thread) + `--stdin` +
    //    `--wait`.
    let reply2 = rho_json(
        runtime_dir,
        &[
            "send",
            "--agent",
            &agent_handle,
            "--stdin",
            "--wait",
            "--timeout",
            "30",
        ],
        Some("Turn 2 clarification from stdin"),
    )
    .await?;
    assert_eq!(reply2["thread"], thread_id);
    assert!(reply2["cursor"].as_u64().unwrap() > cursor2);
    assert!(reply2["text"].as_str().unwrap().contains("clarify"));

    // 6. Read thread messages non-blocking (flashes excluded by default,
    //    included with flag).
    let read = rho_json(runtime_dir, &["read", "--thread", &thread_str], None).await?;
    let msgs = read["messages"].as_array().context("missing messages")?;
    assert_eq!(
        msgs.len(),
        4,
        "expected 4 kept messages, got {}",
        msgs.len()
    );
    assert_eq!(msgs[0]["text"], "Turn 1 task");
    assert_eq!(msgs[2]["text"], "Turn 2 clarification from stdin");
    let read_flashes = rho_json(
        runtime_dir,
        &["read", "--thread", &thread_str, "--include-flashes"],
        None,
    )
    .await?;
    assert_eq!(read_flashes["messages"].as_array().unwrap().len(), 6);

    // 7. Read transcript via AgentLog RPC.
    let transcript = rho_json(runtime_dir, &["transcript", &agent_handle], None).await?;
    let events = transcript["events"].as_array().context("missing events")?;
    assert!(
        events.iter().any(|e| e["kind"] == "replied"),
        "expected replied event in transcript: {events:?}"
    );
    assert!(
        events.iter().any(|e| e["kind"] == "mail_written"),
        "expected mail_written event in transcript: {events:?}"
    );

    // 8. Cancel (idle -> was_asking: false) & stop with --delete-workset.
    let cancel = rho_json(runtime_dir, &["cancel", &agent_handle], None).await?;
    assert_eq!(cancel["was_asking"], false);
    let stopped = rho_json(
        runtime_dir,
        &["stop", &agent_handle, "--delete-workset"],
        None,
    )
    .await?;
    assert_eq!(stopped["stopped"], true);
    assert_eq!(stopped["deleted_workset"], true);

    // 9. Plain-text `spawn --wait --include-flashes` in an isolated workset.
    let (plain_out, plain_err) = rho_cli(
        runtime_dir,
        &[
            "spawn",
            "--runtime",
            "astra-high",
            "--isolated",
            "--wait",
            "--include-flashes",
            "--timeout",
            "30",
            "Plain text turn",
        ],
        None,
    )
    .await?;
    assert!(plain_out.contains("clarify"), "plain stdout: {plain_out}");
    assert!(
        plain_err.contains("agent=eng-"),
        "plain stderr: {plain_err}"
    );
    assert!(
        plain_err.contains("[flash #"),
        "expected flash on stderr: {plain_err}"
    );

    let list_after = rho_json(runtime_dir, &["list"], None).await?;
    let remaining = list_after["agents"].as_array().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0]["runtime"], "astra-high");
    assert_eq!(remaining[0]["cwd"], "/src");

    Ok(())
}

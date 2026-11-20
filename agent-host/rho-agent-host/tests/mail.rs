//! Drive the real agent host, a real worker and the local chat protocol.
//! Only the inference provider and account are fake.
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::Context as _;
use rho_agent_types::{
    Address, ChatEvent, ChatRecord, ContentPart, MentionKind, MessageId, Stop, WorksetKind,
};
use rho_agents_client::protocol::worksets::{CreateWorkset, Eval, EvalResult};
use rho_chat::protocol::{ClientFrame, Leave, Open, Post, Project, ResolveProject, ServerFrame};
use rho_rpc::protocol::client::{call, connect};
use rho_rpc::protocol::{RuntimePaths, read_frame, write_frame, write_open};

// The test doubles as the worker under this name, so the agent runs the
// same build's worker protocol without a prebuilt binary in target/debug.
const WORKER: &str = "rho-agent-worker";

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

struct Host(Child);

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn main() -> anyhow::Result<()> {
    // `worker_main` creates its own runtime and must run before this one.
    if std::env::args_os()
        .next()
        .is_some_and(|program| Path::new(&program).ends_with(WORKER))
    {
        return rho_inference::worker_main();
    }
    // Expose the single case to nextest, like terminal_stream.
    if std::env::args().any(|arg| arg == "--list") {
        if !std::env::args().any(|arg| arg == "--ignored") {
            println!("e2e: test");
        }
        return Ok(());
    }
    tokio::runtime::Runtime::new()?.block_on(mail_through_a_real_host_and_fake_model())
}

async fn mail_through_a_real_host_and_fake_model() -> anyhow::Result<()> {
    let rig = tempfile::tempdir()?;
    let root = rig.path();
    let paths = RuntimePaths::new(Some(root.join("run")))?;
    let auth = root.join("rho/auth.d");
    std::fs::create_dir_all(&auth)?;
    std::fs::write(
        auth.join("default.json"),
        serde_json::to_vec(&serde_json::json!({
            "access_token": "rho-mail-test",
            "expires_at_ms": u64::MAX,
            "account_id": "rho-mail-test",
            "client_secret": vec![0u8; 32],
        }))?,
    )?;
    let mut config = rho_fake_model::FakeModelConfig::seeded(51);
    config.scenario = rho_fake_model::Scenario::RealToolRounds;
    config.real_tool_rounds = 1;
    config.distribution.rate_limit_bps = 0;
    config.distribution.usage_limit_bps = 0;
    config.distribution.overload_bps = 0;
    config.distribution.disconnect_bps = 0;
    let model = rho_fake_model::FakeModel::start(config).await?;
    let host_binary = Path::new(env!("CARGO_BIN_EXE_rho-agent-host"));
    let worker = root.join(WORKER);
    std::os::unix::fs::symlink(std::env::current_exe()?, &worker)?;
    let mut host = Host(
        Command::new(host_binary)
            .arg("--runtime-dir")
            .arg(paths.directory())
            .arg("--claude-config-dir")
            .arg(root.join("claude"))
            .args(["--openai-base-url", &model.openai_base_url()])
            .env("RHO_AGENT_WORKER", worker)
            .env("XDG_STATE_HOME", root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("SHELL", "bash")
            .stdout(Stdio::null())
            .spawn()?,
    );
    let result = tokio::time::timeout(Duration::from_secs(45), run(&paths, root))
        .await
        .context("mail session did not finish")??;
    assert!(
        model.metrics().requests >= 2,
        "the model must see both the brief and real tool output"
    );
    println!("{result}");
    if let Some(pid) = rustix::process::Pid::from_raw(host.0.id() as i32) {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    }
    let _ = host.0.wait();
    Ok(())
}

async fn run(paths: &RuntimePaths, root: &Path) -> anyhow::Result<String> {
    let mut stream = loop {
        if let Ok(stream) = connect(paths).await {
            break stream;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    };
    write_open(&mut stream, &Open::Session).await?;
    let ServerFrame::Hello { head, .. } = read_frame(&mut stream).await? else {
        anyhow::bail!("no chat hello");
    };
    write_frame(&mut stream, &ClientFrame::Follow { since: head }).await?;
    let project = post(paths, 7, None, "tide").await?;
    let thread = post(paths, 8, Some(project), "Check the real mail path.").await?;
    write_frame(
        &mut stream,
        &ClientFrame::View {
            thread: Some(thread),
        },
    )
    .await?;
    let address = call(
        paths,
        ResolveProject {
            name: " tide ".to_owned(),
        },
    )
    .await?;
    assert_eq!(
        address,
        Some(Project {
            name: "tide".to_owned(),
            channel: project
        })
    );

    let created = call(paths, CreateWorkset {
        kind: WorksetKind::Host { cwd: root.to_str().unwrap().into() },
        code: format!(
            r#"new_agent(runtime={{"kind": "native", "model": "sol", "effort": "medium"}}, origin="chat:{}")"#,
            thread.0.0
        ),
    }).await?;
    let agent = created
        .result
        .agent
        .with_context(|| format!("agent not created: {:?}", created.result.error))?;
    call(
        paths,
        Post {
            client_id: 9,
            parent: Some(thread),
            reply_to: None,
            mentions: vec![agent],
            content: vec![ContentPart::Text {
                text: "Please run the check.".into(),
            }],
        },
    )
    .await?;
    let origin_result = wait_for_mail(&mut stream, |row| matches!(&row.event,
        ChatEvent::Posted { parent: Some(parent), from: Address::Agent(from), mentions, .. }
        if *parent == thread && *from == agent && mentions.iter().any(|m| m.to == Address::User && m.kind == MentionKind::Result)
    )).await?;
    let EvalResult { output, error, .. } = call(
        paths,
        Eval {
            workset: created.workset,
            code: format!(
                r#"
import os
from rho.tools import Chat
mail = Chat("{}")
mail.status({}, "Waiting for the capacity report")
note = await mail.post({}, "Use polling for this check.")
assert type(note) is int
mail.edit(note, "Use sockets for this check.")
hits = await mail.search("sockets", project={})
assert [row["message"] for row in hits] == [note]
mail.delete(note)
assert not mail.search("sockets", project={})
print(f"worker:{{os.getpid()}} note:{{note}}")
"#,
                agent.encoded(),
                thread.0.0,
                thread.0.0,
                project.0.0,
                project.0.0
            ),
        },
    )
    .await?;
    anyhow::ensure!(
        error.is_none(),
        "notebook calls failed: {error:?}\n{output}"
    );
    let marker = output.lines().last().context("no worker marker")?;
    let (worker, note) = marker
        .strip_prefix("worker:")
        .and_then(|text| text.split_once(" note:"))
        .context("invalid worker marker")?;
    let worker = rustix::process::Pid::from_raw(worker.parse()?).context("invalid worker pid")?;
    let note = MessageId(rho_agent_types::ChatSeq(note.parse()?));
    let mut edited = false;
    let mut deleted = false;
    let mut status = false;
    while !(edited && deleted && status) {
        match read_frame(&mut stream).await? {
            ServerFrame::Statuses { statuses } => {
                status |= statuses
                    .iter()
                    .any(|s| s.thread == thread && s.text == "Waiting for the capacity report")
            }
            ServerFrame::Chat { records } => {
                for row in records {
                    match row.event {
                        ChatEvent::Edited { message, .. } if message == note => edited = true,
                        ChatEvent::Deleted { message } if message == note => deleted = true,
                        _ => {}
                    }
                }
            }
            ServerFrame::Hello { .. } => {}
        }
    }
    write_frame(&mut stream, &ClientFrame::View { thread: None }).await?;
    rustix::process::kill_process(worker, rustix::process::Signal::KILL)?;
    let mut stop_mail = None;
    let mut status_cleared = false;
    let stopped = loop {
        match read_frame(&mut stream).await? {
            ServerFrame::Chat { records } => {
                stop_mail = stop_mail.or_else(|| {
                    records.into_iter().find(|row| {
                        matches!(
                            row.event, ChatEvent::Posted {
                                parent: Some(parent),
                                from: Address::System { agent: stopped, stop: Stop::Lost, .. },
                                ..
                            } if parent == thread && stopped == agent
                        )
                    })
                });
            }
            ServerFrame::Statuses { statuses } => {
                status_cleared = !statuses.iter().any(|status| status.thread == thread);
            }
            ServerFrame::Hello { .. } => {}
        }
        if status_cleared && let Some(mail) = stop_mail.take() {
            break mail;
        }
    };
    let ChatEvent::Posted {
        mentions, content, ..
    } = stopped.event
    else {
        unreachable!()
    };
    assert!(
        mentions
            .iter()
            .any(|m| m.to == Address::User && m.kind == MentionKind::Ask)
    );
    let text = rho_agent_types::content_text(&content);
    assert!(text.starts_with("lost · "), "{text}");
    assert!(text.contains("Last transcript lines:"), "{text}");

    // The user doesn't address an agent. The host delivers to the original
    // writer, even after the writer leaves.
    call(paths, Leave { thread, agent }).await?;
    let reply_to = MessageId(origin_result.seq);
    let answer = call(
        paths,
        Post {
            client_id: 10,
            parent: Some(thread),
            reply_to: Some(reply_to),
            mentions: vec![],
            content: vec![ContentPart::Text {
                text: "Thanks for checking.".into(),
            }],
        },
    )
    .await?;
    let received = wait_for_mail(&mut stream, |row| row.seq == answer.0).await?;
    let ChatEvent::Posted {
        parent,
        reply_to: target,
        mentions,
        ..
    } = received.event
    else {
        anyhow::bail!("the answer must be durable project mail")
    };
    assert_eq!((parent, target), (Some(thread), Some(reply_to)));
    assert_eq!(
        mentions,
        [rho_agent_types::Mention {
            to: Address::Agent(agent),
            kind: MentionKind::Ask
        }]
    );
    Ok(format!(
        "mail wire: project {}, fake model result #{}, status set/cleared, edit/delete #{}, lost stop #{}, and addressed reply #{}",
        project.0.0, origin_result.seq.0, note.0.0, stopped.seq.0, answer.0.0
    ))
}

async fn post(
    paths: &RuntimePaths,
    client_id: u64,
    parent: Option<MessageId>,
    text: &str,
) -> anyhow::Result<MessageId> {
    call(
        paths,
        Post {
            client_id,
            parent,
            reply_to: None,
            mentions: vec![],
            content: vec![ContentPart::Text { text: text.into() }],
        },
    )
    .await
}

async fn wait_for_mail(
    stream: &mut rho_rpc::Stream,
    predicate: impl Fn(&ChatRecord) -> bool,
) -> anyhow::Result<ChatRecord> {
    loop {
        if let ServerFrame::Chat { records } = read_frame(stream).await?
            && let Some(found) = records.into_iter().find(&predicate)
        {
            return Ok(found);
        }
    }
}

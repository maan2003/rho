//! Drive the real agent host, a real worker and the local chat protocol.
//! Only the inference provider and account are fake.
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::Context as _;
use rho_agent_types::{
    Address, AgentId, ChatEvent, ChatRecord, ContentPart, MessageId, Role, Stop, WorksetKind, Wpos,
};
use rho_agents_client::protocol::worksets::{
    AgentLog, CreateWorkset, Eval, EvalResult, LogEvent, MailLaunch, RestartWorkset, RewindAgent,
    StartFromMail, StopAgent,
};
use rho_mail::protocol::{ClientFrame, Leave, Open, Post, Project, ResolveProject, ServerFrame};
use rho_mail::{HostMail, cards};
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
    tokio::runtime::Runtime::new()?.block_on(async {
        with_host(rho_fake_model::Scenario::RealToolRounds).await?;
        with_host(rho_fake_model::Scenario::MailChain).await
    })
}

async fn with_host(scenario: rho_fake_model::Scenario) -> anyhow::Result<()> {
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
    config.scenario = scenario;
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
    let result = tokio::time::timeout(Duration::from_secs(55), async {
        if scenario == rho_fake_model::Scenario::MailChain {
            run_peer_chain(&paths, root).await
        } else {
            let refused = run_refused_start(&paths, root).await?;
            let mail = run(&paths, root).await?;
            let live = run_live_ask(&paths, root).await?;
            let recovery = run_startup_failure(&paths, root).await?;
            anyhow::Ok(format!("{refused}\n{mail}\n{live}\n{recovery}"))
        }
    })
    .await
    .with_context(|| format!("{} session did not finish", scenario.as_str()))??;
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

async fn run_refused_start(paths: &RuntimePaths, root: &Path) -> anyhow::Result<String> {
    let mut stream = connect_chat(paths).await?;
    let ServerFrame::Hello { head, host } = read_frame(&mut stream).await? else {
        anyhow::bail!("no mail hello");
    };
    write_frame(&mut stream, &ClientFrame::Follow { since: head }).await?;
    let project = post(paths, 100, None, "reef").await?;
    let thread = post(paths, 101, Some(project), "Check the tide clock.").await?;
    let missing = root.join("does-not-exist");
    let error = call(
        paths,
        StartFromMail {
            origin: thread,
            launch: MailLaunch::New(CreateWorkset {
                kind: WorksetKind::Host {
                    cwd: missing.to_str().unwrap().into(),
                },
                code: "raise AssertionError('directory check did not run')".into(),
            }),
            host_label: "lab".into(),
        },
    )
    .await
    .expect_err("the requested directory is missing");
    let reason = format!("{} does not exist on lab", missing.display());
    assert_eq!(error.to_string(), reason);
    let mut history = HostMail::default();
    let mail = loop {
        let ServerFrame::Mail { records } = read_frame(&mut stream).await? else {
            continue;
        };
        let mut failure = None;
        for record in records {
            history.tell(&record);
            failure = failure.or_else(|| {
                rho_agent_types::Message::from_record(&record)
                    .filter(|mail| mail.from == Address::StartFailed)
            });
        }
        if let Some(mail) = failure {
            break mail;
        }
    };
    assert_eq!(mail.parent, Some(thread));
    assert_eq!(mail.reply_to, Some(thread));
    assert_eq!(
        mail.text(),
        format!(
            "failed · did not start · {reason}\n\nTo carry on, run start agent here and choose where reef's agents work."
        )
    );
    let claims = cards::claims(
        &cards::Host {
            endpoint: host,
            chat: &history,
            name: "lab",
        },
        &rho_dealer::Marks::default(),
        jiff::Timestamp::now(),
        None,
    );
    let [claim] = claims.as_slice() else {
        anyhow::bail!("refused start did not deal one card")
    };
    assert_eq!(cards::attention_mail(&claim.cursor), Some(mail.id));
    assert_eq!(claim.title, "Check the tide clock.");
    Ok(format!(
        "a missing directory refused the start and dealt stop mail #{}",
        mail.id.0.0
    ))
}

async fn run_live_ask(paths: &RuntimePaths, root: &Path) -> anyhow::Result<String> {
    let mut stream = connect_chat(paths).await?;
    let ServerFrame::Hello { head, .. } = read_frame(&mut stream).await? else {
        anyhow::bail!("no mail hello");
    };
    write_frame(&mut stream, &ClientFrame::Follow { since: head }).await?;
    let project = post(paths, 110, None, "lighthouse").await?;
    let thread = post(paths, 111, Some(project), "Check the clock offsets.").await?;
    let make_agent = || async {
        let created = call(
            paths,
            CreateWorkset {
                kind: WorksetKind::Host {
                    cwd: root.to_str().unwrap().into(),
                },
                code:
                    r#"new_agent(runtime={"kind": "native", "model": "sol", "effort": "medium"})"#
                        .into(),
            },
        )
        .await?;
        let agent = created.result.agent.context("no agent")?;
        anyhow::Ok((agent, created.workset))
    };
    let (a, aw) = make_agent().await?;
    let (b, bw) = make_agent().await?;
    let eval = |workset, code| async move {
        let result = call(paths, Eval { workset, code }).await?;
        anyhow::ensure!(result.error.is_none(), "{:?}", result.error);
        Ok::<_, anyhow::Error>(result.output)
    };
    for (agent, workset) in [(a, aw), (b, bw)] {
        eval(
            workset,
            format!(
                "from rho import mail\nmail.join({}, {:?})",
                thread.0.0,
                agent.encoded()
            ),
        )
        .await?;
    }
    let send = |workset, agent: AgentId, words: &str| {
        eval(
            workset,
            format!(
                "from rho import mail\nprint(int(mail.send({:?}, 'user', {:?}, thread={})))",
                agent.encoded(),
                words,
                thread.0.0
            ),
        )
    };
    let first = send(aw, a, "Can I use clock A?").await?;
    let repeat = send(bw, b, "Can I use clock A?").await?;
    let changed = send(bw, b, "Can I use clock B?").await?;
    assert_eq!(first, repeat);
    assert_eq!(first, changed);
    let id = MessageId(rho_agent_types::ChatSeq(first.trim().parse()?));
    let revision = wait_for_mail(&mut stream, |row|
        matches!(row.event, ChatEvent::Revised { message, author, .. } if message == id && author == b)
    ).await?;
    let ChatEvent::Revised { content, .. } = revision.event else {
        unreachable!()
    };
    assert_eq!(
        content,
        [ContentPart::Text {
            text: "Can I use clock B?".into()
        }]
    );
    // The second agent was a member of the same thread at the time of the
    // first ask. Neither workset is sent a wake for mail to the user.
    for agent in [a, b] {
        let log = call(
            paths,
            AgentLog {
                agent,
                from: Wpos(0),
            },
        )
        .await?;
        assert!(
            !log.rows
                .iter()
                .any(|(_, row)| matches!(row, LogEvent::Request { .. }))
        );
    }
    // The new writer's address is resolved by the host, even after leaving.
    eval(bw, format!("mail.leave({}, {:?})", thread.0.0, b.encoded())).await?;
    eval(aw, format!("mail.leave({}, {:?})", thread.0.0, a.encoded())).await?;
    let answer = call(
        paths,
        Post {
            client_id: 112,
            parent: Some(thread),
            reply_to: Some(id),
            mentions: Vec::new(),
            content: vec![ContentPart::Text {
                text: "Use clock B.".into(),
            }],
        },
    )
    .await?;
    let answer = wait_for_mail(&mut stream, |row| row.seq == answer.0).await?;
    let ChatEvent::Posted { mentions, .. } = answer.event else {
        unreachable!()
    };
    assert_eq!(
        mentions
            .into_iter()
            .filter_map(|m| m.agent())
            .collect::<Vec<_>>(),
        [b]
    );
    // An explicit stop leaves a status, not another mail.
    call(paths, StopAgent { agent: b }).await?;
    loop {
        match read_frame(&mut stream).await? {
            ServerFrame::Work { threads } if threads.contains(&(thread, rho_agent_types::ThreadWork::StoppedByYou)) => break,
            ServerFrame::Mail { records } => assert!(!records.into_iter().any(|row| {
                matches!(row.event, ChatEvent::Posted { from: Address::System { agent, .. }, .. } if agent == b)
            })),
            _ => {}
        }
    }
    Ok(format!(
        "two agents, one live ask #{}; the answer named the new writer",
        id.0.0
    ))
}

async fn run(paths: &RuntimePaths, root: &Path) -> anyhow::Result<String> {
    let mut stream = connect_chat(paths).await?;
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
            root: project
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
        if *parent == thread && *from == agent && mentions.iter().any(|m| matches!(m, rho_agent_types::Mention::User { ping: true }))
    )).await?;
    let EvalResult { output, error, .. } = call(
        paths,
        Eval {
            workset: created.workset,
            code: format!(
                r#"
import os
from rho.tools import Mail
mail = Mail("{}")
note = await mail.send("user", "Use polling for this check.", thread={}, ping=False)
assert isinstance(note, int)
assert "Mail #{prior} has no user reply" in note.notice, note.notice
assert "mail.edit({prior}, ...)" in note.notice, note.notice
mail.edit(note, "Use sockets for this check.", ping=True)
hits = await mail.search("sockets", project={})
assert [row["message"] for row in hits] == [note]
mail.delete(note)
assert not mail.search("sockets", project={})
print(f"worker:{{os.getpid()}} note:{{note}}")
"#,
                agent.encoded(),
                thread.0.0,
                project.0.0,
                project.0.0,
                prior = origin_result.seq.0,
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
    while !(edited && deleted) {
        match read_frame(&mut stream).await? {
            ServerFrame::Work { .. } => {}
            ServerFrame::Mail { records } => {
                for row in records {
                    match row.event {
                        ChatEvent::Revised {
                            message,
                            ping: Some(true),
                            ..
                        } if message == note => edited = true,
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
    let stopped = loop {
        match read_frame(&mut stream).await? {
            ServerFrame::Mail { records } => {
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
            ServerFrame::Work { .. } => {}
            ServerFrame::Hello { .. } => {}
        }
        if let Some(mail) = stop_mail.take() {
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
            .any(|m| matches!(m, rho_agent_types::Mention::User { ping: true }))
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
    assert_eq!(mentions, [rho_agent_types::Mention::Agent(agent)]);
    Ok(format!(
        "mail wire: project {}, fake model result #{}, edit/delete #{}, lost stop #{}, and addressed reply #{}",
        project.0.0, origin_result.seq.0, note.0.0, stopped.seq.0, answer.0.0
    ))
}

async fn run_startup_failure(paths: &RuntimePaths, root: &Path) -> anyhow::Result<String> {
    // Break the fixture account after the host has bootstrapped it. This
    // fails before a provider request regardless of the machine's Claude
    // executable or namespace support.
    std::fs::write(
        root.join("claude-accounts/default/settings.json"),
        "{broken",
    )?;
    let mut stream = connect_chat(paths).await?;
    let ServerFrame::Hello { head, .. } = read_frame(&mut stream).await? else {
        anyhow::bail!("no mail hello");
    };
    write_frame(&mut stream, &ClientFrame::Follow { since: head }).await?;
    let project = post(paths, 70, None, "clock").await?;
    let thread = post(paths, 71, Some(project), "Check the failing clock test.").await?;
    let created = call(
        paths,
        CreateWorkset {
            kind: WorksetKind::Host { cwd: root.to_str().unwrap().into() },
            code: format!(
                r#"new_agent(runtime={{"kind": "claude", "model": "opus", "effort": "high"}}, origin="chat:{}")"#,
                thread.0.0
            ),
        },
    )
    .await?;
    let agent = created
        .result
        .agent
        .with_context(|| format!("agent not created: {:?}", created.result.error))?;
    let stop = wait_for_mail(&mut stream, |row| {
        matches!(row.event, ChatEvent::Posted {
            from: Address::System { agent: stopped, stop: Stop::Failed, .. },
            parent: Some(parent), ..
        } if parent == thread && stopped == agent)
    })
    .await?;
    let stop = rho_agent_types::Message::from_record(&stop).unwrap();
    let text = stop.text();
    assert!(
        text.contains("parse Claude settings ‹host path›:"),
        "{text}"
    );
    assert!(
        text.contains("Replies are saved. Change its runtime or start another agent."),
        "{text}"
    );
    assert!(!text.contains(root.to_str().unwrap()), "{text}");

    let mut replies = Vec::new();
    for (client_id, reply_to, text) in [
        (72, Some(stop.id), "Use a separate clock per test."),
        (73, None, "Keep the fix small."),
    ] {
        let reply = call(
            paths,
            Post {
                client_id,
                parent: Some(thread),
                reply_to,
                mentions: Vec::new(),
                content: vec![ContentPart::Text { text: text.into() }],
            },
        )
        .await?;
        replies.push(reply);
    }

    // The hold is durable, not just a flag on the running worker. A reply
    // or a host-side restart must not produce another failed attempt.
    call(paths, RestartWorkset { agent }).await?;
    let later = post(paths, 74, Some(thread), "Actually check it twice.").await?;
    replies.push(later);
    let switched = call(
        paths,
        Eval {
            workset: created.workset,
            code: format!(
                r#"rho.agent("{}").set_runtime({{"kind": "native", "model": "sol", "effort": "medium"}})"#,
                agent.encoded()
            ),
        },
    )
    .await?;
    anyhow::ensure!(
        switched.error.is_none(),
        "switch failed: {:?}",
        switched.error
    );

    let wanted = std::iter::once(thread).chain(replies).collect::<Vec<_>>();
    loop {
        let log = call(
            paths,
            AgentLog {
                agent,
                from: Wpos::ZERO,
            },
        )
        .await?;
        let failed = log
            .rows
            .iter()
            .filter(|(_, row)| matches!(row, LogEvent::Stopped { .. }))
            .count();
        assert_eq!(
            failed, 1,
            "replies attempted another launch: {:?}",
            log.rows
        );
        let delivered = log
            .rows
            .iter()
            .filter_map(|(_, row)| match row {
                LogEvent::Request { inputs, .. } => Some(inputs),
                _ => None,
            })
            .flatten()
            .filter_map(|input| input.message)
            .collect::<Vec<_>>();
        if wanted.iter().all(|mail| delivered.contains(mail)) {
            assert_eq!(
                delivered, wanted,
                "the new runtime must read each saved mail exactly once in order"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let refused = call(
        paths,
        Eval {
            workset: created.workset,
            code: format!(
                r#"rho.agent("{}").set_runtime({{"kind": "claude", "model": "opus", "effort": "high"}})"#,
                agent.encoded()
            ),
        },
    )
    .await?;
    assert!(
        refused
            .raised()
            .is_some_and(|error| error.contains("never sent a request"))
    );
    Ok(format!(
        "one failed launch #{}; three saved replies survived restart and reached native in order; later cross-kind switch refused",
        stop.id.0.0
    ))
}

async fn run_peer_chain(paths: &RuntimePaths, root: &Path) -> anyhow::Result<String> {
    let mut stream = connect_chat(paths).await?;
    let ServerFrame::Hello { head, host } = read_frame(&mut stream).await? else {
        anyhow::bail!("no mail hello");
    };
    write_frame(&mut stream, &ClientFrame::Follow { since: head }).await?;
    let project = post(paths, 1, None, "tide").await?;
    let work = post(paths, 2, Some(project), "MAIL-RIG-START").await?;
    let code = format!(
        r#"new_agent(runtime={{"kind": "native", "model": "sol", "effort": "medium"}}, origin="chat:{}")"#,
        work.0.0
    );
    let leader = call(
        paths,
        CreateWorkset {
            kind: WorksetKind::Host {
                cwd: root.to_str().unwrap().into(),
            },
            code,
        },
    )
    .await?
    .result
    .agent
    .context("no parent agent")?;

    let mut history = HostMail::default();
    let mut brief = None;
    let mut peer = None;
    let ask = loop {
        if let ServerFrame::Mail { records } = read_frame(&mut stream).await? {
            let mut ask = None;
            for record in records {
                history.tell(&record);
                let Some(mail) = rho_agent_types::Message::from_record(&record) else {
                    continue;
                };
                match mail.text().as_str() {
                    "MAIL-RIG-CHILD-BRIEF" => {
                        assert_eq!(mail.parent, Some(project));
                        assert_eq!(mail.from, Address::Agent(leader));
                        let [mention] = &mail.mentions[..] else {
                            anyhow::bail!("bad child address")
                        };
                        let rho_agent_types::Mention::Direct(child) = *mention else {
                            anyhow::bail!("not a child")
                        };
                        brief = Some((mail.id, child));
                    }
                    "MAIL-RIG-PEER-RESULT" => {
                        let (brief, child) = brief.context("child sent before brief")?;
                        assert_eq!(mail.parent, Some(brief));
                        assert_eq!(mail.reply_to, Some(brief));
                        assert_eq!(mail.from, Address::Agent(child));
                        assert_eq!(mail.mentions, [rho_agent_types::Mention::Direct(leader)]);
                        let claims = cards::claims(
                            &cards::Host {
                                endpoint: host,
                                chat: &history,
                                name: "local",
                            },
                            &rho_dealer::Marks::default(),
                            jiff::Timestamp::now(),
                            None,
                        );
                        assert!(claims.is_empty(), "peer mail made a card");
                        peer = Some(mail.id);
                    }
                    "Can I use the test clock?" => {
                        assert_eq!(mail.parent, Some(work));
                        assert_eq!(mail.from, Address::Agent(leader));
                        assert!(mail.pings_user());
                        ask = Some(mail.id);
                    }
                    _ => {}
                }
            }
            if let Some(ask) = ask {
                break ask;
            }
        }
    };

    let answer = call(
        paths,
        Post {
            client_id: 3,
            parent: Some(work),
            reply_to: Some(ask),
            mentions: Vec::new(),
            content: vec![ContentPart::Text {
                text: "MAIL-RIG-USER-ANSWER".into(),
            }],
        },
    )
    .await?;
    let row = wait_for_mail(&mut stream, |row| row.seq == answer.0).await?;
    let answer_msg = rho_agent_types::Message::from_record(&row).unwrap();
    assert_eq!(
        (answer_msg.parent, answer_msg.reply_to, answer_msg.from),
        (Some(work), Some(ask), Address::User)
    );

    let peer = peer.context("parent sent an ask before receiving the child mail")?;
    let brief = brief.unwrap().0;
    let peer_delay = carried_after(paths, leader, peer, Role::Tool, "[message from eng-").await?;
    let user_delay =
        carried_after(paths, leader, answer, Role::User, "MAIL-RIG-USER-ANSWER").await?;
    assert!(
        (14_950..18_000).contains(&peer_delay),
        "peer wake was {peer_delay}ms, not 15s"
    );
    assert!(
        (1_950..3_500).contains(&user_delay),
        "user wake was {user_delay}ms, not 2s"
    );
    // The request carrying the reply and the completion are separate rows.
    // Rewind only after the last response stopped.
    loop {
        let log = call(
            paths,
            AgentLog {
                agent: leader,
                from: Wpos(0),
            },
        )
        .await?;
        let mut saw_answer = false;
        let mut completed = false;
        for (_, row) in log.rows {
            match row {
                LogEvent::Request { inputs, .. }
                    if inputs.iter().any(|input| input.message == Some(answer)) =>
                {
                    saw_answer = true
                }
                LogEvent::Replied { .. } if saw_answer => completed = true,
                _ => {}
            }
        }
        if completed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let rewound = call(
        paths,
        RewindAgent {
            agent: leader,
            turns: 2,
        },
    )
    .await?;
    assert!(
        rewound
            .taken_back
            .iter()
            .any(|title| title == "Can I use the test clock?")
    );
    wait_for_mail(&mut stream, |row| {
        matches!(row.event,
            ChatEvent::TakenBack { message, author, .. } if message == ask && author == leader
        )
    })
    .await?;
    Ok(format!(
        "one record: brief #{}, peer reply #{} ({}ms, no card), user ask #{}, user answer #{} ({}ms)",
        brief.0.0, peer.0.0, peer_delay, ask.0.0, answer.0.0, user_delay
    ))
}

async fn carried_after(
    paths: &RuntimePaths,
    agent: AgentId,
    mail: MessageId,
    role: Role,
    text: &str,
) -> anyhow::Result<u64> {
    loop {
        let log = call(
            paths,
            AgentLog {
                agent,
                from: Wpos(0),
            },
        )
        .await?;
        let mut prev = None;
        for (_, row) in log.rows {
            match row {
                LogEvent::Replied { at, .. } => prev = Some(at),
                LogEvent::Request { at, inputs, .. } => {
                    if let Some(input) =
                        inputs.into_iter().find(|input| input.message == Some(mail))
                    {
                        assert_eq!(input.role, role);
                        assert!(
                            input.text.contains(text),
                            "missing framing in {:?}",
                            input.text
                        );
                        let at_prev = prev.context("no prior response for wake delay")?;
                        return Ok(at.0 - at_prev.0);
                    }
                }
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn connect_chat(paths: &RuntimePaths) -> anyhow::Result<rho_rpc::Stream> {
    let mut stream = loop {
        if let Ok(stream) = connect(paths).await {
            break stream;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    };
    write_open(&mut stream, &Open::Session).await?;
    Ok(stream)
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
        if let ServerFrame::Mail { records } = read_frame(stream).await?
            && let Some(found) = records.into_iter().find(&predicate)
        {
            return Ok(found);
        }
    }
}

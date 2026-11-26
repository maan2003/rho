//! Drive the real agent host, a real worker and the local chat protocol.
//! Only the inference provider and account are fake.
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::Context as _;
use rho_agent_types::{
    Address, AgentId, ChatEvent, ChatRecord, ContentPart, MessageId, ProjectChange, ProjectConfig,
    ProjectId, ProjectMode, ProjectName, ProjectPlace, Role, RuntimeLabel, Stop, Voice,
    WorksetKind, Wpos,
};
use rho_agents_client::protocol::worksets;
use rho_agents_client::protocol::worksets::{
    AgentLog, CreateWorkset, Eval, EvalResult, FinishWork, LogEvent, RestartWorkset, RewindAgent,
    StopAgent,
};
use rho_chat::protocol::{
    ChangeProject, ClientFrame, CreateProject, Open, Post, PostIdea, PostIdeaReply, Project,
    ResolveProject, RetryIdea, ServerFrame,
};
use rho_chat::{HostChat, cards};
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
        with_host(rho_fake_model::Scenario::RealToolRounds, false).await?;
        with_host(rho_fake_model::Scenario::MailChain, false).await?;
        with_host(rho_fake_model::Scenario::RealToolRounds, true).await
    })
}

async fn with_host(scenario: rho_fake_model::Scenario, projects: bool) -> anyhow::Result<()> {
    let rig = tempfile::tempdir()?;
    let root = rig.path();
    let paths = RuntimePaths::new(Some(root.join("run")))?;
    let auth = root.join("rho/auth.d");
    std::fs::create_dir_all(&auth)?;
    std::fs::write(
        auth.join("default.json"),
        serde_json::to_vec(&serde_json::json!({
            "access_token": "rho-chat-test",
            "expires_at_ms": u64::MAX,
            "account_id": "rho-chat-test",
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
    if projects {
        let policy = root.join("config/rho");
        std::fs::create_dir_all(&policy)?;
        std::fs::write(
            policy.join("init.py"),
            r#"
from rho.default import new_agent as default_new_agent

def new_agent(*, runtime, **kwargs):
    if runtime["kind"] == "claude":
        runtime = {"kind": "native", "model": "sol", "effort": "medium"}
    return default_new_agent(runtime=runtime, **kwargs)
"#,
        )?;
    }
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
        if projects {
            let _ready = connect_chat(&paths).await?;
            run_projects(&paths, root).await.context("project routing")
        } else if scenario == rho_fake_model::Scenario::MailChain {
            run_peer_chain(&paths, root).await
        } else {
            let mail = run(&paths, root).await?;
            let live = run_live_ask(&paths, root).await?;
            let recovery = run_startup_failure(&paths, root).await?;
            let wakes = run_reply_wakes(&paths, root).await?;
            anyhow::Ok(format!("{mail}\n{live}\n{recovery}\n{wakes}"))
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

async fn run_projects(paths: &RuntimePaths, root: &Path) -> anyhow::Result<String> {
    let old = root.join("first-directory");
    let new = root.join("different-directory");
    let corrected = root.join("corrected-directory");
    let repo = root.join("checkouts-tide");
    for path in [&old, &new, &corrected, &repo] {
        std::fs::create_dir(path)?;
    }
    let git = |args: &[&str]| -> anyhow::Result<()> {
        let ran = Command::new("git").current_dir(&repo).args(args).output()?;
        anyhow::ensure!(
            ran.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&ran.stderr)
        );
        Ok(())
    };
    git(&["init", "-b", "main"])?;
    std::fs::write(repo.join("fixture-name"), "tide-checked-out")?;
    git(&["add", "fixture-name"])?;
    git(&[
        "-c",
        "user.email=fixture@example.invalid",
        "-c",
        "user.name=Fixture",
        "commit",
        "-m",
        "test checkout",
    ])?;

    let place = |path: &Path| ProjectPlace::parse(path.to_str().unwrap()).unwrap();
    let one_config = ProjectConfig {
        place: place(&old),
        mode: ProjectMode::One,
        runtime: RuntimeLabel::default(),
    };
    let make = |name, config| async move {
        call(
            paths,
            CreateProject {
                name: ProjectName::parse(name).unwrap(),
                config,
            },
        )
        .await
    };
    let one = make("same", one_config.clone()).await?;
    let remote = url::Url::from_file_path(repo).unwrap();
    let each_config = ProjectConfig {
        place: ProjectPlace::parse(remote.as_str()).unwrap(),
        mode: ProjectMode::PerIdea,
        runtime: RuntimeLabel::parse("sol-low").unwrap(),
    };
    let each = make("independent", each_config.clone()).await?;
    let failed_config = ProjectConfig {
        place: place(&root.join("missing-place")),
        mode: ProjectMode::One,
        runtime: RuntimeLabel::default(),
    };
    let failed_project = make("asleep", failed_config.clone()).await?;
    assert_eq!(make("SAME", one_config.clone()).await?, one);
    assert!(make("same", failed_config.clone()).await.is_err());

    let mut first_device = connect_chat(paths).await?;
    let ServerFrame::Hello { head, .. } = read_frame(&mut first_device).await? else {
        anyhow::bail!("no mail hello");
    };
    write_frame(&mut first_device, &ClientFrame::Follow { since: head }).await?;
    assert_eq!(
        read_projects(paths).await?,
        vec![
            (one, one_config.clone()),
            (each, each_config.clone()),
            (failed_project, failed_config.clone())
        ],
    );

    let post_in = |project: ProjectId,
                   client_id,
                   text: &'static str,
                   runtime,
                   mode: Option<ProjectMode>| async move {
        call(
            paths,
            PostIdea {
                client_id,
                project,
                content: vec![ContentPart::Text { text: text.into() }],
                host_label: "lab".into(),
                runtime,
                mode,
            },
        )
        .await
    };
    let post_as =
        |project, client_id, text, runtime| post_in(project, client_id, text, runtime, None);
    let send = |project, client_id, text| async move {
        let PostIdeaReply::Posted(message) = post_as(project, client_id, text, None).await? else {
            anyhow::bail!("an idea without an override was refused");
        };
        Ok(message)
    };
    // An idea starting on another connection cannot seed a second standing
    // agent while the first one is being created.
    let (a, b) = tokio::try_join!(
        send(one, 801, "Test the channel's alpha clock."),
        send(one, 802, "Test the channel's unequal beta clock."),
    )?;
    let standing = idea_agent(paths, a).await?;
    assert_eq!(idea_agent(paths, b).await?, standing);
    let continuing = wait_for_mail(&mut first_device, |row| row.seq == a.max(b).0).await?;
    let continuing = rho_agent_types::Message::from_record(&continuing)
        .context("the second idea must be logged as mail")?;
    assert_eq!(continuing.from, Address::User);
    assert_eq!(continuing.parent, Some(one.0));
    assert_eq!(continuing.voice, Voice::Message);
    wait_for_delivery(paths, standing, a).await?;
    wait_for_delivery(paths, standing, b).await?;
    assert_eq!(
        send(one, 802, "This duplicate must not start anything.").await?,
        b
    );

    let PostIdeaReply::Posted(switched) = post_as(
        one,
        850,
        "Use a higher effort on the same timer.",
        Some(RuntimeLabel::parse("sol-high").unwrap()),
    )
    .await?
    else {
        anyhow::bail!("the standing agent could not take a same-kind change");
    };
    assert_eq!(idea_agent(paths, switched).await?, standing);
    wait_for_delivery(paths, standing, switched).await?;
    let refused = post_as(
        one,
        851,
        "This must stay in the draft.",
        Some(RuntimeLabel::parse("opus-high").unwrap()),
    )
    .await?;
    assert!(matches!(refused, PostIdeaReply::RuntimeRefused(reason)
        if reason.contains("only a stopped agent that has never sent a request")));
    let after_refusal = send(one, 851, "Now continue with the same runtime.").await?;
    let receipt = wait_for_mail(&mut first_device, |row| row.seq == after_refusal.0).await?;
    assert_eq!(
        rho_agent_types::Message::from_record(&receipt)
            .unwrap()
            .text(),
        "Now continue with the same runtime.",
        "a refused override never posts the idea"
    );
    assert_eq!(idea_agent(paths, after_refusal).await?, standing);

    let next_default = RuntimeLabel::parse("sol-xhigh").unwrap();
    call(
        paths,
        ChangeProject {
            project: one,
            change: ProjectChange::Runtime(next_default.clone()),
        },
    )
    .await?;
    assert_eq!(
        read_projects(paths).await?[0],
        (
            one,
            ProjectConfig {
                runtime: next_default.clone(),
                ..one_config.clone()
            }
        )
    );

    call(paths, StopAgent { agent: standing }).await?;
    let after_stop = send(one, 803, "Test the stopped channel's stern clock.").await?;
    assert_eq!(idea_agent(paths, after_stop).await?, standing);
    wait_for_delivery(paths, standing, after_stop).await?;
    // Woken by an idea whose `on:` was left alone, it keeps the runtime it
    // was switched to, not the project's newer default.
    assert_eq!(agent_runtime(paths, standing).await?, "sol-high");
    // Every device can name the standing agent's workset by the project.
    assert_eq!(standing_agent(paths, one).await?, Some(standing));

    // `in: new` gives this one idea a fresh agent; the standing agent
    // stays the project's and takes the next idea.
    let PostIdeaReply::Posted(fresh_idea) = post_in(
        one,
        806,
        "Try this one on its own.",
        None,
        Some(ProjectMode::PerIdea),
    )
    .await?
    else {
        anyhow::bail!("an `in: new` idea was refused");
    };
    let fresh_one = idea_agent(paths, fresh_idea).await?;
    assert_ne!(fresh_one, standing);
    assert_eq!(
        agent_runtime(paths, fresh_one).await?,
        next_default.as_str()
    );
    wait_for_delivery(paths, fresh_one, fresh_idea).await?;
    assert_eq!(standing_agent(paths, one).await?, Some(standing));
    let back_to_standing = send(one, 807, "And this one goes to the usual agent.").await?;
    assert_eq!(idea_agent(paths, back_to_standing).await?, standing);

    let PostIdeaReply::Posted(first) = post_as(
        each,
        810,
        "Check the first isolated checkout.",
        Some(RuntimeLabel::parse("sol-xhigh").unwrap()),
    )
    .await?
    else {
        anyhow::bail!("a fresh per-idea agent could not start on the chosen runtime");
    };
    let first_agent = idea_agent(paths, first).await?;
    let second = send(each, 811, "Check the second isolated checkout.").await?;
    let second_agent = idea_agent(paths, second).await?;
    assert_ne!(first_agent, second_agent);
    wait_for_delivery(paths, first_agent, first).await?;
    wait_for_delivery(paths, second_agent, second).await?;
    assert_eq!(standing_agent(paths, each).await?, None);
    // `in: one` in a per-idea project starts its standing agent, and the
    // next `in: one` idea goes to that same agent.
    let mut ones = Vec::new();
    for (client_id, text) in [
        (812, "Keep one agent on the docs."),
        (813, "And the changelog."),
    ] {
        let PostIdeaReply::Posted(idea) =
            post_in(each, client_id, text, None, Some(ProjectMode::One)).await?
        else {
            anyhow::bail!("an `in: one` idea was refused");
        };
        ones.push(idea_agent(paths, idea).await?);
    }
    assert_eq!(ones[0], ones[1]);
    assert!(![first_agent, second_agent].contains(&ones[0]));
    assert_eq!(standing_agent(paths, each).await?, Some(ones[0]));

    // A project renamed is the same project under its new name; a name
    // another project has is refused.
    let rename = |name: &str| ChangeProject {
        project: each,
        change: ProjectChange::Name(ProjectName::parse(name).unwrap()),
    };
    call(paths, rename("apart")).await?;
    let resolved = call(
        paths,
        ResolveProject {
            name: "apart".into(),
        },
    )
    .await?;
    assert_eq!(resolved.map(|project| project.id), Some(each));
    assert!(call(paths, rename("same")).await.is_err());
    // Retired, it starts nothing new; its settings and threads stay.
    call(
        paths,
        ChangeProject {
            project: each,
            change: ProjectChange::Retire,
        },
    )
    .await?;
    assert!(
        post_as(each, 814, "Too late for this project.", None)
            .await
            .is_err()
    );

    let failed = send(
        failed_project,
        820,
        "Retry this work without losing the brief.",
    )
    .await?;
    let failure = wait_for_mail(&mut first_device, |row| {
        matches!(row.event, ChatEvent::Posted {
        from: Address::StartFailed, parent: Some(idea), ..
    } if idea == failed)
    })
    .await?;
    let stop = rho_agent_types::Message::from_record(&failure).unwrap();
    assert_eq!(stop.parent, Some(failed));
    assert!(
        stop.text()
            .contains(&format!("{} on lab", failed_config.place.as_str()))
    );
    assert!(
        stop.text()
            .contains("reply to retry, or change where it works with set project place")
    );
    assert_eq!(
        send(
            failed_project,
            820,
            "A retry of this network request is not a new stop."
        )
        .await?,
        failed
    );

    call(
        paths,
        ChangeProject {
            project: one,
            change: ProjectChange::Place(place(&new)),
        },
    )
    .await?;
    let changed = read_projects(paths).await?;
    assert_eq!(
        changed[0],
        (
            one,
            ProjectConfig {
                place: place(&new),
                mode: ProjectMode::One,
                runtime: next_default.clone(),
            }
        )
    );
    let next = send(one, 804, "Use the new directory only for this next idea.").await?;
    let next_agent = idea_agent(paths, next).await?;
    assert_ne!(standing, next_agent);
    wait_for_delivery(paths, next_agent, next).await?;
    assert_eq!(idea_agent(paths, a).await?, standing);

    // The user fixes the place and the runtime between the refusal and the
    // reply; the retry starts with both as they are now.
    for change in [
        ProjectChange::Place(place(&corrected)),
        ProjectChange::Runtime(RuntimeLabel::parse("sol-low").unwrap()),
    ] {
        call(
            paths,
            ChangeProject {
                project: failed_project,
                change,
            },
        )
        .await?;
    }
    let retry = call(
        paths,
        RetryIdea {
            client_id: 821,
            failed_start: stop.id,
            content: vec![ContentPart::Text {
                text: "Use the corrected place now.".into(),
            }],
            host_label: "lab".into(),
        },
    )
    .await?;
    let retry_agent = idea_agent(paths, failed).await?;
    wait_for_delivery(paths, retry_agent, failed).await?;
    wait_for_delivery(paths, retry_agent, retry).await?;
    let log = call(
        paths,
        AgentLog {
            agent: retry_agent,
            from: Wpos::ZERO,
        },
    )
    .await?;
    let deliveries: Vec<_> = log
        .rows
        .into_iter()
        .filter_map(|(_, row)| match row {
            LogEvent::Request { inputs, .. } => Some(inputs),
            _ => None,
        })
        .flatten()
        .filter_map(|input| input.message)
        .collect();
    assert_eq!(deliveries[..2], [failed, retry]);

    call(
        paths,
        ChangeProject {
            project: one,
            change: ProjectChange::Mode(ProjectMode::PerIdea),
        },
    )
    .await?;
    let per_idea = send(one, 805, "Use a fresh agent after the mode changes.").await?;
    let fresh = idea_agent(paths, per_idea).await?;
    assert_ne!(fresh, next_agent);
    assert_ne!(fresh, standing);
    wait_for_delivery(paths, fresh, per_idea).await?;
    // A new session sees the host's settings.
    assert_eq!(
        read_projects(paths).await?,
        vec![
            (
                one,
                ProjectConfig {
                    place: place(&new),
                    mode: ProjectMode::PerIdea,
                    runtime: next_default.clone(),
                }
            ),
            (each, each_config),
            (
                failed_project,
                ProjectConfig {
                    place: place(&corrected),
                    mode: ProjectMode::One,
                    runtime: RuntimeLabel::parse("sol-low").unwrap(),
                }
            ),
        ],
    );

    let mut directory = connect(paths).await?;
    write_open(&mut directory, &worksets::Open::Session).await?;
    let (worksets, agents) = loop {
        if let worksets::ServerFrame::Directory { worksets, agents } =
            read_frame(&mut directory).await?
        {
            break (worksets, agents);
        }
    };
    let runtime = |id| {
        agents
            .iter()
            .find(|agent| agent.agent == id)
            .unwrap()
            .spec
            .runtime_label()
    };
    assert_eq!(
        runtime(standing),
        "sol-high",
        "unspecified ideas keep the standing runtime"
    );
    assert_eq!(
        runtime(first_agent),
        "sol-xhigh",
        "a new agent uses the explicit runtime"
    );
    assert_eq!(
        runtime(second_agent),
        "sol-low",
        "an unspecified new agent uses the project setting"
    );
    assert_eq!(
        runtime(next_agent),
        "sol-xhigh",
        "a new agent uses the changed project default"
    );
    let by_agent = |id| {
        agents
            .iter()
            .find(|agent| agent.agent == id)
            .unwrap()
            .workset
    };
    let kind = |id| {
        &worksets
            .iter()
            .find(|entry| entry.workset == by_agent(id))
            .unwrap()
            .kind
    };
    let first_workset = by_agent(first_agent);
    let second_workset = by_agent(second_agent);
    assert_ne!(first_workset, second_workset);
    for id in [first_agent, second_agent] {
        assert_eq!(kind(id), &WorksetKind::Isolated);
    }
    for workset in [first_workset, second_workset] {
        let checkout = call(paths, Eval {
            workset,
            code: "from pathlib import Path\nprint(Path('/src/checkouts-tide/fixture-name').read_text())".into(),
        }).await?;
        assert_eq!(checkout.error, None);
        assert_eq!(checkout.output.trim(), "tide-checked-out");
    }
    // `finish work`: a checkout holding changes of its own stays, and a
    // clean one with nothing unpushed goes.
    let touched = call(
        paths,
        Eval {
            workset: first_workset,
            code:
                "from pathlib import Path\nPath('/src/checkouts-tide/draft.txt').write_text('mine')"
                    .into(),
        },
    )
    .await?;
    assert_eq!(touched.error, None);
    // The fake model leaves its round counter in the checkout, as an
    // agent leaves its work; the second workset's agent cleans up.
    let cleaned = call(paths, Eval {
        workset: second_workset,
        code: "from pathlib import Path\nfor p in Path('/src/checkouts-tide').glob('.rho-fake-rounds-*'):\n    p.unlink()".into(),
    }).await?;
    assert_eq!(cleaned.error, None);
    let kept = call(paths, FinishWork { agent: first_agent }).await?;
    assert_eq!(
        (kept.removed, kept.kept),
        (
            Vec::<String>::new(),
            vec!["checkouts-tide: 2 uncommitted changes".to_owned()]
        )
    );
    let removed = call(
        paths,
        FinishWork {
            agent: second_agent,
        },
    )
    .await?;
    assert_eq!(
        (removed.removed, removed.kept),
        (vec!["checkouts-tide".to_owned()], Vec::<String>::new())
    );
    for (workset, there) in [(first_workset, "True"), (second_workset, "False")] {
        let left = call(
            paths,
            Eval {
                workset,
                code: "from pathlib import Path\nprint(Path('/src/checkouts-tide').exists())"
                    .into(),
            },
        )
        .await?;
        assert_eq!(left.output.trim(), there, "{workset}");
    }
    let create_at = |agent| {
        agents
            .iter()
            .find(|entry| entry.agent == agent)
            .unwrap()
            .spec
            .props
            .get("cwd")
            .map(String::as_str)
            .unwrap()
    };
    assert_eq!(create_at(standing), old.to_str().unwrap());
    assert_eq!(create_at(next_agent), new.to_str().unwrap());
    assert_eq!(
        create_at(retry_agent),
        corrected.to_str().unwrap(),
        "a retried refused start works where the project works now"
    );
    assert_eq!(
        runtime(retry_agent),
        "sol-low",
        "a retried refused start runs on the project's runtime now"
    );
    assert_ne!(
        by_agent(next_agent),
        by_agent(fresh),
        "per idea creates a fresh directory workset too"
    );
    assert_eq!(
        agents
            .iter()
            .filter(|entry| {
                entry
                    .spec
                    .props
                    .get("origin")
                    .and_then(|s| s.strip_prefix("chat:"))
                    .and_then(|s| s.parse::<u64>().ok())
                    .is_some_and(|origin| {
                        [
                            a, b, after_stop, first, second, next, retry, per_idea, failed,
                        ]
                        .iter()
                        .any(|mail| origin == mail.0.0)
                    })
            })
            .count(),
        6,
        "reused and refused starts create no extra agents"
    );

    Ok(format!(
        "project delivery: one {standing:?} reused after stop; isolated clones {first_workset}, {second_workset}; place/mode change and failure retry persisted across devices"
    ))
}

/// Each project's settings as a new session folds them from the log: the
/// latest `Project` event of each root, in the order the roots became
/// projects.
async fn read_projects(paths: &RuntimePaths) -> anyhow::Result<Vec<(ProjectId, ProjectConfig)>> {
    let mut stream = connect_chat(paths).await?;
    let ServerFrame::Hello { head, .. } = read_frame(&mut stream).await? else {
        anyhow::bail!("no chat hello");
    };
    write_frame(
        &mut stream,
        &ClientFrame::Follow {
            since: rho_agent_types::ChatSeq::ZERO,
        },
    )
    .await?;
    let mut projects: Vec<(ProjectId, ProjectConfig)> = Vec::new();
    let mut seen = rho_agent_types::ChatSeq::ZERO;
    while seen < head {
        let ServerFrame::Messages { records } = read_frame(&mut stream).await? else {
            continue;
        };
        for record in records {
            seen = seen.max(record.seq);
            if let ChatEvent::Project { project, config } = record.event {
                match projects.iter_mut().find(|(id, _)| *id == project) {
                    Some((_, held)) => *held = config,
                    None => projects.push((project, config)),
                }
            }
        }
    }
    Ok(projects)
}

/// The runtime the agent's spec names now, `sol-high`.
async fn agent_runtime(paths: &RuntimePaths, agent: AgentId) -> anyhow::Result<String> {
    let log = call(
        paths,
        AgentLog {
            agent,
            from: Wpos::ZERO,
        },
    )
    .await?;
    log.rows
        .iter()
        .rev()
        .find_map(|(_, row)| match row {
            LogEvent::AgentCreated {
                agent: of, spec, ..
            }
            | LogEvent::AgentChanged {
                agent: of, spec, ..
            } if *of == agent => Some(spec.runtime_label()),
            _ => None,
        })
        .context("the agent has no spec")
}

/// The agent that stands for `project`, as a new device folds the log.
async fn standing_agent(
    paths: &RuntimePaths,
    project: ProjectId,
) -> anyhow::Result<Option<AgentId>> {
    let mut stream = connect_chat(paths).await?;
    let ServerFrame::Hello { head, .. } = read_frame(&mut stream).await? else {
        anyhow::bail!("no chat hello");
    };
    write_frame(
        &mut stream,
        &ClientFrame::Follow {
            since: rho_agent_types::ChatSeq::ZERO,
        },
    )
    .await?;
    let mut chat = HostChat::default();
    while chat.head < head {
        let ServerFrame::Messages { records } = read_frame(&mut stream).await? else {
            continue;
        };
        for record in &records {
            chat.tell(record);
        }
    }
    Ok(chat
        .standing_agents()
        .find(|(root, _)| *root == project.0)
        .map(|(_, agent)| agent))
}

async fn idea_agent(paths: &RuntimePaths, thread: MessageId) -> anyhow::Result<AgentId> {
    // Each read is a different device cursor. Unlike a race with a single
    // consumed frame, it can observe joins already delivered in a batch.
    let mut stream = connect_chat(paths).await?;
    let _: ServerFrame = read_frame(&mut stream).await?;
    write_frame(
        &mut stream,
        &ClientFrame::Follow {
            since: rho_agent_types::ChatSeq::ZERO,
        },
    )
    .await?;
    let joined = wait_for_mail(
        &mut stream,
        |row| matches!(row.event, ChatEvent::Joined { thread: joined, .. } if joined == thread),
    )
    .await?;
    let ChatEvent::Joined { agent, .. } = joined.event else {
        unreachable!()
    };
    Ok(agent)
}

async fn wait_for_delivery(
    paths: &RuntimePaths,
    agent: AgentId,
    mail: MessageId,
) -> anyhow::Result<()> {
    loop {
        let log = call(
            paths,
            AgentLog {
                agent,
                from: Wpos::ZERO,
            },
        )
        .await?;
        if log.rows.iter().any(|(_, row)| matches!(row, LogEvent::Request { inputs, .. } if inputs.iter().any(|input| input.message == Some(mail)))) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

async fn run_live_ask(paths: &RuntimePaths, root: &Path) -> anyhow::Result<String> {
    let mut stream = connect_chat(paths).await?;
    let ServerFrame::Hello { head, .. } = read_frame(&mut stream).await? else {
        anyhow::bail!("no mail hello");
    };
    write_frame(&mut stream, &ClientFrame::Follow { since: head }).await?;
    let project = post(paths, 110, None, "lighthouse").await?;
    let thread = post(paths, 111, Some(project), "Check the clock offsets.").await?;
    // Each in a directory of its own: the fake model counts its tool
    // rounds in a file in the working directory, and both agents work at
    // once when one reply wakes them.
    let make_agent = |name: &'static str| async move {
        let cwd = root.join(name);
        std::fs::create_dir_all(&cwd)?;
        let created = call(
            paths,
            CreateWorkset {
                kind: WorksetKind::Host {
                    cwd: cwd.to_str().unwrap().into(),
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
    let (a, aw) = make_agent("a").await?;
    let (b, bw) = make_agent("b").await?;
    let eval = |workset, code| async move {
        let result = call(paths, Eval { workset, code }).await?;
        anyhow::ensure!(result.error.is_none(), "{:?}", result.error);
        Ok::<_, anyhow::Error>(result.output)
    };
    for (agent, workset) in [(a, aw), (b, bw)] {
        eval(
            workset,
            format!(
                "from rho import chat\nchat.join({}, {:?})",
                thread.0.0,
                agent.encoded()
            ),
        )
        .await?;
    }
    let chat = |workset, agent: AgentId, words: &str, kind: &str| {
        eval(
            workset,
            format!(
                "from rho.tools import Chat\nprint(int(Chat({:?}).send({:?}, {:?}, thread={})))",
                agent.encoded(),
                words,
                kind,
                thread.0.0
            ),
        )
    };
    for words in [
        "Examining both clocks.",
        "✨ Quick note: checking the edge conditions.",
    ] {
        let sent = chat(aw, a, words, "flash").await?;
        let flash_id = sent.trim().parse::<u64>()?;
        let posted = wait_for_mail(&mut stream, |row| row.seq.0 == flash_id).await?;
        let flash = rho_agent_types::Message::from_record(&posted).context("not a post")?;
        assert_eq!(flash.voice, Voice::Flash);
        assert_eq!(flash.parent, Some(thread));
        assert_eq!(flash.text(), words);
        assert!(!flash.pings_user());
    }
    let first = chat(aw, a, "Use polling for this check.", "message").await?;
    let first: u64 = first.trim().parse()?;
    let posted = wait_for_mail(&mut stream, |row| row.seq.0 == first).await?;
    let posted = rho_agent_types::Message::from_record(&posted).context("not a post")?;
    assert_eq!(posted.voice, Voice::Message);
    assert!(posted.pings_user());
    let independent = chat(bw, b, "Can I use clock B?", "message").await?;
    let id = MessageId(rho_agent_types::ChatSeq(independent.trim().parse()?));
    assert_ne!(first, id.0.0, "each agent owns its own message");
    let refused = chat(aw, a, "Can I use clock A?", "message")
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains(&format!("chat.edit({first}, text)")),
        "{refused:#}"
    );
    assert!(
        refused.to_string().contains("kind=\\\"flash\\\""),
        "{refused:#}"
    );
    let first_before = call(
        paths,
        Eval {
            workset: aw,
            code: format!("from rho import chat\nprint(chat.message({first})['text'])"),
        },
    )
    .await?;
    assert_eq!(first_before.output.trim(), "Use polling for this check.");
    let edit = |workset, agent: AgentId, message: u64, words: &str| {
        eval(
            workset,
            format!(
                "from rho.tools import Chat\nChat({:?}).edit({message}, {:?})",
                agent.encoded(),
                words
            ),
        )
    };
    // An agent updates its live message by editing it.
    for text in ["Use clock A for this check.", "Use clock A for both."] {
        edit(aw, a, first, text).await?;
        let row = wait_for_mail(&mut stream, |row| {
            matches!(row.event, ChatEvent::Revised { message, author, .. } if message.0.0 == first && author == a)
        }).await?;
        let ChatEvent::Revised { content, .. } = row.event else {
            unreachable!()
        };
        assert_eq!(content, [ContentPart::Text { text: text.into() }]);
    }
    let not_its_own = edit(aw, a, id.0.0, "Use clock A.").await.unwrap_err();
    assert!(
        not_its_own.to_string().contains("own words"),
        "{not_its_own:#}"
    );
    edit(bw, b, id.0.0, "Can I use a monotonic clock?").await?;
    let revision = wait_for_mail(&mut stream, |row|
        matches!(row.event, ChatEvent::Revised { message, author, .. } if message == id && author == b)
    ).await?;
    let ChatEvent::Revised { content, .. } = revision.event else {
        unreachable!()
    };
    assert_eq!(
        content,
        [ContentPart::Text {
            text: "Can I use a monotonic clock?".into()
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
    // The reply wakes both joined agents without naming a recipient. The
    // dealt version is metadata only, for settling and freezing one ask.
    let seen = rho_agent_types::MessageVersion {
        message: id,
        version: revision.seq,
    };
    let answer = call(
        paths,
        Post {
            client_id: 112,
            parent: Some(thread),
            settles: Some(seen),
            content: vec![ContentPart::Text {
                text: "Use clock B.".into(),
            }],
        },
    )
    .await?;
    let answer_post = wait_for_mail(&mut stream, |row| row.seq == answer.0).await?;
    let reply = rho_agent_types::Message::from_record(&answer_post).context("not a post")?;
    assert_eq!(reply.voice, Voice::Message);
    assert_eq!(reply.retry_of, None);
    assert_eq!(reply.thread_reply.unwrap().settles, Some(seen));
    for agent in [a, b] {
        wait_for_delivery(paths, agent, answer).await?;
    }
    // There is still no user answer tied to A's older words.
    edit(aw, a, first, "A was not settled.").await?;
    // The user's reply ends B's live message, so the message B's model
    // sends in answer is new, not a revision of the answered one.
    let row = wait_for_mail(&mut stream, |row| {
        row.seq > answer.0
            && matches!(row.event, ChatEvent::Posted { from: Address::Agent(from), .. } if from == b)
    })
    .await?;
    let fresh = rho_agent_types::Message::from_record(&row).context("not a post")?;
    assert_ne!(fresh.id, id);
    assert_eq!(fresh.parent, Some(thread));
    assert_eq!(fresh.voice, Voice::Message);
    assert!(fresh.pings_user());
    let frozen = edit(bw, b, id.0.0, "I will use clock B.")
        .await
        .unwrap_err();
    assert!(
        frozen.to_string().contains("already answered"),
        "{frozen:#}"
    );
    // An explicit stop leaves a status, not another mail.
    call(paths, StopAgent { agent: b }).await?;
    loop {
        match read_frame(&mut stream).await? {
            ServerFrame::Work { threads } if threads.contains(&(thread, rho_agent_types::ThreadWork::StoppedByYou)) => break,
            ServerFrame::Messages { records } => assert!(!records.into_iter().any(|row| {
                matches!(row.event, ChatEvent::Posted { from: Address::System { agent, .. }, .. } if agent == b)
            })),
            _ => {}
        }
    }
    Ok(format!(
        "two flashes did not ping or consume the live slot; repeat refused; edits kept the live slot; thread reply reached both workers, froze only the dealt version; new message #{}",
        id.0.0
    ))
}

async fn run(paths: &RuntimePaths, root: &Path) -> anyhow::Result<String> {
    let mut stream = connect_chat(paths).await?;
    let ServerFrame::Hello { head, .. } = read_frame(&mut stream).await? else {
        anyhow::bail!("no chat hello");
    };
    write_frame(&mut stream, &ClientFrame::Follow { since: head }).await?;
    let config = ProjectConfig {
        place: ProjectPlace::parse(root.to_str().unwrap()).unwrap(),
        mode: ProjectMode::One,
        runtime: RuntimeLabel::default(),
    };
    let id = call(
        paths,
        CreateProject {
            name: ProjectName::parse("tide").unwrap(),
            config: config.clone(),
        },
    )
    .await?;
    let project = id.0;
    let thread = post(paths, 8, Some(project), "Check the real mail path.").await?;
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
            id,
            name: "tide".to_owned(),
            config,
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
        worksets::TellAgent {
            agent,
            parent: Some(thread),
            text: "Please run the check.".into(),
        },
    )
    .await?;
    let origin_result = wait_for_mail(&mut stream, |row| matches!(&row.event,
        ChatEvent::Posted { parent: Some(parent), from: Address::Agent(from), voice: Voice::Message, .. }
        if *parent == thread && *from == agent
    )).await?;
    let tool_thread = post(paths, 90, Some(project), "Check the interface.").await?;
    let EvalResult { output, error, .. } = call(
        paths,
        Eval {
            workset: created.workset,
            code: format!(
                r#"
import os
from rho.tools import Chat
chat = Chat("{}")
note = await chat.send("Use polling for this check.", "message", thread={})
assert isinstance(note, int)
chat.edit(note, "Use sockets for this check.")
hits = await chat.search("sockets", project={})
assert [row["message"] for row in hits] == [note]
chat.delete(note)
assert not chat.search("sockets", project={})
print(f"worker:{{os.getpid()}} note:{{note}}")
"#,
                agent.encoded(),
                tool_thread.0.0,
                project.0.0,
                project.0.0,
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
            ServerFrame::Messages { records } => {
                for row in records {
                    match row.event {
                        ChatEvent::Revised { message, .. } if message == note => edited = true,
                        ChatEvent::Deleted { message } if message == note => deleted = true,
                        _ => {}
                    }
                }
            }
            ServerFrame::Hello { .. } => {}
        }
    }
    rustix::process::kill_process(worker, rustix::process::Signal::KILL)?;
    let mut stop_mail = None;
    let stopped = loop {
        match read_frame(&mut stream).await? {
            ServerFrame::Messages { records } => {
                stop_mail = stop_mail.or_else(|| {
                    records.into_iter().find(|row| {
                        matches!(
                            row.event, ChatEvent::Posted {
                                voice: Voice::Message,
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
    let stop = rho_agent_types::Message::from_record(&stopped).context("not a post")?;
    assert!(stop.pings_user());
    let text = stop.text();
    assert!(text.starts_with("lost · "), "{text}");
    assert!(text.contains("Last transcript lines:"), "{text}");

    // The user's reply goes to the thread, with no addressee, even though
    // the card was the agent's. Its settlement names the version the user
    // saw.
    let seen = rho_agent_types::MessageVersion {
        message: MessageId(origin_result.seq),
        version: origin_result.seq,
    };
    let answer = call(
        paths,
        Post {
            client_id: 10,
            parent: Some(thread),
            settles: Some(seen),
            content: vec![ContentPart::Text {
                text: "Thanks for checking.".into(),
            }],
        },
    )
    .await?;
    let received = wait_for_mail(&mut stream, |row| row.seq == answer.0).await?;
    let reply = rho_agent_types::Message::from_record(&received).context("not a post")?;
    assert_eq!((reply.parent, reply.retry_of), (Some(thread), None));
    assert_eq!(reply.voice, Voice::Message);
    assert_eq!(reply.thread_reply.unwrap().settles, Some(seen));
    Ok(format!(
        "mail wire: project {}, fake model result #{}, edit/delete #{}, lost stop #{}, and thread-wide reply #{}",
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
        text.contains("Reply to retry, change its runtime, or start another agent."),
        "{text}"
    );
    assert!(!text.contains(root.to_str().unwrap()), "{text}");

    let mut replies = Vec::new();
    for (client_id, settles, text) in [
        (
            72,
            Some(rho_agent_types::MessageVersion {
                message: stop.id,
                version: stop.version().0,
            }),
            "Use a separate clock per test.",
        ),
        (73, None, "Keep the fix small."),
    ] {
        let reply = call(
            paths,
            Post {
                client_id,
                parent: Some(thread),
                settles,
                content: vec![ContentPart::Text { text: text.into() }],
            },
        )
        .await?;
        replies.push(reply);
        wait_for_mail(&mut stream, |row| {
            matches!(row.event, ChatEvent::Posted {
                from: Address::System { agent: stopped, stop: Stop::Failed, .. },
                parent: Some(parent), ..
            } if parent == thread && stopped == agent)
        })
        .await?;
    }

    // Replaying the same saved mail must not retry by itself.
    call(paths, RestartWorkset { agent }).await?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let log_before_reply = call(
        paths,
        AgentLog {
            agent,
            from: Wpos::ZERO,
        },
    )
    .await?;
    assert_eq!(
        log_before_reply
            .rows
            .iter()
            .filter(|(_, row)| matches!(row, LogEvent::Stopped { .. }))
            .count(),
        3
    );

    let later = post(paths, 74, Some(thread), "Actually check it twice.").await?;
    replies.push(later);
    wait_for_mail(&mut stream, |row| {
        matches!(row.event, ChatEvent::Posted {
            from: Address::System { agent: stopped, stop: Stop::Failed, .. },
            parent: Some(parent), ..
        } if parent == thread && stopped == agent)
    })
    .await?;
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
            failed, 4,
            "each new reply, but no replay, should attempt another launch: {:?}",
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
        "failed launch #{}; three new replies retried, restart alone did not, and all mail reached native in order; later cross-kind switch refused",
        stop.id.0.0
    ))
}

/// A reply in its thread is all it takes to get a stopped, failed or lost
/// agent going again: the user never has to know which agent, or how it
/// ended. The fake model answers only the brief; it fails every later user
/// turn, which makes each wake also a real `failed` stop to reply to.
async fn run_reply_wakes(paths: &RuntimePaths, root: &Path) -> anyhow::Result<String> {
    let mut stream = connect_chat(paths).await?;
    let ServerFrame::Hello { head, .. } = read_frame(&mut stream).await? else {
        anyhow::bail!("no chat hello");
    };
    write_frame(&mut stream, &ClientFrame::Follow { since: head }).await?;
    let project = post(paths, 130, None, "harbor").await?;
    let thread = post(paths, 131, Some(project), "Keep the harbor clock honest.").await?;
    let cwd = root.join("harbor");
    std::fs::create_dir_all(&cwd)?;
    let created = call(paths, CreateWorkset {
        kind: WorksetKind::Host { cwd: cwd.to_str().unwrap().into() },
        code: format!(
            r#"new_agent(runtime={{"kind": "native", "model": "sol", "effort": "medium"}}, origin="chat:{}")"#,
            thread.0.0
        ),
    }).await?;
    let agent = created
        .result
        .agent
        .with_context(|| format!("agent not created: {:?}", created.result.error))?;
    wait_for_mail(&mut stream, |row| {
        matches!(&row.event, ChatEvent::Posted {
            parent: Some(parent), from: Address::Agent(from), voice: Voice::Message, ..
        } if *parent == thread && *from == agent)
    })
    .await?;
    let stopped_after = |after: MessageId, stop: Stop| {
        move |row: &ChatRecord| {
            row.seq > after.0
                && matches!(&row.event, ChatEvent::Posted {
                    parent: Some(parent), from: Address::System { agent: stopped, stop: said, .. }, ..
                } if *parent == thread && *stopped == agent && *said == stop)
        }
    };

    // (a) Stopped by the user: a reply wakes the same agent with the reply.
    call(paths, StopAgent { agent }).await?;
    let after_stop = post(paths, 132, Some(thread), "Carry on after the stop.").await?;
    wait_for_delivery(paths, agent, after_stop).await?;
    // The fake model refuses that turn, so the agent is now `failed`.
    wait_for_mail(&mut stream, stopped_after(after_stop, Stop::Failed)).await?;

    // (b) Failed: a reply retries it, with the reply.
    let after_failure = post(paths, 133, Some(thread), "Try that again.").await?;
    wait_for_delivery(paths, agent, after_failure).await?;
    wait_for_mail(&mut stream, stopped_after(after_failure, Stop::Failed)).await?;

    // Lost: its process ended under it. A reply brings it back on a new one.
    let EvalResult { output, error, .. } = call(
        paths,
        Eval {
            workset: created.workset,
            code: "import os\nprint(os.getpid())".into(),
        },
    )
    .await?;
    anyhow::ensure!(error.is_none(), "{error:?}");
    let worker = rustix::process::Pid::from_raw(output.trim().parse()?).context("no worker pid")?;
    rustix::process::kill_process(worker, rustix::process::Signal::KILL)?;
    wait_for_mail(&mut stream, stopped_after(after_failure, Stop::Lost)).await?;
    let after_loss = post(paths, 134, Some(thread), "Carry on after the loss.").await?;
    wait_for_delivery(paths, agent, after_loss).await?;
    Ok(format!(
        "reply wakes: agent {agent:?} read #{} after a stop, #{} after a failure and #{} after a loss",
        after_stop.0.0, after_failure.0.0, after_loss.0.0
    ))
}

async fn run_peer_chain(paths: &RuntimePaths, root: &Path) -> anyhow::Result<String> {
    let mut stream = connect_chat(paths).await?;
    let ServerFrame::Hello { head, host, .. } = read_frame(&mut stream).await? else {
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

    let mut history = HostChat::default();
    let mut brief = None;
    let mut peer = None;
    let ask = loop {
        if let ServerFrame::Messages { records } = read_frame(&mut stream).await? {
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
                        let Voice::ToAgent(child) = mail.voice else {
                            anyhow::bail!("the brief is not to the child")
                        };
                        brief = Some((mail.id, child));
                    }
                    "MAIL-RIG-PEER-RESULT" => {
                        let (brief, child) = brief.context("child sent before brief")?;
                        assert_eq!(mail.parent, Some(brief));
                        assert_eq!(mail.from, Address::Agent(child));
                        assert_eq!(mail.voice, Voice::ToAgent(leader));
                        let claims = cards::claims(
                            &cards::Host {
                                endpoint: host,
                                chat: &history,
                                name: "local",
                                names: &rho_chat::names::ProjectNames::default(),
                            },
                            &rho_dealer::Marks::default(),
                            jiff::Timestamp::now(),
                            None,
                            [],
                        );
                        assert!(claims.is_empty(), "a message to an agent made a card");
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
            settles: Some(rho_agent_types::MessageVersion {
                message: ask,
                version: ask.0,
            }),
            content: vec![ContentPart::Text {
                text: "MAIL-RIG-USER-ANSWER".into(),
            }],
        },
    )
    .await?;
    let row = wait_for_mail(&mut stream, |row| row.seq == answer.0).await?;
    let answer_msg = rho_agent_types::Message::from_record(&row).unwrap();
    assert_eq!(
        (answer_msg.parent, answer_msg.retry_of, answer_msg.from),
        (Some(work), None, Address::User)
    );
    assert_eq!(
        answer_msg.thread_reply.unwrap().settles.unwrap().message,
        ask
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
            back: rho_agent_types::Rewind::Turns(2),
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
            settles: None,
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
        if let ServerFrame::Messages { records } = read_frame(stream).await?
            && let Some(found) = records.into_iter().find(&predicate)
        {
            return Ok(found);
        }
    }
}

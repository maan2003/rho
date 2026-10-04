//! Headless evaluations run the real agent loop against an isolated, temporary
//! DB.
use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use rho_agent::db::{AgentReadTxnExt as _, AgentWriteTxnExt as _};
use rho_agent::entry::{Entry, Party};
use rho_agent::{AgentEvent, StartPlace};
use rho_agent_types::{AgentId, AgentRole, EngineerIntelligence, Place};
use rho_fs_view::{UserEnvironment, Worksets};
use serde_json::{Value, json};

#[derive(Clone, clap::Args)]
pub(crate) struct EvalArgs {
    /// Task text. Alternatively use --prompt-file (use - for stdin). With
    /// --rewind-to and no task, the model is asked again as after a restart.
    #[arg(
        required_unless_present_any = ["prompt_file", "rewind_to"],
        conflicts_with = "prompt_file"
    )]
    pub prompt: Option<String>,
    #[arg(long)]
    pub prompt_file: Option<PathBuf>,
    /// Engineer role. high-eng selects GPT-6 Astra; opus-eng and fable-eng
    /// run Claude Code and need --claude-accounts. Every role works in the
    /// Python notebook.
    #[arg(long, default_value = "high-eng", value_parser = ["mini-eng", "med-eng", "high-eng", "opus-eng", "fable-eng"])]
    pub role: String,
    /// Use this LIVE working directory. Defaults to an empty temporary
    /// directory.
    #[arg(long)]
    pub workdir: Option<PathBuf>,
    /// Keep the run's database and worksets in this directory instead of
    /// deleting them on exit.
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
    /// Log Claude in through the accounts in this directory, such as
    /// ~/.claude-accounts. Without it Claude starts unauthenticated.
    #[arg(long)]
    pub claude_accounts: Option<PathBuf>,
    /// The account under --claude-accounts that Claude agents run on.
    #[arg(long, default_value = rho_claude::accounts::DEFAULT_ACCOUNT)]
    pub claude_account: String,
    /// Agent execution timeout; excludes setup and blocked output writes.
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=86400))]
    pub timeout: u64,
    /// Require this substring in the final answer (repeatable).
    #[arg(long)]
    pub expect: Vec<String>,
    /// Require an actual model call to this tool (repeatable; e.g. exec).
    #[arg(long)]
    pub require_tool: Vec<String>,
    /// Continue this agent from --state-dir instead of creating one; the
    /// task text becomes its next user message.
    #[arg(long, requires = "state_dir")]
    pub resume: Option<String>,
    /// Before resuming, rewind the agent to just after it sent its Nth
    /// request; the task then asks the model again from there.
    #[arg(long, requires = "resume")]
    pub rewind_to: Option<usize>,
    /// Each time the agent sends its Nth request, snapshot --workdir, which
    /// must be a bcachefs subvolume, read-only into this directory as
    /// r<N>: the files that request was asked about, for a later
    /// --rewind-to N.
    #[arg(long, requires = "workdir")]
    pub checkpoints: Option<PathBuf>,
    /// End the run once the agent has sent this many requests.
    #[arg(long)]
    pub max_requests: Option<usize>,
}

enum KeptOrTemp {
    Kept(PathBuf),
    Temp(tempfile::TempDir),
}

impl KeptOrTemp {
    fn path(&self) -> &std::path::Path {
        match self {
            Self::Kept(path) => path,
            Self::Temp(dir) => dir.path(),
        }
    }
}

/// Writes one event, stamped with milliseconds since the evaluation began.
fn emit(mut value: Value) -> Result<()> {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(Instant::now);
    value["ms"] = json!(start.elapsed().as_millis() as u64);
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, &value)?;
    writeln!(stdout)?;
    stdout.flush()?;
    Ok(())
}

pub(crate) async fn run(args: EvalArgs) -> Result<()> {
    use tokio::io::AsyncReadExt as _;
    let prompt = match (&args.prompt, &args.prompt_file) {
        (None, None) => None,
        (Some(prompt), _) => Some(prompt.clone()),
        (_, Some(path)) if path.as_os_str() == "-" => {
            let mut prompt = String::new();
            tokio::io::stdin()
                .take(1024 * 1024 + 1)
                .read_to_string(&mut prompt)
                .await?;
            Some(prompt)
        }
        (_, Some(path)) => {
            let mut prompt = String::new();
            tokio::fs::File::open(path)
                .await
                .context("open evaluation prompt")?
                .take(1024 * 1024 + 1)
                .read_to_string(&mut prompt)
                .await?;
            Some(prompt)
        }
    };
    anyhow::ensure!(
        prompt
            .as_ref()
            .is_none_or(|prompt| !prompt.trim().is_empty() && prompt.len() <= 1024 * 1024),
        "task must contain 1 byte through 1 MiB of text"
    );
    let started = Instant::now();
    let temp = match &args.state_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir).context("create evaluation state directory")?;
            KeptOrTemp::Kept(dir.canonicalize()?)
        }
        None => KeptOrTemp::Temp(tempfile::tempdir().context("create isolated evaluation state")?),
    };
    let workdir = match args.workdir {
        Some(ref path) => path
            .canonicalize()
            .context("resolve live evaluation workdir")?,
        None => {
            let path = temp.path().join("work");
            std::fs::create_dir(&path)?;
            path
        }
    };
    let env = UserEnvironment::new(std::env::vars_os().collect());
    // An eval adopts its directory as a workset; no
    // mirror keeper runs, so `git` inside is the plain one.
    let worksets = Worksets::open(
        temp.path().join("state"),
        env,
        Default::default(),
        rho_fs_view::StoreService::None,
    )
    .await?;
    let db = rho_db::RhoDb::open(temp.path().join("eval.redb"));
    let resume = match &args.resume {
        Some(handle) => {
            let read = db.read();
            let domain = rho_agent_types::AgentIdDomain(read.machine_seed());
            let id = match AgentId::from_prefix(
                handle.trim(),
                read.last_agent_counter() + 1,
                &domain,
            )? {
                prefix_id::PrefixResolution::Unique(id) => id,
                prefix_id::PrefixResolution::Ambiguous { .. } => {
                    anyhow::bail!("ambiguous agent id {handle}")
                }
                prefix_id::PrefixResolution::NotFound => anyhow::bail!("no agent with id {handle}"),
            };
            let workset = read.get_agent(id).place().workset.clone();
            drop(read);
            if let Some(n) = args.rewind_to {
                let sent = requests_sent(&db, id);
                let Some(&at) = sent.get(n.wrapping_sub(1)) else {
                    anyhow::bail!("agent {handle} sent {} requests, not {n}", sent.len());
                };
                let mut write = db.write().await;
                write.rewind_agent(rho_agent_types::UnixMs::now(), id, at.next());
                write.commit();
            }
            Some((id, workset))
        }
        None => None,
    };
    // A resumed agent finds its workset under the id it recorded.
    let workset = match &resume {
        Some((_, workset)) => worksets.adopt_as(workset.clone(), &workdir)?,
        None => worksets.adopt(&workdir)?,
    };
    let place = Place {
        workset: workset.id().to_owned(),
        cwd: rho_fs_view::MOUNT_ROOT.into(),
        origin: None,
    };
    rho_inference::ensure_crypto_provider();
    let inference = rho_inference::Accounts::new(db.clone()).await?;
    // An eval keeps its Claude state in its own directory; only a login,
    // when asked for, comes from the user's accounts.
    let claude_home = camino::Utf8PathBuf::try_from(temp.path().join("claude"))?;
    let claude = match &args.claude_accounts {
        Some(accounts) => {
            let accounts = camino::Utf8PathBuf::try_from(accounts.canonicalize()?)?;
            let claude = rho_claude::accounts::ClaudePaths::at_with_accounts(claude_home, accounts);
            anyhow::ensure!(
                claude.list()?.contains(&args.claude_account),
                "no Claude account {:?} under --claude-accounts",
                args.claude_account
            );
            let mut write = db.write().await;
            write.set_claude_account(&args.claude_account);
            write.commit();
            claude
        }
        None => rho_claude::accounts::ClaudePaths::at(claude_home),
    };
    let pool = rho_agent::host::pool::AgentPool::new(
        db.clone(),
        std::sync::Arc::new(inference),
        worksets,
        claude,
    )
    .await;
    let role = AgentRole::Engineer {
        intelligence: match args.role.as_str() {
            "mini-eng" => EngineerIntelligence::Mini,
            "med-eng" => EngineerIntelligence::Medium,
            "high-eng" => EngineerIntelligence::High,
            "opus-eng" => EngineerIntelligence::Medium1,
            "fable-eng" => EngineerIntelligence::High1,
            _ => unreachable!("clap validates evaluation roles"),
        },
    };
    let mut feed = rho_agent::journal::feed(&db);
    let (id, agent) = match resume {
        Some((id, _)) => {
            let (id, agent, _) = pool.load(id).await?;
            (id, agent)
        }
        None => {
            pool.create(role, Some("CLI evaluation".into()), StartPlace::new(place))
                .await?
        }
    };
    // Drop is also cancellation, including early output/connection failures.
    struct CancelOnDrop(rho_agent::host::AgentClient);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let _cancel = CancelOnDrop(agent.clone());
    let model = match db
        .read()
        .agent_event(id, rho_agent::log::AgentEventPos::new(0))
    {
        Some(AgentEvent::Created { binding, .. }) => {
            binding.deep_model().map(|model| model.as_str())
        }
        _ => None,
    };
    emit(json!({"type":"start", "role":args.role, "model":model, "workdir":workdir}))?;
    // Subscribed before the prompt goes, so its work is seen starting.
    let mut statuses = agent.statuses();
    match prompt {
        Some(prompt) => agent.send_user_message(prompt),
        None => agent.retry(),
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(args.timeout);
    let mut requests = 0;
    let mut calls = BTreeSet::new();
    let mut final_answer = String::new();
    let mut tokens = [0u64; 3];
    let mut working = false;
    // Once the agent stops, only the rows it wrote first are left to read:
    // each was announced before the status that followed it.
    let mut finished: Option<Result<(), String>> = None;
    // An ended turn can still be woken: notify() lands about 2 s later and an
    // unclaimed task failure about 20 s later. It counts as done only after
    // staying idle this long, and its elapsed time stops where idling began.
    const SETTLE: Duration = Duration::from_secs(25);
    let mut idle_since: Option<Instant> = None;
    let outcome = loop {
        let event = match &finished {
            Some(outcome) => match feed.try_recv() {
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break outcome.clone(),
                event => event.map_err(|error| error.to_string()),
            },
            None => tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break Err("Evaluation timed out".to_owned()),
                _ = tokio::signal::ctrl_c() => break Err("Evaluation interrupted".to_owned()),
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(idle_since.unwrap_or(started) + SETTLE)), if idle_since.is_some() => {
                    finished = Some(Ok(()));
                    continue;
                }
                changed = statuses.changed() => {
                    if changed.is_err() {
                        break Err("Evaluation agent stopped".to_owned());
                    }
                    let runtime = statuses.borrow_and_update().runtime.clone();
                    working |= runtime.is_working();
                    // Between cells the agent is idle but due a check-in; it is
                    // done only once it ends its turn, archives, stops or fails.
                    // A turn ended over running tasks is not done: no human
                    // answers here, but those tasks can still wake it.
                    let responding = runtime.inference == rho_agent::InferenceState::Responding;
                    let stopped = !runtime.is_working() && runtime.checkin_at.is_none();
                    let ended = runtime.awaiting_human && runtime.running_tasks == 0;
                    if let rho_agent::InferenceState::Failed { error } = runtime.inference {
                        finished = Some(Err(error));
                    } else if working && runtime.archived {
                        finished = Some(Ok(()));
                    } else if working && !responding && (ended || stopped) {
                        idle_since.get_or_insert_with(Instant::now);
                    } else {
                        idle_since = None;
                    }
                    continue;
                }
                event = feed.recv() => event.map_err(|error| error.to_string()),
            },
        };
        let appended = match event {
            Ok(event) => event,
            Err(error) => break Err(format!("Evaluation feed lost: {error}")),
        };
        let agent_id = appended.agent_id;
        let Some(AgentEvent::Entry(entry)) = db.read().agent_event(agent_id, appended.pos.into())
        else {
            continue;
        };
        // Agents it spawns are part of the run: their work and tokens count.
        let agent = format!("{agent_id:?}");
        match entry {
            Entry::RequestSent { report, .. } => {
                requests += 1;
                emit(json!({"type":"request", "agent":agent, "number":requests}))?;
                if agent_id == id {
                    // Numbered by the agent's own history, so a resumed run
                    // continues the numbers it was rewound to.
                    let n = requests_sent(&db, id).len();
                    if let Some(dir) = &args.checkpoints {
                        let status = std::process::Command::new("bcachefs")
                            .args(["subvolume", "snapshot", "-r"])
                            .arg(&workdir)
                            .arg(dir.join(format!("r{n}")))
                            .status()
                            .context("run bcachefs")?;
                        anyhow::ensure!(status.success(), "snapshot for request {n} failed");
                    }
                    if args.max_requests.is_some_and(|max| n > max) {
                        break Err(format!("Stopped before request {n}"));
                    }
                }
                let prior = db.read().agent_input_carry(agent_id, appended.pos.into());
                let results = rho_inference::transcript::report_results(&report, prior.as_ref());
                for result in results {
                    emit(
                        json!({"type":"notebook_report", "agent":agent, "id":result.display_id(), "output":result.text}),
                    )?;
                }
            }
            Entry::Step { carry, usage, .. } => {
                for call in carry.display_calls() {
                    calls.insert("exec".to_owned());
                    emit(
                        json!({"type":"tool_call", "agent":agent, "id":call.display_id(), "name":"exec", "arguments":call.code}),
                    )?;
                }
                if let Some(usage) = usage {
                    tokens[0] += usage.input_tokens;
                    tokens[1] += usage.cache_read_tokens;
                    tokens[2] += usage.output_tokens;
                    emit(
                        json!({"type":"usage", "agent":agent, "input_tokens":usage.input_tokens,
                    "cached_input_tokens":usage.cache_read_tokens,"output_tokens":usage.output_tokens}),
                    )?;
                }
            }
            Entry::Sent {
                to: Party::Human,
                text,
                ..
            } if agent_id == id => {
                if !final_answer.is_empty() {
                    final_answer.push('\n');
                }
                final_answer.push_str(&text);
                emit(json!({"type":"message", "agent":agent, "text":text}))?;
            }
            Entry::Sent { to, text, .. } => {
                emit(
                    json!({"type":"message", "agent":agent, "to":format!("{to:?}"), "text":text}),
                )?;
            }
            _ => {}
        }
    };
    agent.cancel();
    // Every runtime records provider usage here, Claude Code included, which
    // reports no steps of its own.
    let usage: Vec<Value> = db
        .read()
        .global_agent_usage(Default::default())
        .into_iter()
        .map(|(model, total)| {
            json!({"model":format!("{model:?}"), "requests":total.requests,
            "input_tokens":total.input_tokens, "cached_input_tokens":total.cache_read_tokens,
            "cache_write_tokens":total.cache_write_tokens,
            "cache_write_1h_tokens":total.cache_write_1h_tokens, "output_tokens":total.output_tokens})
        })
        .collect();
    let mut failures = outcome.err().into_iter().collect::<Vec<_>>();
    failures.extend(checks(
        &args.expect,
        &args.require_tool,
        &final_answer,
        &calls,
    ));
    emit(
        json!({"type":"summary", "passed":failures.is_empty(),"failures":failures,"requests":requests,"input_tokens":tokens[0],"cached_input_tokens":tokens[1],"output_tokens":tokens[2],"usage_by_model":usage,"tools_called":calls,"elapsed_seconds":idle_since.unwrap_or_else(Instant::now).duration_since(started).as_secs_f64(),"final_answer":final_answer}),
    )?;
    anyhow::ensure!(
        failures.is_empty(),
        "evaluation failed: {}",
        failures.join("; ")
    );
    Ok(())
}

/// Where each request the agent's visible history sent was recorded.
fn requests_sent(db: &rho_db::RhoDb, id: AgentId) -> Vec<rho_agent::log::AgentEventPos> {
    db.read()
        .agent_event_records(id)
        .1
        .into_iter()
        .filter(|(_, event)| matches!(event, AgentEvent::Entry(Entry::RequestSent { .. })))
        .map(|(pos, _)| pos)
        .collect()
}

fn checks(
    expect: &[String],
    required_tools: &[String],
    answer: &str,
    calls: &BTreeSet<String>,
) -> Vec<String> {
    expect
        .iter()
        .filter(|text| !answer.contains(text.as_str()))
        .map(|text| format!("Final answer missing {text:?}"))
        .chain(
            required_tools
                .iter()
                .filter(|name| !calls.contains(name.as_str()))
                .map(|name| format!("Tool was not called: {name}")),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checks_require_observed_tools_not_claims_in_prose() {
        assert_eq!(
            checks(
                &["PASS".into()],
                &["exec".into()],
                "PASS used exec",
                &BTreeSet::new()
            ),
            vec!["Tool was not called: exec"]
        );
        assert!(
            checks(
                &["PASS".into()],
                &["exec".into()],
                "PASS",
                &BTreeSet::from(["exec".into()])
            )
            .is_empty()
        );
        assert_eq!(
            checks(&["PASS".into()], &[], "FAIL", &BTreeSet::new()).len(),
            1
        );
    }
}

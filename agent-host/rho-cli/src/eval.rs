//! Headless evaluations run the real agent loop against an isolated, temporary
//! DB.
use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use rho_agent::db::{AgentReadTxnExt as _, AgentWriteTxnExt as _};
use rho_agent::entry::Entry;
use rho_agent::log::TranscriptLine;
use rho_agent::record::{ClaudeRow, Event};
use rho_agent::store::{Appended, StoreRead as _};
use rho_agent_types::{Address, AgentId, ContentPart, WorksetId, WorksetKind, Wpos};
use rho_agents_client::snippets;
use rho_fs_view::{UserEnvironment, Worksets};
use serde_json::{Value, json};

#[derive(Clone, clap::Args)]
pub(crate) struct EvalArgs {
    /// Task text. Alternatively use --prompt-file (use - for stdin).
    #[arg(
        required_unless_present = "prompt_file",
        conflicts_with = "prompt_file"
    )]
    pub prompt: Option<String>,
    #[arg(long)]
    pub prompt_file: Option<PathBuf>,
    /// Engineer runtime. mini-eng runs GPT-6.1 Terra, med-eng GPT-6.1 Sol,
    /// high-eng GPT-6 Astra; opus-eng and fable-eng run Claude Code and
    /// need --claude-accounts. Every runtime works in the Python notebook.
    #[arg(long, default_value = "high-eng", value_parser = ["mini-eng", "med-eng", "high-eng", "opus-eng", "fable-eng"])]
    pub role: String,
    /// Work in this LIVE directory, as a host workset. Defaults to a fresh
    /// isolated workset.
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
    /// Override the OpenAI Responses base URL (isolated QA providers).
    #[arg(long, value_name = "URL")]
    pub openai_base_url: Option<String>,
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
    /// Each time the agent sends its Nth request, snapshot --workdir, which
    /// must be a bcachefs subvolume, read-only into this directory as
    /// r<N>: the files that request was asked about.
    #[arg(long, requires = "workdir")]
    pub checkpoints: Option<PathBuf>,
    /// End the run once the agent has sent this many requests.
    #[arg(long)]
    pub max_requests: Option<usize>,
}

/// The runtime each role names, as the policy's `runtime=` takes it.
fn role_runtime(role: &str) -> snippets::Runtime {
    let label = match role {
        "mini-eng" => "terra-xhigh",
        "med-eng" => "sol-medium",
        "high-eng" => "astra-high",
        "opus-eng" => "opus-medium",
        "fable-eng" => "fable-medium",
        _ => unreachable!("clap validates evaluation roles"),
    };
    snippets::Runtime::parse(label).expect("every role names a runtime the policy takes")
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
        (Some(prompt), _) => prompt.clone(),
        (None, Some(path)) if path.as_os_str() == "-" => {
            let mut prompt = String::new();
            tokio::io::stdin()
                .take(1024 * 1024 + 1)
                .read_to_string(&mut prompt)
                .await?;
            prompt
        }
        (None, Some(path)) => {
            let mut prompt = String::new();
            tokio::fs::File::open(path)
                .await
                .context("open evaluation prompt")?
                .take(1024 * 1024 + 1)
                .read_to_string(&mut prompt)
                .await?;
            prompt
        }
        (None, None) => unreachable!("clap requires a prompt or a prompt file"),
    };
    anyhow::ensure!(
        !prompt.trim().is_empty() && prompt.len() <= 1024 * 1024,
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
    let env = UserEnvironment::new(std::env::vars_os().collect());
    // No mirror keeper runs, so `git` inside is the plain one.
    let worksets = Worksets::open(
        temp.path().join("state"),
        env,
        Default::default(),
        rho_fs_view::StoreService::None,
    )
    .await?;
    let db = rho_db::RhoDb::open(temp.path().join("eval.redb"));
    rho_inference::ensure_crypto_provider();
    let inference = match &args.openai_base_url {
        Some(endpoint) => {
            rho_inference::Accounts::new_with_config(
                db.clone(),
                rho_inference::InferenceConfig::with_responses_base_url(endpoint.as_str())?,
            )
            .await?
        }
        None => rho_inference::Accounts::new(db.clone()).await?,
    };
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
        None,
    )
    .await;
    // Subscribed before the agent exists, so none of its rows go unseen.
    let mut feed = rho_agent::store::feed(&db);
    let id = match &args.resume {
        Some(handle) => {
            let read = db.read();
            let domain = rho_agent_types::AgentIdDomain(read.machine_seed());
            match AgentId::from_prefix(handle.trim(), read.last_agent_counter() + 1, &domain)? {
                prefix_id::PrefixResolution::Unique(id) if read.agent(id).is_some() => id,
                prefix_id::PrefixResolution::Ambiguous { .. } => {
                    anyhow::bail!("ambiguous agent id {handle}")
                }
                _ => anyhow::bail!("no agent with id {handle}"),
            }
        }
        // A live workdir is a host workset; the default is a fresh
        // isolated one.
        None => {
            let kind = match &args.workdir {
                Some(path) => WorksetKind::Host {
                    cwd: camino::Utf8PathBuf::try_from(
                        path.canonicalize()
                            .context("resolve live evaluation workdir")?,
                    )?,
                },
                None => WorksetKind::Isolated,
            };
            let code = snippets::new_agent_in(
                &rho_agent::host::pool::workset_root(&kind),
                &role_runtime(&args.role),
            );
            let (workset, evaluated) = pool.create_workset(kind, code, None).await?;
            if let Some(error) = evaluated.error {
                anyhow::bail!("creating the evaluation agent failed: {error}");
            }
            *db.read()
                .workset(workset)
                .context("the new workset has no head")?
                .agents
                .first()
                .context("the creation code made no agent")?
        }
    };
    let head = db
        .read()
        .agent(id)
        .context("the evaluation agent has no head")?;
    let workset = head.workset;
    // Where the agent's files are, as this process sees them.
    let workdir = match db.read().workset(workset).context("no workset")?.kind {
        WorksetKind::Isolated => pool
            .worksets()
            .open_workset(workset)
            .await?
            .root()
            .to_owned(),
        WorksetKind::Host { cwd } => cwd,
    };
    let (_, agent, _) = pool.load(id).await?;
    emit(
        json!({"type":"start", "role":args.role, "agent":format!("{id:?}"),
        "runtime":format!("{:?}", head.spec.runtime), "workdir":workdir.as_str()}),
    )?;
    // Subscribed before the prompt goes, so its work is seen starting.
    let mut statuses = agent.statuses();
    let client_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos() as u64;
    pool.post_user_mail(
        client_id,
        id,
        None,
        vec![ContentPart::Text { text: prompt }],
    )
    .await?;
    let mut watch = Watch {
        args: &args,
        db: &db,
        id,
        workset,
        workdir: &workdir,
        requests: 0,
        calls: BTreeSet::new(),
        final_answer: String::new(),
        tokens: [0; 3],
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(args.timeout);
    let mut working = false;
    // Once the agent stops, only the rows it wrote first are left to read:
    // each was announced before the status that followed it.
    let mut finished: Option<Result<(), String>> = None;
    // An ended turn can still be woken: notify() lands about 2 s later and an
    // unclaimed task failure about 20 s later. It counts as done only after
    // staying idle this long, and its elapsed time stops where idling began.
    const SETTLE: Duration = Duration::from_secs(25);
    let mut idle_since: Option<Instant> = None;
    // An early output failure leaves the loop too, and the agent is
    // cancelled either way.
    let outcome = async {
        Ok::<_, anyhow::Error>(loop {
            let event = match &finished {
                Some(outcome) => match feed.try_recv() {
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                        break outcome.clone();
                    }
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
            if let Some(stop) = watch.observe(appended)? {
                break Err(stop);
            }
        })
    }
    .await;
    pool.cancel(id).await?;
    let outcome = outcome?;
    // Every runtime records provider usage here, Claude Code included.
    pool.flush_agent_usage(None).await;
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
    let Watch {
        requests,
        calls,
        final_answer,
        tokens,
        ..
    } = watch;
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

/// What the run has seen of its workset's rows and mail.
struct Watch<'a> {
    args: &'a EvalArgs,
    db: &'a rho_db::RhoDb,
    /// The agent the task went to.
    id: AgentId,
    workset: WorksetId,
    workdir: &'a camino::Utf8Path,
    requests: usize,
    calls: BTreeSet<String>,
    final_answer: String,
    tokens: [u64; 3],
}

impl Watch<'_> {
    /// Emits what one append says; `Some` is why the run stops here.
    fn observe(&mut self, appended: Appended) -> Result<Option<String>> {
        match appended {
            Appended::Row { workset, pos } if workset == self.workset => {
                let Some(record) = self.db.read().workset_row(workset, pos) else {
                    return Ok(None);
                };
                match record.event {
                    Event::Native { agent, entry } => self.entry(agent, pos, entry),
                    Event::Claude { agent, row } => self.claude(agent, pos, row),
                    _ => Ok(None),
                }
            }
            Appended::Mail(mail) => {
                let Some(mail) = self.db.read().mail(mail) else {
                    return Ok(None);
                };
                let Address::Agent(from) = mail.from else {
                    return Ok(None);
                };
                // Agents it spawns are part of the run: their mail counts.
                if self.db.read().agent(from).map(|head| head.workset) != Some(self.workset) {
                    return Ok(None);
                }
                let text = text_of(&mail.content);
                let agent = format!("{from:?}");
                if from == self.id && mail.to == Address::User {
                    if !self.final_answer.is_empty() {
                        self.final_answer.push('\n');
                    }
                    self.final_answer.push_str(&text);
                    emit(json!({"type":"message", "agent":agent, "text":text}))?;
                } else {
                    emit(
                        json!({"type":"message", "agent":agent, "to":format!("{:?}", mail.to), "text":text}),
                    )?;
                }
                Ok(None)
            }
            Appended::Row { .. } | Appended::WorksetDeleted(_) => Ok(None),
        }
    }

    fn entry(&mut self, agent_id: AgentId, pos: Wpos, entry: Entry) -> Result<Option<String>> {
        // Agents it spawns are part of the run: their work and tokens count.
        let agent = format!("{agent_id:?}");
        match entry {
            Entry::RequestSent { report, .. } => {
                if let Some(stop) = self.request(agent_id, pos)? {
                    return Ok(Some(stop));
                }
                let rho_inference::transcript::Before {
                    carry: prior,
                    inputs,
                } = rho_inference::transcript::before_at(&self.db.read(), agent_id, pos);
                let results =
                    rho_inference::transcript::report_results(&report, inputs, prior.as_ref());
                for result in results {
                    emit(
                        json!({"type":"notebook_report", "agent":agent, "id":result.display_id(), "output":result.text}),
                    )?;
                }
            }
            Entry::Step { carry, usage, .. } => {
                for call in carry.display_calls() {
                    self.calls.insert("exec".to_owned());
                    emit(
                        json!({"type":"tool_call", "agent":agent, "id":call.display_id(), "name":"exec", "arguments":call.code}),
                    )?;
                }
                if let Some(usage) = usage {
                    self.usage(
                        &agent,
                        usage.input_tokens,
                        usage.cache_read_tokens,
                        usage.output_tokens,
                    )?;
                }
            }
            Entry::Notice { .. } | Entry::CompactionTrigger { .. } => {}
        }
        Ok(None)
    }

    fn claude(&mut self, agent_id: AgentId, pos: Wpos, row: ClaudeRow) -> Result<Option<String>> {
        let agent = format!("{agent_id:?}");
        match row {
            ClaudeRow::RequestSent { .. } => return self.request(agent_id, pos),
            ClaudeRow::Transcript {
                line: TranscriptLine::Assistant { calls, usage, .. },
                ..
            } => {
                for call in calls {
                    self.calls.insert(call.name.clone());
                    emit(
                        json!({"type":"tool_call", "agent":agent, "id":call.id, "name":call.name, "arguments":call.arguments}),
                    )?;
                }
                if let Some(usage) = usage {
                    self.usage(
                        &agent,
                        usage.input_tokens,
                        usage.cache_read_tokens,
                        usage.output_tokens,
                    )?;
                }
            }
            _ => {}
        }
        Ok(None)
    }

    /// A request went: counted, and for the task's agent checkpointed and
    /// held to --max-requests.
    fn request(&mut self, agent_id: AgentId, pos: Wpos) -> Result<Option<String>> {
        self.requests += 1;
        emit(json!({"type":"request", "agent":format!("{agent_id:?}"), "number":self.requests}))?;
        if agent_id != self.id {
            return Ok(None);
        }
        // Numbered by the agent's own history, so a resumed run continues
        // its numbers.
        let n = self
            .db
            .read()
            .agent_rows(agent_id, Wpos(0))
            .into_iter()
            .filter(|(row, record)| {
                *row <= pos
                    && matches!(
                        record.event,
                        Event::Native {
                            entry: Entry::RequestSent { .. },
                            ..
                        } | Event::Claude {
                            row: ClaudeRow::RequestSent { .. },
                            ..
                        }
                    )
            })
            .count();
        if let Some(dir) = &self.args.checkpoints {
            let status = std::process::Command::new("bcachefs")
                .args(["subvolume", "snapshot", "-r"])
                .arg(self.workdir)
                .arg(dir.join(format!("r{n}")))
                .status()
                .context("run bcachefs")?;
            anyhow::ensure!(status.success(), "snapshot for request {n} failed");
        }
        Ok(self
            .args
            .max_requests
            .is_some_and(|max| n > max)
            .then(|| format!("Stopped before request {n}")))
    }

    fn usage(&mut self, agent: &str, input: u64, cached: u64, output: u64) -> Result<()> {
        self.tokens[0] += input;
        self.tokens[1] += cached;
        self.tokens[2] += output;
        emit(json!({"type":"usage", "agent":agent, "input_tokens":input,
            "cached_input_tokens":cached, "output_tokens":output}))
    }
}

/// The text of some content: its text parts, in order.
fn text_of(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            ContentPart::Image { .. } => None,
        })
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
    fn every_role_names_a_runtime() {
        let runtimes = ["mini-eng", "med-eng", "high-eng", "opus-eng", "fable-eng"]
            .map(|role| role_runtime(role).label());
        assert_eq!(
            runtimes,
            [
                "terra-xhigh",
                "sol-medium",
                "astra-high",
                "opus-medium",
                "fable-medium"
            ]
        );
    }

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

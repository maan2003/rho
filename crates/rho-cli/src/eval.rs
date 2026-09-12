//! Headless evaluations run the real agent loop against an isolated, temporary
//! DB.
use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use rho_agent::db::{AgentReadTxnExt as _, AgentRole, EngineerIntelligence, TurnEdge, TurnOutcome};
use rho_agent::{AgentEvent, MessageDelivery, StartWorkdir};
use rho_core::{ContextBlock, InferenceResponseItem};
use rho_workspaces::{PathOverrides, Repo, UserEnvironment};
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
    /// Native engineer role. eng-high selects GPT-6 Astra; every role works
    /// in the Python notebook.
    #[arg(long, default_value = "eng-high", value_parser = ["eng-high", "eng", "eng-cheap", "eng-low"])]
    pub role: String,
    /// Use this LIVE working directory. Defaults to an empty temporary
    /// directory.
    #[arg(long)]
    pub workdir: Option<PathBuf>,
    /// Agent execution timeout; excludes setup and blocked output writes.
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub timeout: u64,
    /// Require this substring in the final answer (repeatable).
    #[arg(long)]
    pub expect: Vec<String>,
    /// Require an actual model call to this tool (repeatable; e.g. exec).
    #[arg(long)]
    pub require_tool: Vec<String>,
}

fn emit(value: Value) -> Result<()> {
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
        (_, Some(path)) if path.as_os_str() == "-" => {
            let mut prompt = String::new();
            tokio::io::stdin()
                .take(1024 * 1024 + 1)
                .read_to_string(&mut prompt)
                .await?;
            prompt
        }
        (_, Some(path)) => {
            let mut prompt = String::new();
            tokio::fs::File::open(path)
                .await
                .context("open evaluation prompt")?
                .take(1024 * 1024 + 1)
                .read_to_string(&mut prompt)
                .await?;
            prompt
        }
        _ => anyhow::bail!("provide a task or --prompt-file"),
    };
    anyhow::ensure!(
        !prompt.trim().is_empty() && prompt.len() <= 1024 * 1024,
        "task must contain 1 byte through 1 MiB of text"
    );
    let started = Instant::now();
    let temp = tempfile::tempdir().context("create isolated evaluation state")?;
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
    let (root, is_jj) = rho_workspaces::resolve_workdir_root(&workdir)?;
    let repo = Arc::new(if is_jj {
        Repo::open_with_environment(root.as_std_path(), PathOverrides::default(), env.clone())?
    } else {
        Repo::open_plain_with_environment(
            root.as_std_path(),
            PathOverrides::default(),
            env.clone(),
        )?
    });
    let workspace = repo.user_checkout().await?;
    let db = rho_db::RhoDb::open(temp.path().join("eval.redb"));
    rho_inference::ensure_crypto_provider();
    let inference = rho_inference::Inference::new(db.clone()).await?;
    let pool = rho_agent::pool::AgentPool::new(
        db.clone(),
        inference,
        PathOverrides::default(),
        camino::Utf8PathBuf::try_from(temp.path().to_owned())?,
        // An eval runs on its own directory, not on the user's Claude state.
        rho_claude::accounts::ClaudePaths::at(camino::Utf8PathBuf::try_from(
            temp.path().join("claude"),
        )?),
        env,
    )
    .await;
    let role = AgentRole::Engineer {
        intelligence: match args.role.as_str() {
            "eng-high" => EngineerIntelligence::High,
            "eng" => EngineerIntelligence::Medium,
            "eng-cheap" => EngineerIntelligence::Cheap,
            "eng-low" => EngineerIntelligence::Low,
            _ => unreachable!("clap validates evaluation roles"),
        },
    };
    let mut feed = rho_agent::mirror::feed(&db);
    let (id, agent) = pool
        .create(
            role,
            Some("CLI evaluation".into()),
            vec![StartWorkdir::Existing(workspace)],
        )
        .await?;
    // Drop is also cancellation, including early output/connection failures.
    struct CancelOnDrop(rho_agent::pool::RunningAgent);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let _cancel = CancelOnDrop(agent.clone());
    let model = match db
        .read()
        .agent_event(id, rho_agent::db::AgentEventPos::new(0))
    {
        Some(AgentEvent::Created { binding, .. }) => {
            binding.deep_model().map(|model| model.as_str())
        }
        _ => None,
    };
    emit(json!({"type":"start", "role":args.role, "model":model, "workdir":workdir}))?;
    agent.send_user_message(prompt, MessageDelivery::Immediate);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(args.timeout);
    let mut requests = 0;
    let mut calls = BTreeSet::new();
    let mut final_answer = String::new();
    let outcome = loop {
        let event = tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break Err("Evaluation timed out".to_owned()),
            _ = tokio::signal::ctrl_c() => break Err("Evaluation interrupted".to_owned()),
            event = feed.recv() => event,
        };
        let appended = match event {
            Ok(rho_agent::mirror::Feed::Appended(event)) if event.agent_id == id => event,
            Ok(_) => continue,
            Err(error) => break Err(format!("Evaluation feed lost: {error}")),
        };
        let Some(event) = db.read().agent_event(id, appended.pos.into()) else {
            continue;
        };
        match event {
            AgentEvent::Sent { blocks, .. } => {
                requests += 1;
                emit(json!({"type":"request", "number":requests}))?;
                for block in blocks.iter() {
                    match block {
                        ContextBlock::ToolResults { results } => {
                            for result in results {
                                emit(
                                    json!({"type":"tool_result","id":result.call_id.as_str(),"output":result.body.output,"status":result.body.status}),
                                )?;
                            }
                        }
                        ContextBlock::ToolUpdate(update) => emit(
                            json!({"type":"tool_update","id":update.call_id.as_str(),"output":update.output}),
                        )?,
                        _ => {}
                    }
                }
            }
            AgentEvent::Replied { blocks, usage, .. } => {
                // The final response replaces earlier commentary for assertions.
                final_answer.clear();
                for block in blocks.iter() {
                    if let ContextBlock::InferenceResponse { items, .. } = block {
                        for item in items {
                            match item {
                                InferenceResponseItem::AssistantMessage {
                                    content, phase, ..
                                } => {
                                    let text: String = rho_core::text_content(content);
                                    if *phase != Some(rho_core::MessagePhase::Commentary) {
                                        if !final_answer.is_empty() {
                                            final_answer.push('\n');
                                        }
                                        final_answer.push_str(&text);
                                    }
                                    emit(
                                        json!({"type":"assistant", "text":text,"phase":format!("{phase:?}")}),
                                    )?;
                                }
                                InferenceResponseItem::ToolCall {
                                    id,
                                    name,
                                    arguments,
                                    ..
                                } => {
                                    calls.insert(name.as_str().to_owned());
                                    emit(
                                        json!({"type":"tool_call","id":id.as_str(),"name":name.as_str(),"arguments":arguments}),
                                    )?;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                if let Some(usage) = usage {
                    emit(
                        json!({"type":"usage","input_tokens":usage.input_tokens,"cached_input_tokens":usage.cache_read_tokens,"output_tokens":usage.output_tokens}),
                    )?;
                }
            }
            AgentEvent::Failed {
                error, retrying, ..
            } => emit(json!({"type":"provider_error","error":error,"retrying":retrying}))?,
            AgentEvent::Turn {
                edge: TurnEdge::Ended(outcome),
                ..
            } => {
                break match outcome {
                    TurnOutcome::Completed => Ok(()),
                    TurnOutcome::Cancelled => Err("Agent turn cancelled".into()),
                    TurnOutcome::Errored { message } => Err(message),
                };
            }
            _ => {}
        }
    };
    agent.cancel();
    let mut failures = outcome.err().into_iter().collect::<Vec<_>>();
    failures.extend(checks(
        &args.expect,
        &args.require_tool,
        &final_answer,
        &calls,
    ));
    emit(
        json!({"type":"summary", "passed":failures.is_empty(),"failures":failures,"requests":requests,"tools_called":calls,"elapsed_seconds":started.elapsed().as_secs_f64(),"final_answer":final_answer}),
    )?;
    anyhow::ensure!(
        failures.is_empty(),
        "evaluation failed: {}",
        failures.join("; ")
    );
    Ok(())
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

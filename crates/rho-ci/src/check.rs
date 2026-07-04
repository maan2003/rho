// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The check engine: runs a repo's configured CI jobs against provided
//! base/candidate checkouts, starting with the "main" job and spawning
//! more as in-script `selfci job start` requests arrive.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use anyhow::Context as _;
use tracing::debug;

use crate::{config, protocol, worker};

/// The revision under test, as reported to jobs via `SELFCI_*` env vars.
/// rho rebases candidates in place before checking, so a single triple
/// serves as both candidate and merged result.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub commit_id: String,
    pub change_id: String,
    /// What the submitter called it (`SELFCI_CANDIDATE_ID`).
    pub display: String,
}

/// Live progress from a running check, delivered on the calling thread.
#[derive(Debug, Clone)]
pub enum CheckEvent {
    JobStarted {
        job: String,
    },
    StepStarted {
        job: String,
        step: String,
    },
    StepFinished {
        job: String,
        step: String,
        status: protocol::StepStatus,
        duration: Duration,
    },
    JobFinished {
        job: String,
        status: protocol::JobStatus,
        duration: Duration,
    },
}

pub struct CheckOptions<'a> {
    /// The job to run. Callers read it from the repo's *base* revision
    /// (e.g. `jj file show -r <base> .config/selfci/ci.yaml`), not the
    /// candidate: a candidate must not rewrite its own checks.
    pub job: &'a config::JobConfig,
    /// Checkout of the candidate; jobs run with this as their cwd.
    pub candidate_dir: &'a Path,
    pub candidate: Candidate,
}

/// Aggregated result of one check run.
#[derive(Debug)]
pub struct CheckResult {
    /// The whole run passed: every job exited 0 with no non-ignored
    /// failed step.
    pub passed: bool,
    /// Human-readable progress log and each job's captured output —
    /// the transcript to show someone when the check fails.
    pub output: String,
    /// Every step, names prefixed `job/step`.
    pub steps: Vec<protocol::StepLogEntry>,
    pub jobs: Vec<protocol::CompletedJob>,
    pub duration: Duration,
}

/// Job bookkeeping shared between the collector, the control-socket
/// listener, and `selfci job wait` waiters.
#[derive(Default)]
struct JobStates {
    /// Names ever started; duplicate `job start` requests are rejected.
    started: HashSet<String>,
    /// Steps for each job - key is job name
    steps: HashMap<String, Vec<protocol::StepLogEntry>>,
    /// Completion status for each job
    completions: HashMap<String, protocol::JobStatus>,
}

#[derive(Clone, Default)]
pub struct SharedJobStates {
    inner: Arc<Mutex<JobStates>>,
    /// Notified when a job completion is recorded
    completed: Arc<Condvar>,
}

impl SharedJobStates {
    /// Registers the job name; false when it was already started.
    pub fn try_start(&self, name: &str) -> bool {
        self.inner.lock().unwrap().started.insert(name.to_owned())
    }

    pub fn is_started(&self, name: &str) -> bool {
        self.inner.lock().unwrap().started.contains(name)
    }

    /// Appends a Running step, finishing a still-Running predecessor;
    /// returns the predecessor's name when one was finished.
    pub fn log_step(&self, job_name: &str, step_name: &str) -> Option<String> {
        let mut guard = self.inner.lock().unwrap();
        let job_steps = guard.steps.entry(job_name.to_owned()).or_default();
        let completed_step = match job_steps.last_mut() {
            Some(prev_step) if matches!(prev_step.status, protocol::StepStatus::Running) => {
                prev_step.status = protocol::StepStatus::Success;
                Some(prev_step.name.clone())
            }
            _ => None,
        };
        job_steps.push(protocol::StepLogEntry {
            ts: std::time::SystemTime::now(),
            name: step_name.to_owned(),
            status: protocol::StepStatus::Running,
            job_started_at: None,
        });
        completed_step
    }

    /// Marks the job's last step failed, returning its name.
    pub fn mark_last_step_failed(&self, job_name: &str, ignore: bool) -> Result<String, String> {
        let mut guard = self.inner.lock().unwrap();
        let Some(job_steps) = guard.steps.get_mut(job_name) else {
            return Err(format!("Job '{}' not found", job_name));
        };
        let Some(last_step) = job_steps.last_mut() else {
            return Err(format!("No steps found for job '{}'", job_name));
        };
        last_step.status = protocol::StepStatus::Failed { ignored: ignore };
        Ok(last_step.name.clone())
    }

    /// The job's steps, with still-Running ones counted as Success (the
    /// job exited without failing them).
    fn finished_steps(&self, job_name: &str) -> Vec<protocol::StepLogEntry> {
        let guard = self.inner.lock().unwrap();
        guard
            .steps
            .get(job_name)
            .map(|steps| {
                steps
                    .iter()
                    .map(|step| {
                        let mut step = step.clone();
                        if matches!(step.status, protocol::StepStatus::Running) {
                            step.status = protocol::StepStatus::Success;
                        }
                        step
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Wait until the named job has a completion entry, then return its status
    pub fn wait_for_completion(&self, name: &str) -> protocol::JobStatus {
        let mut guard = self.inner.lock().unwrap();
        loop {
            if let Some(status) = guard.completions.get(name).cloned() {
                return status;
            }
            guard = self.completed.wait(guard).unwrap();
        }
    }

    /// Record a job completion and notify all waiters
    fn complete_job(&self, name: String, status: protocol::JobStatus) {
        let mut guard = self.inner.lock().unwrap();
        guard.completions.insert(name, status);
        self.completed.notify_all();
    }
}

/// Runs the configured CI jobs, reporting progress through `on_event`
/// (called on this thread; pass `|_| {}` when unused). `Err` is reserved
/// for failures to run at all (no config, socket setup); a red check is
/// `Ok` with `passed: false` and the transcript in `output`.
pub fn run_check(
    options: CheckOptions<'_>,
    mut on_event: impl FnMut(CheckEvent),
) -> anyhow::Result<CheckResult> {
    let CheckOptions {
        job,
        candidate_dir,
        candidate,
    } = options;

    // The socket lives in a per-run tempdir; jobs get its path via env.
    let run_dir = tempfile::Builder::new()
        .prefix("rho-ci-")
        .tempdir()
        .context("create run dir")?;
    let socket_path = run_dir.path().join("job-control.sock");
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind control socket {}", socket_path.display()))?;
    debug!(socket_path = %socket_path.display(), "Created control socket");

    let shared_job_states = SharedJobStates::default();
    let (messages_sender, messages_receiver) = mpsc::channel::<worker::JobMessage>();

    let spawn_context = worker::JobSpawnContext {
        candidate_dir: candidate_dir.to_path_buf(),
        command_prefix: job.command_prefix.clone(),
        command: job.command.clone(),
        socket_path: socket_path.clone(),
        candidate_commit_id: candidate.commit_id,
        candidate_change_id: candidate.change_id,
        candidate_id: candidate.display,
    };

    // Control socket listener thread; shut down via flag + wake-up connect.
    let listener_shutdown = Arc::new(AtomicBool::new(false));
    {
        let shared_job_states = shared_job_states.clone();
        let messages_sender = messages_sender.clone();
        let spawn_context = spawn_context.clone();
        let shutdown = Arc::clone(&listener_shutdown);
        std::thread::spawn(move || {
            worker::control_socket_listener(
                listener,
                shared_job_states,
                messages_sender,
                spawn_context,
                shutdown,
            );
        });
    }

    // Start the "main" job; ci.sh spawns any others via the socket.
    shared_job_states.try_start("main");
    worker::spawn_job(spawn_context.request("main".to_owned()), messages_sender);

    // Track running jobs and collect results
    let mut active_jobs = 0;
    let mut total_jobs = 0;
    let mut all_outputs = String::new();
    let mut all_steps = Vec::new();
    let mut all_jobs = Vec::new();
    let mut any_job_failed = false;
    let check_start = std::time::Instant::now();

    // Track step start times for duration calculation
    let mut step_start_times: HashMap<(String, String), std::time::Instant> = HashMap::new();

    let mut emit = |event: CheckEvent| on_event(event);

    fn step_emoji(status: &protocol::StepStatus) -> &'static str {
        match status {
            protocol::StepStatus::Success => "✅",
            protocol::StepStatus::Failed { ignored: true } => "⚠️",
            protocol::StepStatus::Failed { ignored: false } => "❌",
            protocol::StepStatus::Running => "⏳",
        }
    }

    for message in messages_receiver {
        match message {
            worker::JobMessage::Started { job_name } => {
                debug!(job = %job_name, "Started");
                active_jobs += 1;
                total_jobs += 1;
                let _ = writeln!(
                    all_outputs,
                    "[{}/{}] 🚀 started: {}",
                    total_jobs - active_jobs,
                    total_jobs,
                    job_name
                );
                emit(CheckEvent::JobStarted { job: job_name });
            }
            worker::JobMessage::StepStarted {
                job_name,
                step_name,
            } => {
                debug!(job = %job_name, step = %step_name, "Step started");
                let now = std::time::Instant::now();
                step_start_times.insert((job_name.clone(), step_name.clone()), now);
                emit(CheckEvent::StepStarted {
                    job: job_name,
                    step: step_name,
                });
            }
            worker::JobMessage::StepCompleted {
                job_name,
                step_name,
                status,
            } => {
                debug!(job = %job_name, step = %step_name, ?status, "Step completed");
                let jobs_completed = total_jobs - active_jobs;
                let duration = step_start_times
                    .remove(&(job_name.clone(), step_name.clone()))
                    .map(|start| start.elapsed())
                    .unwrap_or(Duration::ZERO);
                let _ = writeln!(
                    all_outputs,
                    "[{}/{}] {} {}: {}/{} ({:.3}s)",
                    jobs_completed,
                    total_jobs,
                    step_emoji(&status),
                    if matches!(status, protocol::StepStatus::Success) {
                        "passed"
                    } else {
                        "failed"
                    },
                    job_name,
                    step_name,
                    duration.as_secs_f64()
                );
                emit(CheckEvent::StepFinished {
                    job: job_name,
                    step: step_name,
                    status,
                    duration,
                });
            }
            worker::JobMessage::Completed(mut outcome) => {
                debug!(job = %outcome.job_name, exit_code = ?outcome.exit_code, "completed");

                outcome.steps = shared_job_states.finished_steps(&outcome.job_name);

                // Output completion for the last running step (if any)
                if let Some(last_step) = outcome.steps.last() {
                    // A remaining start time means the step wasn't already
                    // reported as completed by the control socket.
                    let key = (outcome.job_name.clone(), last_step.name.clone());
                    if let Some(start) = step_start_times.remove(&key) {
                        let duration = start.elapsed();
                        let jobs_completed = total_jobs - active_jobs;
                        let _ = writeln!(
                            all_outputs,
                            "[{}/{}] {} {}: {}/{} ({:.3}s)",
                            jobs_completed,
                            total_jobs,
                            step_emoji(&last_step.status),
                            if matches!(last_step.status, protocol::StepStatus::Success) {
                                "passed"
                            } else {
                                "failed"
                            },
                            outcome.job_name,
                            last_step.name,
                            duration.as_secs_f64()
                        );
                        emit(CheckEvent::StepFinished {
                            job: outcome.job_name.clone(),
                            step: last_step.name.clone(),
                            status: last_step.status.clone(),
                            duration,
                        });
                    }
                }

                // Prefix step names with job name for display
                all_steps.extend(outcome.steps.iter().map(|step| {
                    let mut step = step.clone();
                    step.name = format!("{}/{}", outcome.job_name, step.name);
                    step
                }));

                let has_failed_step = outcome.steps.iter().any(|step| {
                    matches!(step.status, protocol::StepStatus::Failed { ignored: false })
                });

                let job_failed = match outcome.exit_code {
                    Some(code) => code != 0 || has_failed_step,
                    None => true,
                };

                if job_failed {
                    any_job_failed = true;
                }

                // Record job completion status for `selfci job wait`
                let job_status = if job_failed {
                    protocol::JobStatus::Failed
                } else {
                    protocol::JobStatus::Succeeded
                };
                shared_job_states.complete_job(outcome.job_name.clone(), job_status.clone());

                all_jobs.push(protocol::CompletedJob {
                    name: outcome.job_name.clone(),
                    status: job_status.clone(),
                });

                // Output job completion status
                let jobs_completed = total_jobs - active_jobs + 1;
                let duration_secs = outcome.duration.as_secs_f64();

                if job_failed {
                    let reason = if has_failed_step {
                        "step failure"
                    } else if let Some(code) = outcome.exit_code {
                        &format!("exit code: {}", code)
                    } else {
                        "no exit code"
                    };
                    let _ = writeln!(
                        all_outputs,
                        "[{}/{}] ❌ failed: {} ({}, {:.3}s)",
                        jobs_completed, total_jobs, outcome.job_name, reason, duration_secs
                    );
                } else {
                    let _ = writeln!(
                        all_outputs,
                        "[{}/{}] ✅ passed: {} ({:.3}s)",
                        jobs_completed, total_jobs, outcome.job_name, duration_secs
                    );
                }

                if !outcome.output.is_empty() {
                    let _ = writeln!(all_outputs, "--- output: {} ---", outcome.job_name);
                    all_outputs.push_str(&outcome.output);
                    let _ = writeln!(all_outputs, "--- end output ---");
                }

                emit(CheckEvent::JobFinished {
                    job: outcome.job_name,
                    status: job_status,
                    duration: outcome.duration,
                });

                active_jobs -= 1;
                if active_jobs == 0 {
                    break;
                }
            }
        }
    }

    // Output final run summary
    let total_duration = check_start.elapsed();
    let _ = writeln!(
        all_outputs,
        "[{}/{}] {} {} ({:.3}s)",
        total_jobs,
        total_jobs,
        if any_job_failed { "❌" } else { "✅" },
        if any_job_failed { "failed" } else { "passed" },
        total_duration.as_secs_f64()
    );

    // Signal control socket listener to shut down and wake it up
    listener_shutdown.store(true, Ordering::SeqCst);
    let _ = UnixStream::connect(&socket_path); // Wake up blocking accept()

    Ok(CheckResult {
        passed: !any_job_failed,
        output: all_outputs,
        steps: all_steps,
        jobs: all_jobs,
        duration: total_duration,
    })
}

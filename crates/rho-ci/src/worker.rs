// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Job execution (one thread per job) and the control-socket listener
//! that in-script `selfci step`/`selfci job` requests land on.

use std::io::Read as _;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tracing::debug;

use crate::check::SharedJobStates;
use crate::{envs, protocol};

#[derive(Debug)]
pub struct RunJobRequest {
    pub candidate_dir: PathBuf,
    pub job_name: String,
    pub job_full_command: Vec<String>,
    pub socket_path: PathBuf,
    pub candidate_commit_id: String,
    pub candidate_change_id: String,
    pub candidate_id: String,
}

pub struct RunJobOutcome {
    pub job_name: String,
    pub exit_code: Option<i32>,
    pub output: String,
    pub duration: Duration,
    pub steps: Vec<protocol::StepLogEntry>,
}

pub enum JobMessage {
    Started {
        job_name: String,
    },
    Completed(RunJobOutcome),
    StepStarted {
        job_name: String,
        step_name: String,
    },
    StepCompleted {
        job_name: String,
        step_name: String,
        status: protocol::StepStatus,
    },
}

/// Runs the job on its own thread, reporting through `messages_sender`.
pub fn spawn_job(job: RunJobRequest, messages_sender: mpsc::Sender<JobMessage>) {
    std::thread::spawn(move || run_job(job, messages_sender));
}

fn run_job(job: RunJobRequest, messages_sender: mpsc::Sender<JobMessage>) {
    debug!(job = %job.job_name, "Running job");

    let _ = messages_sender.send(JobMessage::Started {
        job_name: job.job_name.clone(),
    });

    let start_time = Instant::now();
    let outcome = match execute(&job) {
        Ok((output, exit_code)) => RunJobOutcome {
            job_name: job.job_name,
            output,
            exit_code,
            duration: start_time.elapsed(),
            steps: Vec::new(), // Populated by the collector from SharedJobStates
        },
        Err(error) => RunJobOutcome {
            job_name: job.job_name,
            output: format!("Failed to run command: {}", error),
            exit_code: None,
            duration: start_time.elapsed(),
            steps: Vec::new(),
        },
    };
    let _ = messages_sender.send(JobMessage::Completed(outcome));
}

/// Runs the command with stdout and stderr interleaved into one captured
/// stream, the way a terminal would show them.
fn execute(job: &RunJobRequest) -> std::io::Result<(String, Option<i32>)> {
    let (mut reader, writer) = std::io::pipe()?;
    // SELFCI_MERGED_* always mirrors the candidate: rho rebases in place
    // before checking, so candidate and merged result coincide.
    let mut child = Command::new(&job.job_full_command[0])
        .args(&job.job_full_command[1..])
        .current_dir(&job.candidate_dir)
        .env(envs::SELFCI_VERSION, env!("CARGO_PKG_VERSION"))
        .env(envs::SELFCI_CANDIDATE_DIR, &job.candidate_dir)
        .env(envs::SELFCI_CANDIDATE_COMMIT_ID, &job.candidate_commit_id)
        .env(envs::SELFCI_CANDIDATE_CHANGE_ID, &job.candidate_change_id)
        .env(envs::SELFCI_CANDIDATE_ID, &job.candidate_id)
        .env(envs::SELFCI_MERGED_COMMIT_ID, &job.candidate_commit_id)
        .env(envs::SELFCI_MERGED_CHANGE_ID, &job.candidate_change_id)
        .env(envs::SELFCI_JOB_NAME, &job.job_name)
        .env(envs::SELFCI_JOB_SOCK_PATH, &job.socket_path)
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .spawn()?;

    // The parent's pipe writers were consumed by spawn; reading hits EOF
    // once the job (and anything it leaked the fd to) exits.
    let mut raw_output = Vec::new();
    reader.read_to_end(&mut raw_output)?;
    let status = child.wait()?;
    Ok((
        String::from_utf8_lossy(&raw_output).into_owned(),
        status.code(),
    ))
}

/// Context needed to spawn new jobs dynamically
#[derive(Clone)]
pub struct JobSpawnContext {
    pub candidate_dir: PathBuf,
    pub command_prefix: Vec<String>,
    pub command: String,
    pub socket_path: PathBuf,
    pub candidate_commit_id: String,
    pub candidate_change_id: String,
    pub candidate_id: String,
}

impl JobSpawnContext {
    pub(crate) fn request(&self, job_name: String) -> RunJobRequest {
        let mut full_command = self.command_prefix.clone();
        full_command.push(self.command.clone());
        RunJobRequest {
            candidate_dir: self.candidate_dir.clone(),
            job_name,
            job_full_command: full_command,
            socket_path: self.socket_path.clone(),
            candidate_commit_id: self.candidate_commit_id.clone(),
            candidate_change_id: self.candidate_change_id.clone(),
            candidate_id: self.candidate_id.clone(),
        }
    }
}

/// Control socket listener - handles step logging and job control
/// The shutdown flag is checked after each accept(). To wake up a blocking
/// accept(), the caller should connect to the socket after setting the shutdown
/// flag.
pub fn control_socket_listener(
    listener: UnixListener,
    shared_job_states: SharedJobStates,
    messages_sender: mpsc::Sender<JobMessage>,
    spawn_context: JobSpawnContext,
    shutdown: std::sync::Arc<AtomicBool>,
) {
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // Check for shutdown before processing
                if shutdown.load(Ordering::SeqCst) {
                    debug!("Control socket listener shutting down");
                    break;
                }
                let shared_job_states = shared_job_states.clone();
                let messages_sender = messages_sender.clone();
                let spawn_context = spawn_context.clone();

                // Per-connection thread: WaitForJob blocks until the job
                // completes, so requests cannot share the listener thread.
                std::thread::spawn(move || {
                    let Ok(request) = protocol::read_request(&mut stream) else {
                        return;
                    };
                    let response = match request {
                        protocol::JobControlRequest::WaitForJob { name } => {
                            if !shared_job_states.is_started(&name) {
                                protocol::JobControlResponse::JobNotFound
                            } else {
                                let status = shared_job_states.wait_for_completion(&name);
                                protocol::JobControlResponse::JobCompleted { status }
                            }
                        }
                        protocol::JobControlRequest::StartJob { name } => {
                            if !shared_job_states.try_start(&name) {
                                protocol::JobControlResponse::Error(format!(
                                    "Job '{}' already started",
                                    name
                                ))
                            } else {
                                debug!(job = %name, "Job started via control socket");
                                spawn_job(spawn_context.request(name), messages_sender.clone());
                                protocol::JobControlResponse::JobStarted
                            }
                        }
                        protocol::JobControlRequest::LogStep {
                            job_name,
                            step_name,
                        } => {
                            debug!(job = %job_name, step = %step_name, "Step logged via control socket");
                            // Starting a step completes a previous Running one.
                            let prev_step = shared_job_states.log_step(&job_name, &step_name);
                            if let Some(prev_step_name) = prev_step {
                                let _ = messages_sender.send(JobMessage::StepCompleted {
                                    job_name: job_name.clone(),
                                    step_name: prev_step_name,
                                    status: protocol::StepStatus::Success,
                                });
                            }
                            let _ = messages_sender.send(JobMessage::StepStarted {
                                job_name,
                                step_name,
                            });
                            protocol::JobControlResponse::StepLogged
                        }
                        protocol::JobControlRequest::MarkStepFailed { job_name, ignore } => {
                            match shared_job_states.mark_last_step_failed(&job_name, ignore) {
                                Ok(step_name) => {
                                    debug!(job = %job_name, step = %step_name, ignore, "Step marked as failed");
                                    let _ = messages_sender.send(JobMessage::StepCompleted {
                                        job_name,
                                        step_name,
                                        status: protocol::StepStatus::Failed { ignored: ignore },
                                    });
                                    protocol::JobControlResponse::StepMarkedFailed
                                }
                                Err(error) => protocol::JobControlResponse::Error(error),
                            }
                        }
                    };
                    let _ = protocol::write_response(&mut stream, response);
                });
            }
            Err(e) => {
                // Check for shutdown on error
                if shutdown.load(Ordering::SeqCst) {
                    debug!("Control socket listener shutting down");
                    break;
                }
                debug!("Control socket accept error: {e}");
                continue;
            }
        }
    }
}

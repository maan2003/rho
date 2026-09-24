// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The in-script client: `ci.sh` calls `selfci step`/`selfci job`, which
//! reaches this binary when it is on PATH. The CLI grammar and exit codes
//! mirror stock selfci — scripts should not be able to tell the difference.

use std::path::PathBuf;
use std::process::exit;

use crate::{envs, protocol};

/// `selfci job wait --success` on a failed job (stock selfci value).
const EXIT_JOB_WAIT_FAILED: i32 = 13;
/// `selfci job wait` on an unknown job (stock selfci value).
const EXIT_JOB_NOT_FOUND: i32 = 12;

/// Handles one `step`/`job`/`version` invocation and exits.
pub fn run_client(args: Vec<String>) -> ! {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["version"] => {
            println!("rho-ci {}", env!("CARGO_PKG_VERSION"));
            exit(0);
        }
        ["step", "start", name] => {
            let response = send(protocol::JobControlRequest::LogStep {
                job_name: job_name(),
                step_name: (*name).to_owned(),
            });
            match response {
                protocol::JobControlResponse::StepLogged => exit(0),
                other => fail_response("logging step", other),
            }
        }
        ["step", "fail", rest @ ..] => {
            let ignore = match rest {
                [] => false,
                ["--ignore"] | ["-i"] => true,
                _ => usage(),
            };
            let response = send(protocol::JobControlRequest::MarkStepFailed {
                job_name: job_name(),
                ignore,
            });
            match response {
                protocol::JobControlResponse::StepMarkedFailed => exit(0),
                other => fail_response("marking step as failed", other),
            }
        }
        ["job", "start", name] => {
            let response = send(protocol::JobControlRequest::StartJob {
                name: (*name).to_owned(),
            });
            match response {
                protocol::JobControlResponse::JobStarted => exit(0),
                other => fail_response("starting job", other),
            }
        }
        ["job", "wait", name, rest @ ..] => {
            let success = match rest {
                [] => false,
                ["--success"] | ["-s"] => true,
                _ => usage(),
            };
            let response = send(protocol::JobControlRequest::WaitForJob {
                name: (*name).to_owned(),
            });
            match response {
                protocol::JobControlResponse::JobCompleted { status } => match status {
                    protocol::JobStatus::Succeeded => exit(0),
                    protocol::JobStatus::Failed if success => {
                        eprintln!("Job '{name}' failed");
                        exit(EXIT_JOB_WAIT_FAILED);
                    }
                    protocol::JobStatus::Failed => exit(0),
                    protocol::JobStatus::Running => {
                        eprintln!("Job '{name}' is still running");
                        exit(1);
                    }
                },
                protocol::JobControlResponse::JobNotFound => {
                    eprintln!("Job '{name}' not found");
                    exit(EXIT_JOB_NOT_FOUND);
                }
                other => fail_response("waiting for job", other),
            }
        }
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: selfci step start <name> | step fail [--ignore] | \
         job start <name> | job wait <name> [--success] | version"
    );
    exit(1);
}

fn job_name() -> String {
    require_env(envs::SELFCI_JOB_NAME)
}

fn require_env(name: &str) -> String {
    match std::env::var(name) {
        Ok(value) => value,
        Err(_) => {
            eprintln!("Error: {name} environment variable not set");
            exit(1);
        }
    }
}

fn send(request: protocol::JobControlRequest) -> protocol::JobControlResponse {
    let socket_path = PathBuf::from(require_env(envs::SELFCI_JOB_SOCK_PATH));
    match protocol::send_request(&socket_path, request) {
        Ok(response) => response,
        Err(err) => {
            eprintln!("Error communicating with control socket: {err}");
            exit(1);
        }
    }
}

fn fail_response(action: &str, response: protocol::JobControlResponse) -> ! {
    match response {
        protocol::JobControlResponse::Error(err) => eprintln!("Error {action}: {err}"),
        other => eprintln!("Unexpected response from control socket {action}: {other:?}"),
    }
    exit(1);
}

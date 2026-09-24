// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The job-control socket protocol between the check engine and in-script
//! `selfci step`/`selfci job` invocations: one CBOR-encoded request per
//! connection, one CBOR response back. Types must stay wire-compatible
//! with the stock `selfci` binary, which is the client end.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StepStatus {
    Running,
    Success,
    Failed { ignored: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepLogEntry {
    pub ts: SystemTime,
    pub name: String,
    pub status: StepStatus,
    /// Job start time (kept for wire compatibility; selfci used it for
    /// live merge-queue status displays).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_started_at: Option<SystemTime>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum JobControlRequest {
    StartJob { name: String },
    LogStep { job_name: String, step_name: String },
    MarkStepFailed { job_name: String, ignore: bool },
    WaitForJob { name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JobStatus {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletedJob {
    pub name: String,
    pub status: JobStatus,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum JobControlResponse {
    JobStarted,
    StepLogged,
    StepMarkedFailed,
    JobCompleted { status: JobStatus },
    JobNotFound,
    Error(String),
}

pub fn send_request(
    socket_path: &Path,
    request: JobControlRequest,
) -> Result<JobControlResponse, String> {
    let mut stream = UnixStream::connect(socket_path)
        .map_err(|e| format!("Failed to connect to control socket: {}", e))?;

    ciborium::into_writer(&request, &mut stream)
        .map_err(|e| format!("Failed to send request: {}", e))?;

    // Shutdown write side to signal end of request
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| format!("Failed to shutdown write: {}", e))?;

    let response: JobControlResponse = ciborium::from_reader(&mut stream)
        .map_err(|e| format!("Failed to read response: {}", e))?;

    Ok(response)
}

pub fn read_request<R: Read>(reader: R) -> Result<JobControlRequest, String> {
    ciborium::from_reader(reader).map_err(|e| format!("Failed to decode request: {}", e))
}

pub fn write_response<W: Write>(mut writer: W, response: JobControlResponse) -> Result<(), String> {
    ciborium::into_writer(&response, &mut writer)
        .map_err(|e| format!("Failed to encode response: {}", e))
}

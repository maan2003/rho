use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct CiStatusResponse {
    pub pr: PrInfo,
    pub runs: Vec<WorkflowRun>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkflowRunResponse {
    pub run: WorkflowRun,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PrInfo {
    pub number: u64,
    pub branch: String,
    pub state: String,
    pub head_sha: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PrCreateRequest {
    pub head: String,
    pub base: String,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PrCreateResponse {
    pub number: u64,
    pub url: String,
    pub head: String,
    pub base: String,
    pub draft: bool,
}

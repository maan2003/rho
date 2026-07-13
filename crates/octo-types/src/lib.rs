use serde::{Deserialize, Serialize};

/// Socket shared by the Octo client, Git remote helper, and Rho daemon.
pub fn socket_path() -> std::io::Result<std::path::PathBuf> {
    dirs::runtime_dir()
        .or_else(dirs::state_dir)
        .map(|base| base.join("rho").join("octo.sock"))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "neither runtime nor state directory is available",
            )
        })
}

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
    pub kind: String,
    pub url: String,
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

#[derive(Debug, Serialize, Deserialize)]
pub struct PrSnapshot {
    pub repository_id: u64,
    pub authenticated_user_id: Option<u64>,
    pub pr_author_id: Option<u64>,
    pub number: u64,
    pub url: String,
    pub state: String,
    pub merged: bool,
    pub draft: bool,
    pub mergeable: Option<bool>,
    pub mergeable_state: String,
    pub review_decision: String,
    pub head_sha: String,
    pub legacy_status: Option<String>,
    pub pending_review_ids: Vec<u64>,
    pub feedback: Vec<PrFeedback>,
    pub runs: Vec<WorkflowRun>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PrFeedback {
    /// `issue`, `inline`, or `review`.
    pub surface: String,
    pub id: u64,
    pub updated_at: String,
    pub author: String,
    pub author_id: Option<u64>,
    pub author_type: String,
    pub author_association: String,
    pub body: String,
    pub url: String,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub diff_hunk: Option<String>,
    pub review_id: Option<u64>,
    pub review_state: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PrCommentRequest {
    pub body: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PrCommentResponse {
    pub id: u64,
    pub url: String,
}

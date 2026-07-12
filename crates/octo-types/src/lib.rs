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

use serde::{Deserialize, Serialize};

/// Fixed socket shared by the Octo client, Git remote helper, and Rho daemon.
pub fn socket_path() -> std::path::PathBuf {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `geteuid` has no preconditions and does not mutate memory.
        let uid = unsafe { libc::geteuid() };
        std::path::PathBuf::from(format!("/run/user/{uid}/rho/octo.sock"))
    }

    #[cfg(not(target_os = "linux"))]
    {
        dirs::state_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("rho")
            .join("octo.sock")
    }
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

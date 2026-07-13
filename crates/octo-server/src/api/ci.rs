use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use octo_types::{CiStatusResponse, PrInfo, WorkflowRun, WorkflowRunResponse};
use serde::Deserialize;

use crate::error::AppError;
use crate::state::AppState;
use crate::types::PathSegment;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/ci/status/{owner}/{repo}", get(get_ci_status))
        .route("/ci/run/{owner}/{repo}/{run_id}", get(get_ci_run))
        .route("/ci/logs/{owner}/{repo}/{run_id}", get(get_ci_logs))
        .route("/ci/rerun/{owner}/{repo}/{run_id}", post(rerun_ci))
}

#[derive(Deserialize)]
struct StatusQuery {
    pr: String,
}

fn parse_pr_number_from_url(url: &str) -> Option<u64> {
    let parts: Vec<&str> = url.trim_end_matches('/').rsplit('/').collect();
    if parts.len() >= 2 && parts[1] == "pull" {
        parts[0].parse().ok()
    } else {
        None
    }
}

async fn resolve_pr(
    state: &AppState,
    owner: &str,
    repo: &str,
    pr: &str,
) -> Result<serde_json::Value, AppError> {
    if let Ok(number) = pr.parse::<u64>() {
        let pr_json = state
            .github_get_json(&["repos", owner, repo, "pulls", &number.to_string()], None)
            .await?;
        return Ok(pr_json);
    }

    if let Some(number) = parse_pr_number_from_url(pr) {
        let pr_json = state
            .github_get_json(&["repos", owner, repo, "pulls", &number.to_string()], None)
            .await?;
        return Ok(pr_json);
    }

    // Treat as branch name
    let query = format!("head={}:{}&state=open", owner, pr);
    let prs: Vec<serde_json::Value> = state
        .github_get_json(&["repos", owner, repo, "pulls"], Some(&query))
        .await?
        .as_array()
        .cloned()
        .unwrap_or_default();

    prs.into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no open PR found for branch {}", pr).into())
}

fn extract_pr_info(pr: &serde_json::Value) -> Result<PrInfo, AppError> {
    Ok(PrInfo {
        number: pr["number"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("PR response missing number"))?,
        branch: pr["head"]["ref"].as_str().unwrap_or("").to_string(),
        state: pr["state"].as_str().unwrap_or("").to_string(),
        head_sha: pr["head"]["sha"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("PR response missing head.sha"))?
            .to_string(),
    })
}

fn extract_runs(runs_json: &serde_json::Value) -> Vec<WorkflowRun> {
    runs_json["workflow_runs"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|r| WorkflowRun {
                    id: r["id"].as_u64().unwrap_or(0),
                    name: r["name"].as_str().unwrap_or("").to_string(),
                    kind: "workflow".to_owned(),
                    url: r["html_url"].as_str().unwrap_or("").to_owned(),
                    status: r["status"].as_str().unwrap_or("").to_string(),
                    conclusion: r["conclusion"].as_str().map(|s| s.to_string()),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn extract_run(run_json: &serde_json::Value) -> WorkflowRun {
    WorkflowRun {
        id: run_json["id"].as_u64().unwrap_or(0),
        name: run_json["name"].as_str().unwrap_or("").to_string(),
        kind: "workflow".to_owned(),
        url: run_json["html_url"].as_str().unwrap_or("").to_owned(),
        status: run_json["status"].as_str().unwrap_or("").to_string(),
        conclusion: run_json["conclusion"].as_str().map(|s| s.to_string()),
    }
}

async fn get_ci_status(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(PathSegment, PathSegment)>,
    Query(query): Query<StatusQuery>,
) -> Result<Json<CiStatusResponse>, AppError> {
    let pr_json = resolve_pr(&state, owner.as_str(), repo.as_str(), &query.pr).await?;
    let pr = extract_pr_info(&pr_json)?;

    let runs_query = format!("head_sha={}&per_page=100", pr.head_sha);
    let runs_json = state
        .github_get_json(
            &["repos", owner.as_str(), repo.as_str(), "actions", "runs"],
            Some(&runs_query),
        )
        .await?;

    Ok(Json(CiStatusResponse {
        pr,
        runs: extract_runs(&runs_json),
    }))
}

async fn get_ci_run(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, run_id)): Path<(PathSegment, PathSegment, u64)>,
) -> Result<Json<WorkflowRunResponse>, AppError> {
    let run_json = state
        .github_get_json(
            &[
                "repos",
                owner.as_str(),
                repo.as_str(),
                "actions",
                "runs",
                &run_id.to_string(),
            ],
            None,
        )
        .await?;

    Ok(Json(WorkflowRunResponse {
        run: extract_run(&run_json),
    }))
}

async fn get_ci_logs(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, run_id)): Path<(PathSegment, PathSegment, u64)>,
) -> Result<Response, AppError> {
    state
        .proxy_github_get(
            &[
                "repos",
                owner.as_str(),
                repo.as_str(),
                "actions",
                "runs",
                &run_id.to_string(),
                "logs",
            ],
            None,
        )
        .await
}

async fn rerun_ci(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, run_id)): Path<(PathSegment, PathSegment, u64)>,
) -> Result<Response, AppError> {
    state
        .proxy_github_post(&[
            "repos",
            owner.as_str(),
            repo.as_str(),
            "actions",
            "runs",
            &run_id.to_string(),
            "rerun-failed-jobs",
        ])
        .await
}

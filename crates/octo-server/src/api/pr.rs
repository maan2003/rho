use std::sync::Arc;

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use octo_types::{
    PrCommentRequest, PrCommentResponse, PrCreateRequest, PrCreateResponse, PrFeedback, PrSnapshot,
    WorkflowRun,
};

use crate::error::AppError;
use crate::state::AppState;
use crate::types::PathSegment;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/pr/create/{owner}/{repo}", post(create_pr))
        .route("/pr/snapshot/{owner}/{repo}/{number}", get(pr_snapshot))
        .route("/pr/comment/{owner}/{repo}/{number}", post(comment_on_pr))
        .route(
            "/pr/reply/{owner}/{repo}/{number}/{comment_id}",
            post(reply_to_review_comment),
        )
}

async fn create_pr(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(PathSegment, PathSegment)>,
    Json(req): Json<PrCreateRequest>,
) -> Result<Json<PrCreateResponse>, AppError> {
    let pr_json = state
        .github_post_json(
            &["repos", owner.as_str(), repo.as_str(), "pulls"],
            &serde_json::json!({
                "title": req.title,
                "body": req.body,
                "head": req.head,
                "base": req.base,
                "draft": true,
            }),
        )
        .await?;

    Ok(Json(PrCreateResponse {
        number: pr_json["number"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("PR response missing number"))?,
        url: pr_json["html_url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("PR response missing html_url"))?
            .to_string(),
        head: pr_json["head"]["ref"].as_str().unwrap_or("").to_string(),
        base: pr_json["base"]["ref"].as_str().unwrap_or("").to_string(),
        draft: pr_json["draft"].as_bool().unwrap_or(true),
    }))
}

fn feedback(surface: &str, values: &serde_json::Value) -> Vec<PrFeedback> {
    values
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| {
            let body = value["body"].as_str()?.trim();
            if body.is_empty() {
                return None;
            }
            Some(PrFeedback {
                surface: surface.to_owned(),
                id: value["id"].as_u64()?,
                updated_at: value["updated_at"]
                    .as_str()
                    .or_else(|| value["submitted_at"].as_str())
                    .unwrap_or("")
                    .to_owned(),
                author: value["user"]["login"].as_str().unwrap_or("").to_owned(),
                author_id: value["user"]["id"].as_u64(),
                author_type: value["user"]["type"].as_str().unwrap_or("").to_owned(),
                author_association: value["author_association"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned(),
                body: body.to_owned(),
                url: value["html_url"].as_str().unwrap_or("").to_owned(),
                path: value["path"].as_str().map(str::to_owned),
                line: value["line"]
                    .as_u64()
                    .or_else(|| value["original_line"].as_u64()),
                diff_hunk: value["diff_hunk"].as_str().map(str::to_owned),
                review_id: value["pull_request_review_id"].as_u64(),
                review_state: value["state"].as_str().map(str::to_owned),
            })
        })
        .collect()
}

async fn pr_snapshot(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(PathSegment, PathSegment, u64)>,
) -> Result<Json<PrSnapshot>, AppError> {
    let owner = owner.as_str();
    let repo = repo.as_str();
    let number_text = number.to_string();
    let pr = state
        .github_get_json(&["repos", owner, repo, "pulls", &number_text], None)
        .await?;
    let issue_comments = state
        .github_get_json_pages(
            &["repos", owner, repo, "issues", &number_text, "comments"],
            2,
        )
        .await?;
    let inline_comments = state
        .github_get_json_pages(
            &["repos", owner, repo, "pulls", &number_text, "comments"],
            2,
        )
        .await?;
    let reviews = state
        .github_get_json_pages(&["repos", owner, repo, "pulls", &number_text, "reviews"], 2)
        .await?;
    let head_sha = pr["head"]["sha"].as_str().unwrap_or("").to_owned();
    let runs = state
        .github_get_json(
            &["repos", owner, repo, "actions", "runs"],
            Some(&format!("head_sha={head_sha}&per_page=100")),
        )
        .await?;
    if runs["total_count"].as_u64().unwrap_or(0) > 100 {
        return Err(anyhow::anyhow!("workflow runs exceed the 100-item snapshot bound").into());
    }
    let mut runs = runs["workflow_runs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|run| WorkflowRun {
            id: run["id"].as_u64().unwrap_or(0),
            name: run["name"].as_str().unwrap_or("").to_owned(),
            kind: "workflow".to_owned(),
            url: run["html_url"].as_str().unwrap_or("").to_owned(),
            status: run["status"].as_str().unwrap_or("").to_owned(),
            conclusion: run["conclusion"].as_str().map(str::to_owned),
        })
        .collect::<Vec<_>>();
    let checks = state
        .github_get_json(
            &["repos", owner, repo, "commits", &head_sha, "check-runs"],
            Some("per_page=100"),
        )
        .await?;
    if checks["total_count"].as_u64().unwrap_or(0) > 100 {
        return Err(anyhow::anyhow!("check runs exceed the 100-item snapshot bound").into());
    }
    let statuses = state
        .github_get_json(
            &["repos", owner, repo, "commits", &head_sha, "status"],
            None,
        )
        .await?;
    let legacy_status = statuses["statuses"]
        .as_array()
        .is_some_and(|statuses| !statuses.is_empty())
        .then(|| statuses["state"].as_str().unwrap_or("pending").to_owned());
    runs.extend(
        checks["check_runs"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|run| WorkflowRun {
                id: run["id"].as_u64().unwrap_or(0),
                name: run["name"].as_str().unwrap_or("").to_owned(),
                kind: "check".to_owned(),
                url: run["html_url"].as_str().unwrap_or("").to_owned(),
                status: run["status"].as_str().unwrap_or("").to_owned(),
                conclusion: run["conclusion"].as_str().map(str::to_owned),
            }),
    );
    runs.sort_by(|a, b| (&a.kind, a.id, &a.name).cmp(&(&b.kind, b.id, &b.name)));
    runs.dedup_by(|a, b| a.kind == b.kind && a.id == b.id && a.name == b.name);
    let mut all_feedback = feedback("issue", &issue_comments);
    let pending_reviews = reviews
        .as_array()
        .into_iter()
        .flatten()
        .filter(|review| review["state"].as_str() == Some("PENDING"))
        .filter_map(|review| review["id"].as_u64())
        .collect::<std::collections::HashSet<_>>();
    let review_feedback = feedback("review", &reviews);
    let graphql = state
        .execute_graphql::<serde_json::Value>(
            "query($owner: String!, $repo: String!, $number: Int!) { viewer { databaseId } repository(owner: $owner, name: $repo) { pullRequest(number: $number) { reviewDecision } } }",
            serde_json::json!({ "owner": owner, "repo": repo, "number": number }),
        )
        .await
        .ok();
    let review_decision = match graphql.as_ref() {
        Some(value) if value["errors"].is_null() => {
            value["data"]["repository"]["pullRequest"]["reviewDecision"]
                .as_str()
                .unwrap_or("NONE")
        }
        _ => "UNKNOWN",
    };
    all_feedback.extend(feedback("inline", &inline_comments));
    all_feedback.extend(review_feedback);

    Ok(Json(PrSnapshot {
        repository_id: pr["base"]["repo"]["id"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("PR response missing base.repo.id"))?,
        authenticated_user_id: graphql
            .as_ref()
            .and_then(|value| value["data"]["viewer"]["databaseId"].as_u64()),
        pr_author_id: pr["user"]["id"].as_u64(),
        number,
        url: pr["html_url"].as_str().unwrap_or("").to_owned(),
        state: pr["state"].as_str().unwrap_or("").to_owned(),
        merged: pr["merged"].as_bool().unwrap_or(false),
        draft: pr["draft"].as_bool().unwrap_or(false),
        mergeable: pr["mergeable"].as_bool(),
        mergeable_state: pr["mergeable_state"].as_str().unwrap_or("").to_owned(),
        review_decision: review_decision.to_owned(),
        head_sha,
        legacy_status,
        pending_review_ids: pending_reviews.into_iter().collect(),
        feedback: all_feedback,
        runs,
    }))
}

async fn comment_on_pr(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(PathSegment, PathSegment, u64)>,
    Json(request): Json<PrCommentRequest>,
) -> Result<Json<PrCommentResponse>, AppError> {
    let response = state
        .github_post_json(
            &[
                "repos",
                owner.as_str(),
                repo.as_str(),
                "issues",
                &number.to_string(),
                "comments",
            ],
            &request,
        )
        .await?;
    Ok(Json(comment_response(&response)?))
}

async fn reply_to_review_comment(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number, comment_id)): Path<(PathSegment, PathSegment, u64, u64)>,
    Json(request): Json<PrCommentRequest>,
) -> Result<Json<PrCommentResponse>, AppError> {
    let response = state
        .github_post_json(
            &[
                "repos",
                owner.as_str(),
                repo.as_str(),
                "pulls",
                &number.to_string(),
                "comments",
                &comment_id.to_string(),
                "replies",
            ],
            &request,
        )
        .await?;
    Ok(Json(comment_response(&response)?))
}

fn comment_response(value: &serde_json::Value) -> Result<PrCommentResponse, AppError> {
    Ok(PrCommentResponse {
        id: value["id"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("comment response missing id"))?,
        url: value["html_url"].as_str().unwrap_or("").to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_preserves_revision_and_inline_context() {
        let values = serde_json::json!([{
            "id": 7,
            "updated_at": "2026-01-02T03:04:05Z",
            "user": { "id": 9, "login": "reviewer", "type": "User" },
            "author_association": "MEMBER",
            "body": "please fix this",
            "html_url": "https://github.com/acme/repo/pull/1#discussion_r7",
            "path": "src/lib.rs",
            "line": 42,
            "diff_hunk": "@@ context",
            "pull_request_review_id": 11
        }]);
        let records = feedback("inline", &values);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].updated_at, "2026-01-02T03:04:05Z");
        assert_eq!(records[0].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(records[0].review_id, Some(11));
    }
}

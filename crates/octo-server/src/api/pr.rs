use std::sync::Arc;

use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use octo_types::{PrCreateRequest, PrCreateResponse};

use crate::error::AppError;
use crate::state::AppState;
use crate::types::PathSegment;

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/pr/create/{owner}/{repo}", post(create_pr))
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

use anyhow::Result;
use clap::Parser;
use octo_types::{PrCreateRequest, PrCreateResponse};

use crate::octo_client::OctoClient;
use crate::repo::{resolve_default_base_branch, resolve_repo};

#[derive(Parser)]
#[command(about = "Create a draft pull request")]
pub struct CreateArgs {
    /// Source branch for the pull request
    #[arg(short = 'H', long)]
    pub head: String,

    /// Base branch for the pull request
    #[arg(short = 'B', long)]
    pub base: Option<String>,

    /// Title for the pull request
    #[arg(short = 't', long)]
    pub title: String,

    /// Body for the pull request
    #[arg(short = 'b', long)]
    pub body: String,
}

pub async fn create(args: CreateArgs) -> Result<()> {
    let (owner, repo) = resolve_repo()?;
    let client = OctoClient::new()?;
    let base = match args.base {
        Some(base) => base,
        None => resolve_default_base_branch()?,
    };

    let resp: PrCreateResponse = client
        .post_json(
            &format!("/pr/create/{owner}/{repo}"),
            &PrCreateRequest {
                head: args.head,
                base,
                title: args.title,
                body: args.body,
            },
        )
        .await?;

    println!("{}", resp.url);

    Ok(())
}

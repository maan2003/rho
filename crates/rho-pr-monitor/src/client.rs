use anyhow::{Context as _, Result};
use bytes::Bytes;
use futures_util::StreamExt as _;
use octo_types::{
    PrCommentRequest, PrCommentResponse, PrCreateRequest, PrCreateResponse, PrSnapshot,
};

#[derive(Clone)]
pub(crate) struct OctoClient {
    client: reqwest::Client,
    base_url: reqwest::Url,
}

impl OctoClient {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .unix_socket(octo_types::socket_path()?)
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            base_url: reqwest::Url::parse("http://localhost")?,
        })
    }

    pub(crate) async fn snapshot(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PrSnapshot> {
        self.get(&format!("/pr/snapshot/{owner}/{repo}/{number}"))
            .await
    }

    pub(crate) async fn create(
        &self,
        owner: &str,
        repo: &str,
        request: &PrCreateRequest,
    ) -> Result<PrCreateResponse> {
        self.post(&format!("/pr/create/{owner}/{repo}"), request)
            .await
    }

    pub(crate) async fn rerun(&self, owner: &str, repo: &str, run_id: u64) -> Result<()> {
        self.post_empty(&format!("/ci/rerun/{owner}/{repo}/{run_id}"))
            .await
    }

    pub(crate) async fn logs(&self, owner: &str, repo: &str, run_id: u64) -> Result<Bytes> {
        let response = self
            .client
            .get(
                self.base_url
                    .join(&format!("/ci/logs/{owner}/{repo}/{run_id}"))?,
            )
            .send()
            .await?;
        let status = response.status();
        let mut bytes = bytes::BytesMut::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            anyhow::ensure!(
                bytes.len().saturating_add(chunk.len()) <= 48 * 1024 * 1024,
                "CI logs exceed the 48 MiB daemon protocol limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        let bytes = bytes.freeze();
        anyhow::ensure!(
            status.is_success(),
            "Octo returned {status}: {}",
            String::from_utf8_lossy(&bytes)
        );
        Ok(bytes)
    }

    pub(crate) async fn comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: String,
    ) -> Result<PrCommentResponse> {
        self.post(
            &format!("/pr/comment/{owner}/{repo}/{number}"),
            &PrCommentRequest { body },
        )
        .await
    }

    pub(crate) async fn reply(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        comment_id: u64,
        body: String,
    ) -> Result<PrCommentResponse> {
        self.post(
            &format!("/pr/reply/{owner}/{repo}/{number}/{comment_id}"),
            &PrCommentRequest { body },
        )
        .await
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self.client.get(self.base_url.join(path)?).send().await?;
        decode(response).await
    }

    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &impl serde::Serialize,
    ) -> Result<T> {
        let response = self
            .client
            .post(self.base_url.join(path)?)
            .json(body)
            .send()
            .await?;
        decode(response).await
    }

    async fn post_empty(&self, path: &str) -> Result<()> {
        let response = self.client.post(self.base_url.join(path)?).send().await?;
        let status = response.status();
        let text = response.text().await?;
        anyhow::ensure!(status.is_success(), "Octo returned {status}: {text}");
        Ok(())
    }
}

async fn decode<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    let status = response.status();
    let bytes = response.bytes().await?;
    anyhow::ensure!(
        status.is_success(),
        "Octo returned {status}: {}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).context("decoding Octo response")
}

use std::env;

use anyhow::{Context, Result};
use bytes::Bytes;
use reqwest::Url;
use serde::Serialize;
use serde::de::DeserializeOwned;

pub struct OctoClient {
    client: reqwest::Client,
    base_url: Url,
}

impl OctoClient {
    pub fn from_env() -> Result<Self> {
        let socket_path =
            env::var("OCTO_SOCKET").context("OCTO_SOCKET environment variable not set")?;

        let client = reqwest::Client::builder()
            .unix_socket(socket_path)
            .build()?;

        let base_url = Url::parse("http://localhost")?;

        Ok(Self { client, base_url })
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = self.base_url.join(path)?;
        self.get_url(url).await
    }

    pub async fn get_with_query<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T> {
        let mut url = self.base_url.join(path)?;
        url.query_pairs_mut().extend_pairs(query.iter().copied());
        self.get_url(url).await
    }

    async fn get_url<T: DeserializeOwned>(&self, url: Url) -> Result<T> {
        let resp = self.client.get(url).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Server returned {}: {}", status, text);
        }

        let result: T = resp.json().await?;
        Ok(result)
    }

    pub async fn get_bytes(&self, path: &str) -> Result<Bytes> {
        let url = self.base_url.join(path)?;

        let resp = self.client.get(url).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Server returned {}: {}", status, text);
        }

        let bytes = resp.bytes().await?;
        Ok(bytes)
    }

    pub async fn post(&self, path: &str) -> Result<String> {
        let url = self.base_url.join(path)?;

        let resp = self.client.post(url).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Server returned {}: {}", status, text);
        }

        let text = resp.text().await?;
        Ok(text)
    }

    pub async fn post_json<T: DeserializeOwned, B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let url = self.base_url.join(path)?;

        let resp = self.client.post(url).json(body).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Server returned {}: {}", status, text);
        }

        let result = resp.json().await?;
        Ok(result)
    }
}

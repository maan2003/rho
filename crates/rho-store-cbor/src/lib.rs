//! Append-only CBOR persistence for rho transcript blocks.

use std::path::{Path, PathBuf};

use anyhow::Result;
use rho_core::ItemBlock;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Debug)]
pub struct CborLog {
    path: PathBuf,
}

impl CborLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn append_block(&self, block: &ItemBlock) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let bytes = serde_cbor::to_vec(block)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;

        file.write_u32(bytes.len() as u32).await?;
        file.write_all(&bytes).await?;
        file.flush().await?;
        Ok(())
    }

    pub async fn read_blocks(&self) -> Result<Vec<ItemBlock>> {
        let mut file = match fs::File::open(&self.path).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };

        let mut blocks = Vec::new();
        loop {
            let len = match file.read_u32().await {
                Ok(len) => len as usize,
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(error) => return Err(error.into()),
            };

            let mut bytes = vec![0; len];
            file.read_exact(&mut bytes).await?;
            blocks.push(serde_cbor::from_slice(&bytes)?);
        }

        Ok(blocks)
    }
}

#[cfg(test)]
mod tests;

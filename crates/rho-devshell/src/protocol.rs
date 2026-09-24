//! The daemon's dev shell cache socket, `daemon.sock` in the shared cache
//! directory. Views bind that directory at its host path, so the same path
//! reaches the daemon from a workset process and from a `nix develop` in a
//! view. Each request is answered before the next is read; frames are a
//! little-endian `u32` length and a senax-encoded message.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail, ensure};
use senax_encoder::{Decode, Encode};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;

use crate::Candidate;

const MAX_FRAME: usize = 64 * 1024 * 1024;

/// The socket below the shared cache directory.
pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join("daemon.sock")
}

#[derive(Debug, Encode, Decode)]
pub enum Request {
    Lookup(String),
    Used(u64),
    Store {
        key: String,
        env_store_path: String,
        data: Vec<u8>,
    },
    Forget(u64),
}

#[derive(Debug, Encode, Decode)]
pub enum Reply {
    Candidates(Vec<Candidate>),
    Rooted(bool),
    Stored(u64),
    Done,
    Error(String),
}

/// Read one message; `None` at a clean end of stream.
pub async fn read<T: senax_encoder::Decoder>(stream: &mut (impl tokio::io::AsyncRead + Unpin)) -> Result<Option<T>> {
    let mut length = [0; 4];
    match stream.read_exact(&mut length).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let length = u32::from_le_bytes(length) as usize;
    ensure!(length <= MAX_FRAME, "dev shell cache message too large");
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    let mut remaining = bytes.as_slice();
    let value = senax_encoder::decode(&mut remaining).map_err(|_| anyhow::anyhow!("invalid dev shell cache message"))?;
    ensure!(remaining.is_empty(), "trailing dev shell cache message data");
    Ok(Some(value))
}

pub async fn write<T: senax_encoder::Encoder>(stream: &mut (impl tokio::io::AsyncWrite + Unpin), value: &T) -> Result<()> {
    let bytes = senax_encoder::encode(value).map_err(|_| anyhow::anyhow!("encode dev shell cache message"))?;
    ensure!(bytes.len() <= MAX_FRAME, "dev shell cache message too large");
    let mut frame = Vec::with_capacity(4 + bytes.len());
    frame.extend((bytes.len() as u32).to_le_bytes());
    frame.extend(&bytes[..]);
    stream.write_all(&frame).await?;
    Ok(())
}

/// The daemon's shell cache, over its socket, reconnecting as needed.
pub struct Client {
    socket: PathBuf,
    /// Taken for the length of a request, so that one abandoned midway
    /// closes its connection rather than leaving a reply behind.
    stream: tokio::sync::Mutex<Option<UnixStream>>,
}

impl Client {
    pub fn new(dir: &Path) -> Self {
        Self {
            socket: socket_path(dir),
            stream: tokio::sync::Mutex::new(None),
        }
    }

    async fn request(&self, request: Request) -> Result<Reply> {
        let mut slot = self.stream.lock().await;
        let reused = slot.is_some();
        let mut stream = match slot.take() {
            Some(stream) => stream,
            None => self.connect().await?,
        };
        let reply = match exchange(&mut stream, &request).await {
            // The daemon may have restarted since the last request.
            Err(_) if reused => {
                stream = self.connect().await?;
                exchange(&mut stream, &request).await?
            }
            result => result?,
        };
        *slot = Some(stream);
        match reply {
            Reply::Error(error) => bail!("{error}"),
            reply => Ok(reply),
        }
    }

    async fn connect(&self) -> Result<UnixStream> {
        UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connect to {}", self.socket.display()))
    }
}

async fn exchange(stream: &mut UnixStream, request: &Request) -> Result<Reply> {
    write(stream, request).await?;
    read(stream).await?.context("dev shell cache closed the connection")
}

fn unexpected() -> anyhow::Error {
    anyhow::anyhow!("unexpected dev shell cache reply")
}

/// The daemon's shell cache. Entries of a key come newest first; `used`
/// marks one as in use and says whether its environment is still pinned,
/// and the caller pins it otherwise.
impl Client {
    pub async fn lookup(&self, key: String) -> Result<Vec<Candidate>> {
        match self.request(Request::Lookup(key)).await? {
            Reply::Candidates(candidates) => Ok(candidates),
            _ => Err(unexpected()),
        }
    }

    pub async fn used(&self, id: u64) -> Result<bool> {
        match self.request(Request::Used(id)).await? {
            Reply::Rooted(rooted) => Ok(rooted),
            _ => Err(unexpected()),
        }
    }

    pub async fn store(&self, key: String, env_store_path: String, data: Vec<u8>) -> Result<u64> {
        let request = Request::Store {
            key,
            env_store_path,
            data,
        };
        match self.request(request).await? {
            Reply::Stored(id) => Ok(id),
            _ => Err(unexpected()),
        }
    }

    /// The entry's environment is gone.
    pub async fn forget(&self, id: u64) -> Result<()> {
        match self.request(Request::Forget(id)).await? {
            Reply::Done => Ok(()),
            _ => Err(unexpected()),
        }
    }
}

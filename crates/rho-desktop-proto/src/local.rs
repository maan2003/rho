//! Direct same-user local desktop access, including across mount namespaces.
use std::os::linux::net::SocketAddrExt;

use anyhow::{Result, ensure};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::{MAX_HEADER, Request, Response, VERSION};

pub async fn connect(address: &str) -> Result<tokio::net::UnixStream> {
    let address = address.to_owned();
    let stream = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
        let stream = if let Some(name) = address.strip_prefix('@') {
            let address = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
            std::os::unix::net::UnixStream::connect_addr(&address)?
        } else {
            std::os::unix::net::UnixStream::connect(address)?
        };
        stream.set_nonblocking(true)?;
        Ok(stream)
    })
    .await??;
    Ok(tokio::net::UnixStream::from_std(stream)?)
}
pub struct Desktop {
    pub control: BufReader<tokio::net::UnixStream>,
    pub media: tokio::net::UnixStream,
}
impl Desktop {
    pub async fn open(address: &str) -> Result<Self> {
        let mut control = BufReader::new(connect(address).await?);
        let response = request(&mut control, Request::Hello { version: VERSION }).await?;
        let Response::Hello {
            version: VERSION,
            media_socket,
            ..
        } = response
        else {
            anyhow::bail!("desktop version mismatch")
        };
        let media = connect(&media_socket).await?;
        Ok(Self { control, media })
    }
}
pub async fn request(
    stream: &mut BufReader<tokio::net::UnixStream>,
    request: Request,
) -> Result<Response> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut data = serde_json::to_vec(&request)?;
        data.push(b'\n');
        stream.get_mut().write_all(&data).await?;
        let mut data = Vec::new();
        (&mut *stream)
            .take(MAX_HEADER)
            .read_until(b'\n', &mut data)
            .await?;
        ensure!(data.last() == Some(&b'\n'), "invalid desktop response");
        let response = serde_json::from_slice(&data)?;
        if let Response::Error { message } = response {
            anyhow::bail!("{message}")
        }
        Ok::<_, anyhow::Error>(response)
    })
    .await?
}

/// Captures one packed top-to-bottom BGRA8 frame from `output` into `pixels`,
/// returning `(width, height)`.
pub async fn capture(
    stream: &mut BufReader<tokio::net::UnixStream>,
    output: &str,
    pixels: &mut Vec<u8>,
) -> Result<(u32, u32)> {
    let Response::Frame { width, height } = request(
        stream,
        Request::Capture {
            output: output.to_owned(),
        },
    )
    .await?
    else {
        anyhow::bail!("unexpected desktop capture response");
    };
    let len = crate::frame_len(width, height)
        .ok_or_else(|| anyhow::anyhow!("invalid desktop frame dimensions: {width}x{height}"))?;
    pixels.resize(len, 0);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        stream.read_exact(pixels),
    )
    .await??;
    Ok((width, height))
}

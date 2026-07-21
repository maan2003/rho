//! Neutral, bounded sideband protocol between rho-daemon and a shell kernel.
//!
//! The protocol is intentionally independent of the kernel implementation. The
//! daemon only knows these bounded execution, output, and lifecycle messages.

use std::io;

use senax_encoder::{Decode, Encode, Pack, Packer, Unpack, Unpacker};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

pub const PROTOCOL_VERSION: u16 = 4;
pub const MAX_FRAME_LEN: usize = 2 * 1024 * 1024;
pub const MAX_PAGER_FRAME_LEN: usize = 4 * 1024;
pub const MAX_ACTIVE_PAGERS: usize = 64;
pub const MAX_PAGER_LINES: u32 = 1_000;
pub const MAX_PAGER_BYTES: u64 = 64 * 1024;
pub const MAX_COMMAND_BYTES: usize = 1024 * 1024;
pub const MAX_PROMPT_BYTES: usize = 16 * 1024;

pub fn command_fits(command: &str) -> bool {
    command.len() <= MAX_COMMAND_BYTES
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Request {
    Execute {
        execution: u64,
        command: String,
    },
    Interrupt {
        execution: u64,
    },
    Eof {
        execution: u64,
    },
    PagerAction {
        execution: u64,
        pager: u64,
        page: u64,
        action: PagerAction,
    },
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum PagerAction {
    Continue,
    Drain,
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Response {
    Ready {
        protocol: u16,
        prompt: String,
        cwd: String,
    },
    Started {
        execution: u64,
    },
    Output {
        execution: u64,
        data: Vec<u8>,
    },
    PagerStarted {
        execution: u64,
        pager: u64,
    },
    PagerPaused {
        execution: u64,
        pager: u64,
        page: u64,
        lines: u32,
        bytes: u64,
    },
    PagerResumed {
        execution: u64,
        pager: u64,
    },
    PagerFinished {
        execution: u64,
        pager: u64,
    },
    Finished {
        execution: u64,
        status: i32,
        prompt: String,
        cwd: String,
    },
    Error {
        execution: Option<u64>,
        message: String,
    },
    Exited {
        status: i32,
    },
}

/// Private pager-to-sidecar messages carried by the socket advertised in the
/// evaluated command's environment.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum PagerMessage {
    Hello {
        protocol: u16,
        token: String,
        execution_token: String,
    },
    Paused {
        page: u64,
        lines: u32,
        bytes: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum PagerReply {
    Continue,
    Drain,
    Quit,
}

impl From<PagerAction> for PagerReply {
    fn from(action: PagerAction) -> Self {
        match action {
            PagerAction::Continue => Self::Continue,
            PagerAction::Drain => Self::Drain,
            PagerAction::Quit => Self::Quit,
        }
    }
}

pub fn write_frame<T: Packer>(writer: &mut impl io::Write, value: &T) -> io::Result<()> {
    let payload = senax_encoder::pack(value).map_err(invalid_data)?;
    ensure_frame_len(payload.len())?;
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "shell frame too large"))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

pub fn read_frame<T: Unpacker>(reader: &mut impl io::Read) -> io::Result<T> {
    read_frame_with_limit(reader, MAX_FRAME_LEN)
}

pub fn write_pager_frame<T: Packer>(writer: &mut impl io::Write, value: &T) -> io::Result<()> {
    write_frame_with_limit(writer, value, MAX_PAGER_FRAME_LEN)
}

pub fn read_pager_frame<T: Unpacker>(reader: &mut impl io::Read) -> io::Result<T> {
    read_frame_with_limit(reader, MAX_PAGER_FRAME_LEN)
}

fn read_frame_with_limit<T: Unpacker>(reader: &mut impl io::Read, limit: usize) -> io::Result<T> {
    let mut len = [0; size_of::<u32>()];
    reader.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("shell frame length {len} exceeds {limit}"),
        ));
    }
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload)?;
    senax_encoder::unpack(&mut payload.as_slice()).map_err(invalid_data)
}

fn write_frame_with_limit<T: Packer>(
    writer: &mut impl io::Write,
    value: &T,
    limit: usize,
) -> io::Result<()> {
    let payload = senax_encoder::pack(value).map_err(invalid_data)?;
    if payload.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("shell frame length {} exceeds {limit}", payload.len()),
        ));
    }
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "shell frame too large"))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

pub async fn write_frame_async<T: Packer>(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &T,
) -> io::Result<()> {
    let payload = senax_encoder::pack(value).map_err(invalid_data)?;
    ensure_frame_len(payload.len())?;
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "shell frame too large"))?;
    writer.write_u32_le(len).await?;
    writer.write_all(&payload).await?;
    writer.flush().await
}

pub async fn read_frame_async<T: Unpacker>(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<T> {
    let len = reader.read_u32_le().await? as usize;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("shell frame length {len} exceeds {MAX_FRAME_LEN}"),
        ));
    }
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload).await?;
    senax_encoder::unpack(&mut payload.as_slice()).map_err(invalid_data)
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn ensure_frame_len(len: usize) -> io::Result<()> {
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("shell frame length {len} exceeds {MAX_FRAME_LEN}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let expected = Response::Output {
            execution: 42,
            data: vec![0, 1, 0xff],
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &expected).unwrap();
        assert_eq!(
            read_frame::<Response>(&mut bytes.as_slice()).unwrap(),
            expected
        );
    }

    #[test]
    fn writers_enforce_frame_limit() {
        let exact = (MAX_FRAME_LEN - 64..=MAX_FRAME_LEN)
            .find_map(|len| {
                let frame = Response::Output {
                    execution: 1,
                    data: vec![0; len],
                };
                (senax_encoder::pack(&frame).unwrap().len() == MAX_FRAME_LEN).then_some(frame)
            })
            .expect("senax vector overhead fits search window");
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &exact).unwrap();

        let Response::Output { mut data, .. } = exact else {
            unreachable!()
        };
        data.push(0);
        let over = Response::Output { execution: 1, data };
        assert_eq!(
            write_frame(&mut Vec::new(), &over).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn pager_frames_have_a_smaller_limit() {
        let message = PagerMessage::Hello {
            protocol: 1,
            token: "x".repeat(MAX_PAGER_FRAME_LEN),
            execution_token: String::new(),
        };
        assert_eq!(
            write_pager_frame(&mut Vec::new(), &message)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}

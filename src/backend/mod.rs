//! Compile-time backend selection. There is no runtime fallback to AF_UNIX.

#[cfg(any(windows, test))]
mod pipe_packet;

#[cfg(target_os = "macos")]
mod udp;
#[cfg(target_os = "macos")]
pub(crate) use udp::Backend;

#[cfg(target_os = "linux")]
mod unix;
#[cfg(target_os = "linux")]
pub(crate) use unix::Backend;

#[cfg(windows)]
mod pipe;
#[cfg(windows)]
pub(crate) use pipe::Backend;

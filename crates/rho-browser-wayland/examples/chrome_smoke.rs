use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use rho_browser_wayland::{BrowserEvent, BrowserProgram, BrowserSession, chrome_wrapper};

fn main() -> Result<()> {
    let chrome = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(chrome_wrapper()));
    let program = if std::env::var_os("RHO_SMOKE_FIREFOX").is_some() {
        BrowserProgram::Firefox
    } else {
        BrowserProgram::Chromium
    };
    let session = BrowserSession::launch(program, chrome, "https://example.com", (800, 600), None)?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        session.presented();
        while let Some(event) = session.try_recv() {
            match event {
                BrowserEvent::Frame(frame) => {
                    let nonblack = frame
                        .rgba
                        .chunks_exact(4)
                        .any(|pixel| pixel[..3] != [0, 0, 0]);
                    println!(
                        "frame {}x{} ({} bytes, {} nonblack, {} opaque)",
                        frame.width,
                        frame.height,
                        frame.rgba.len(),
                        frame
                            .rgba
                            .chunks_exact(4)
                            .filter(|pixel| pixel[..3] != [0, 0, 0])
                            .count(),
                        frame
                            .rgba
                            .chunks_exact(4)
                            .filter(|pixel| pixel[3] == 255)
                            .count(),
                    );
                    if !nonblack {
                        continue;
                    }
                    return Ok(());
                }
                BrowserEvent::DmaBuf(_) => unreachable!("SHM smoke does not enable DMA-BUF"),
                BrowserEvent::Failed(error) => bail!("{error}"),
                BrowserEvent::ChromeExited(code) => bail!("Chrome exited before a frame: {code:?}"),
                BrowserEvent::Cleared => println!("surface cleared"),
                BrowserEvent::ToplevelReady => println!("toplevel ready"),
            }
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    Err(anyhow::anyhow!("timed out waiting for Chrome frame")).context("stock Chrome smoke test")
}

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;

use anyhow::{Context as _, Result, bail};
use rho_browser_wayland::{BrowserCompositor, BrowserSession, DmaBufConfig, chrome_wrapper};

use crate::store::validate_launch_url;

/// One stock Chromium identity and one private Wayland compositor. Window
/// association is exclusively xdg-activation-v1: the compositor issues the
/// token, Chromium applies it to the new surface, and websites never see it.
pub(crate) struct BrowserRuntime {
    compositor: BrowserCompositor,
    profile: PathBuf,
    _profile_lock: File,
    chrome: Mutex<Option<Child>>,
}

impl BrowserRuntime {
    pub(crate) fn launch(state_dir: &Path, dma_buf: Option<DmaBufConfig>) -> Result<Self> {
        let profile = state_dir.join("chromium");
        std::fs::create_dir_all(&profile)?;
        let profile_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state_dir.join("chromium.lock"))?;
        if unsafe { libc::flock(profile_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!(
                "the persistent Rho browser identity is already in use by another rho-gui process"
            );
        }
        Ok(Self {
            compositor: BrowserCompositor::launch(dma_buf)?,
            profile,
            _profile_lock: profile_lock,
            chrome: Mutex::new(None),
        })
    }

    pub(crate) fn open(&self, target: &str, size: (u32, u32)) -> Result<BrowserSession> {
        validate_launch_url(target)?;
        let session = self.compositor.open(size)?;
        let mut command = Command::new(chrome_wrapper());
        command
            .env("WAYLAND_DISPLAY", self.compositor.socket_name())
            .env("XDG_SESSION_TYPE", "wayland")
            .env("XDG_ACTIVATION_TOKEN", session.activation_token())
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .arg("--ozone-platform=wayland")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg(format!(
                "--xdg-activation-token={}",
                session.activation_token()
            ))
            .arg(format!("--user-data-dir={}", self.profile.display()))
            .arg("--new-window")
            .arg(target);
        command.process_group(0);
        let child = command.spawn().context("launch stock Chromium")?;
        let mut primary = self.chrome.lock().unwrap();
        if primary
            .as_mut()
            .is_some_and(|child| child.try_wait().ok().flatten().is_none())
        {
            thread::spawn(move || {
                let mut child = child;
                let _ = child.wait();
            });
        } else {
            *primary = Some(child);
        }
        Ok(session)
    }
}

impl Drop for BrowserRuntime {
    fn drop(&mut self) {
        if let Some(mut child) = self.chrome.lock().unwrap().take() {
            unsafe { libc::kill(-(child.id() as i32), libc::SIGTERM) };
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_only_bounded_web_urls() {
        assert!(validate_launch_url("https://example.com/path").is_ok());
        assert!(validate_launch_url("file:///etc/passwd").is_err());
        assert!(validate_launch_url("javascript:alert(1)").is_err());
        assert!(validate_launch_url(&format!("https://example.com/{}", "x".repeat(8192))).is_err());
    }
}

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use anyhow::{Context as _, Result, bail};
use rho_browser_wayland::{BrowserCompositor, BrowserSession, DmaBufConfig, chrome_wrapper};
use serde_json::json;

use crate::native_host::{Bridge, socket_path, write_installation};
use crate::store::{PageId, PageRecord, validate_launch_url};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct BrowserWindow;

/// One stock Chromium identity, one normal Chrome window, and one private
/// Wayland compositor. Logical pages live as extension-owned tabs inside the
/// singleton browser window.
pub(crate) struct BrowserRuntime {
    _compositor: BrowserCompositor<BrowserWindow>,
    bridge: Bridge,
    _profile: PathBuf,
    _profile_lock: File,
    chrome: Mutex<Option<Child>>,
}

impl BrowserRuntime {
    pub(crate) fn launch(
        state_dir: &Path,
        dma_buf: DmaBufConfig,
    ) -> Result<(Self, BrowserSession<BrowserWindow>)> {
        let profile = state_dir.join("chromium");
        let extension = state_dir.join("chromium-extension");
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

        let executable = std::env::current_exe().context("resolve rho-gui executable")?;
        write_installation(&profile, &extension, &executable)?;
        configure_vertical_tabs(&profile)?;
        let bridge = Bridge::bind(socket_path()?)?;
        let compositor = BrowserCompositor::launch(dma_buf)?;
        let session = compositor.open(BrowserWindow, (1280, 720))?;

        let mut command = Command::new(chrome_wrapper());
        command
            .env("WAYLAND_DISPLAY", compositor.socket_name())
            .env("XDG_SESSION_TYPE", "wayland")
            .env("XDG_ACTIVATION_TOKEN", session.activation_token())
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .arg("--ozone-platform=wayland")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--restore-last-session")
            .arg("--enable-features=VerticalTabs")
            // Chrome 137–141 gates command-line extension loading behind this
            // feature. Newer branded builds require one manual Load unpacked;
            // Chromium and Chrome for Testing continue to honor the switch.
            .arg("--disable-features=DisableLoadExtensionCommandLineSwitch")
            .arg(format!("--load-extension={}", extension.display()))
            .arg(format!(
                "--xdg-activation-token={}",
                session.activation_token()
            ))
            .arg(format!("--user-data-dir={}", profile.display()));
        command.process_group(0);
        let child = command.spawn().context("launch stock Chromium")?;
        Ok((
            Self {
                _compositor: compositor,
                bridge,
                _profile: profile,
                _profile_lock: profile_lock,
                chrome: Mutex::new(Some(child)),
            },
            session,
        ))
    }

    pub(crate) fn create_page(&self, target: &str) -> Result<PageRecord> {
        validate_launch_url(target)?;
        let value = self.bridge.request("create", json!({ "url": target }))?;
        serde_json::from_value(value).context("decode created browser page")
    }

    pub(crate) fn focus_page(&self, id: PageId) -> Result<()> {
        self.bridge.request("focus", json!({ "id": id.0 }))?;
        Ok(())
    }

    pub(crate) fn close_page(&self, id: PageId) -> Result<()> {
        self.bridge.request("close", json!({ "id": id.0 }))?;
        Ok(())
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

/// Chrome owns this file while running; the profile lock guarantees it is
/// offline here. These ordinary, unprotected UI prefs only select Chrome's
/// native collapsed vertical-tab presentation.
fn configure_vertical_tabs(profile: &Path) -> Result<()> {
    let default = profile.join("Default");
    std::fs::create_dir_all(&default)?;
    let preferences = default.join("Preferences");
    let mut root = match std::fs::read(&preferences) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .context("decode Chrome Preferences")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.into()),
    };
    let object = root
        .as_object_mut()
        .context("Chrome Preferences root is not an object")?;
    let vertical = object
        .entry("vertical_tabs")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Chrome vertical_tabs preference is not an object")?;
    vertical.insert("enabled".into(), true.into());
    vertical.insert("collapsed_state".into(), true.into());
    vertical.insert("expand_on_hover".into(), false.into());
    let temporary = preferences.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec(&root)?)?;
    std::fs::rename(temporary, preferences)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enables_collapsed_vertical_tabs_without_losing_other_preferences() {
        let temp = tempfile::tempdir().unwrap();
        let default = temp.path().join("Default");
        std::fs::create_dir(&default).unwrap();
        std::fs::write(default.join("Preferences"), br#"{"other":{"kept":true}}"#).unwrap();
        configure_vertical_tabs(temp.path()).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(default.join("Preferences")).unwrap()).unwrap();
        assert_eq!(value["other"]["kept"], true);
        assert_eq!(value["vertical_tabs"]["enabled"], true);
        assert_eq!(value["vertical_tabs"]["collapsed_state"], true);
        assert_eq!(value["vertical_tabs"]["expand_on_hover"], false);
    }
}

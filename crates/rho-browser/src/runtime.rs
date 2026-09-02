use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context as _, Result, bail};
use rho_browser_wayland::{BrowserCompositor, BrowserRenderConfig, BrowserSession};
use serde_json::json;

use crate::native_host::{Bridge, SOCKET_PATH_ENV, write_installation};
use crate::store::{PageId, PageRecord, validate_launch_url};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct BrowserWindow;

/// One normal Brave Origin window and one private Wayland compositor. Logical
/// pages live as extension-owned tabs inside the singleton browser window.
pub(crate) struct BrowserRuntime {
    compositor: Mutex<Option<BrowserCompositor<BrowserWindow>>>,
    bridge: Mutex<Option<Arc<Bridge>>>,
    runtime_lock: Mutex<Option<File>>,
    chrome: Mutex<Option<Child>>,
    shutdown_started: AtomicBool,
}

impl BrowserRuntime {
    pub(crate) fn launch(
        state_dir: &Path,
        socket_path: &Path,
        render: BrowserRenderConfig,
    ) -> Result<(Self, BrowserSession<BrowserWindow>)> {
        let brave_config = dirs::config_dir().context("resolve user config directory")?;
        let extension = state_dir.join("chromium-extension");
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("create browser state directory {}", state_dir.display()))?;
        let runtime_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state_dir.join("chromium.lock"))?;
        if unsafe { libc::flock(runtime_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("the Rho browser runtime is already in use by another rho-gui process");
        }

        let executable = std::env::current_exe().context("resolve rho-gui executable")?;
        write_installation(&brave_config, &extension, &executable)?;
        let bridge = Bridge::bind(socket_path.to_owned())?;
        let software_shm = matches!(render, BrowserRenderConfig::SoftwareShmQa);
        let compositor = BrowserCompositor::launch(render)?;
        let session = compositor.open(BrowserWindow, (1280, 720))?;

        // PATH stays in sync with the deployed profile, unlike a store path
        // baked into the session environment at login.
        let browser =
            std::env::var_os("RHO_CUSTOM_BRAVE_BIN").unwrap_or_else(|| "rho-brave-origin".into());
        let mut command = Command::new(browser);
        // The Nix Brave wrapper supplies the first two, but Chromium honors
        // only the final --disable-features argument.
        let disabled_features = "OutdatedBuildDetector,UseChromeOSDirectVideoDecoder,DisableLoadExtensionCommandLineSwitch,WaylandOverlayDelegation";
        command
            .env("WAYLAND_DISPLAY", compositor.socket_name())
            .env("XDG_SESSION_TYPE", "wayland")
            .env(SOCKET_PATH_ENV, socket_path)
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .arg("--ozone-platform=wayland")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--restore-last-session")
            .arg(format!("--disable-features={disabled_features}"))
            .arg(format!("--rho-component-extension={}", extension.display()));
        if software_shm {
            command.arg("--disable-gpu");
        }
        command.process_group(0);
        let child = command.spawn().context("launch pinned Brave Origin")?;
        tracing::info!(pid = child.id(), "launched Brave Origin browser");
        Ok((
            Self {
                compositor: Mutex::new(Some(compositor)),
                bridge: Mutex::new(Some(Arc::new(bridge))),
                runtime_lock: Mutex::new(Some(runtime_lock)),
                chrome: Mutex::new(Some(child)),
                shutdown_started: AtomicBool::new(false),
            },
            session,
        ))
    }

    pub(crate) fn create_page(&self, target: &str) -> Result<PageRecord> {
        validate_launch_url(target)?;
        let bridge = self.bridge()?;
        let value = bridge.request("create", json!({ "url": target }))?;
        serde_json::from_value(value).context("decode created browser page")
    }

    pub(crate) fn focus_page(&self, id: PageId) -> Result<()> {
        self.bridge()?.request("focus", json!({ "id": id.0 }))?;
        Ok(())
    }

    pub(crate) fn close_page(&self, id: PageId) -> Result<()> {
        self.bridge()?.request("close", json!({ "id": id.0 }))?;
        Ok(())
    }

    pub(crate) fn list_pages(&self) -> Result<Vec<PageRecord>> {
        let value = self.bridge()?.request("list", json!({ "limit": 1000 }))?;
        serde_json::from_value(value).context("decode browser page list")
    }

    fn bridge(&self) -> Result<Arc<Bridge>> {
        self.bridge
            .lock()
            .unwrap()
            .clone()
            .context("browser runtime is shut down")
    }

    /// Reports whether the Chrome process has exited (or was already torn
    /// down). Abrupt browser death does not reliably surface as a compositor
    /// event — the client can disconnect before a toplevel ever bound — so
    /// callers holding a cached runtime must health-check before reuse.
    pub(crate) fn chrome_exited(&self) -> bool {
        match self.chrome.lock().unwrap().as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(Some(_)) | Err(_)),
            None => true,
        }
    }

    /// Starts one non-blocking teardown of Chrome and its private compositor.
    /// The runtime lock is released only after Chrome has been reaped.
    pub(crate) fn shutdown_background(&self) {
        if !begin_shutdown(&self.shutdown_started) {
            return;
        }
        let child = self.chrome.lock().unwrap().take();
        let compositor = self.compositor.lock().unwrap().take();
        let bridge = self.bridge.lock().unwrap().take();
        let runtime_lock = self.runtime_lock.lock().unwrap().take();
        thread::spawn(move || {
            if let Some(mut child) = child {
                unsafe { libc::kill(-(child.id() as i32), libc::SIGTERM) };
                let _ = child.wait();
            }
            drop(compositor);
            drop(bridge);
            drop(runtime_lock);
        });
    }
}

fn begin_shutdown(started: &AtomicBool) -> bool {
    started
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

impl Drop for BrowserRuntime {
    fn drop(&mut self) {
        self.shutdown_background();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_shutdown_starts_only_once() {
        let started = AtomicBool::new(false);
        assert!(begin_shutdown(&started));
        assert!(!begin_shutdown(&started));
    }
}

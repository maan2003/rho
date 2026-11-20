use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use rho_browser_wayland::{BrowserCompositor, BrowserRenderConfig, BrowserSession};
use serde_json::json;

use crate::native_host::{Bridge, CONNECT_TIMEOUT, SOCKET_PATH_ENV, write_installation};
use crate::store::{PageId, PageRecord, validate_launch_url};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct BrowserWindow;

/// One normal Brave Origin window and one private Wayland compositor. Logical
/// pages live as extension-owned tabs inside the singleton browser window.
pub(crate) struct BrowserRuntime {
    /// The binary that was launched, for saying what failed.
    browser: PathBuf,
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
        let browser = browser_binary()?;
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

        let mut command = Command::new(&browser);
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
        tracing::info!(pid = child.id(), browser = %browser.display(), "launched Brave Origin browser");
        Ok((
            Self {
                browser,
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
        self.await_extension(&bridge)?;
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

    fn bridge(&self) -> Result<Arc<Bridge>> {
        self.bridge
            .lock()
            .unwrap()
            .clone()
            .context("browser runtime is shut down")
    }

    /// Waits for the extension to connect while the browser lives: a
    /// browser that quits as it starts says so now, not when the
    /// connection times out.
    fn await_extension(&self, bridge: &Bridge) -> Result<()> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        while !bridge.is_connected() && Instant::now() < deadline {
            if let Some(status) = self.chrome_status() {
                return Err(match status.code() {
                    // What a shell or a wrapper's `exec` exits with when the
                    // program it runs is not there.
                    Some(127) => NoBrowser(format!(
                        "{} could not run the browser it wraps (exit 127)",
                        self.browser.display()
                    ))
                    .into(),
                    _ => anyhow!(
                        "the browser quit as it started ({status}): {}",
                        self.browser.display()
                    ),
                });
            }
            thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }

    /// How the Chrome process ended, while it has not been torn down.
    fn chrome_status(&self) -> Option<ExitStatus> {
        self.chrome.lock().unwrap().as_mut()?.try_wait().ok()?
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

/// No browser to launch: what was looked for, and where.
#[derive(Debug)]
pub(crate) struct NoBrowser(String);

impl std::fmt::Display for NoBrowser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no browser found · {}", self.0)
    }
}

impl std::error::Error for NoBrowser {}

/// `RHO_CUSTOM_BRAVE_BIN`, or `rho-brave-origin` on `PATH`, which stays in
/// sync with the deployed profile, unlike a store path baked into the
/// session environment at login.
fn browser_binary() -> Result<PathBuf, NoBrowser> {
    browser_binary_in(
        std::env::var_os("RHO_CUSTOM_BRAVE_BIN"),
        std::env::var_os("PATH"),
    )
}

fn browser_binary_in(
    custom: Option<OsString>,
    path: Option<OsString>,
) -> Result<PathBuf, NoBrowser> {
    if let Some(custom) = custom {
        let custom = PathBuf::from(custom);
        return match is_executable(&custom) {
            true => Ok(custom),
            false => Err(NoBrowser(format!(
                "RHO_CUSTOM_BRAVE_BIN names {}, which is no program",
                custom.display()
            ))),
        };
    }
    path.iter()
        .flat_map(std::env::split_paths)
        .map(|directory| directory.join(BROWSER))
        .find(|candidate| is_executable(candidate))
        .ok_or_else(|| {
            NoBrowser(format!(
                "install {BROWSER} on PATH, or name a browser in RHO_CUSTOM_BRAVE_BIN"
            ))
        })
}

/// The deployed Brave Origin wrapper's name.
const BROWSER: &str = "rho-brave-origin";

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
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
    fn the_browser_is_the_named_program_or_the_wrapper_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let program = |name: &str, mode: u32| {
            let path = dir.path().join(name);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        };
        let custom = program("brave", 0o755);
        let unrunnable = program("plain", 0o644);
        let empty = tempfile::tempdir().unwrap();
        let path = |dirs: &[&Path]| Some(std::env::join_paths(dirs).unwrap());

        // A named program wins over PATH, and must be one.
        assert_eq!(
            browser_binary_in(Some(custom.clone().into()), path(&[empty.path()])).unwrap(),
            custom
        );
        let error = browser_binary_in(Some(unrunnable.clone().into()), None).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "no browser found · RHO_CUSTOM_BRAVE_BIN names {}, which is no program",
                unrunnable.display()
            )
        );
        // Otherwise the wrapper is found in the first PATH entry holding a
        // runnable one; a file that does not run is passed over.
        let wrapper = program(BROWSER, 0o755);
        let shadow = tempfile::tempdir().unwrap();
        let shadowed = shadow.path().join(BROWSER);
        std::fs::write(&shadowed, "").unwrap();
        assert_eq!(
            browser_binary_in(None, path(&[shadow.path(), dir.path()])).unwrap(),
            wrapper
        );
        assert_eq!(
            browser_binary_in(None, path(&[empty.path()]))
                .unwrap_err()
                .to_string(),
            "no browser found · install rho-brave-origin on PATH, or name a browser in RHO_CUSTOM_BRAVE_BIN"
        );
        assert!(browser_binary_in(None, None).is_err());
    }

    #[test]
    fn runtime_shutdown_starts_only_once() {
        let started = AtomicBool::new(false);
        assert!(begin_shutdown(&started));
        assert!(!begin_shutdown(&started));
    }
}

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context as _, Result, bail};
use rho_browser_wayland::{
    BrowserCompositor, BrowserRenderConfig, BrowserSession, bubblewrap_wrapper, chrome_wrapper,
};
use serde_json::json;

use crate::native_host::{Bridge, socket_path, write_installation};
use crate::store::{PageId, PageRecord, validate_launch_url};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct BrowserWindow;

/// One persistent Brave Origin identity, one normal browser window, and one
/// private Wayland compositor. Logical pages live as extension-owned tabs
/// inside the singleton browser window.
pub(crate) struct BrowserRuntime {
    compositor: Mutex<Option<BrowserCompositor<BrowserWindow>>>,
    bridge: Mutex<Option<Arc<Bridge>>>,
    _profile: PathBuf,
    profile_lock: Mutex<Option<File>>,
    chrome: Mutex<Option<Child>>,
    shutdown_started: AtomicBool,
}

impl BrowserRuntime {
    pub(crate) fn launch(
        state_dir: &Path,
        render: BrowserRenderConfig,
    ) -> Result<(Self, BrowserSession<BrowserWindow>)> {
        let profile = state_dir.join("chromium");
        let brave_config = state_dir.join("brave-config");
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
        write_installation(&profile, &brave_config, &extension, &executable)?;
        configure_browser_chrome(&profile)?;
        let policy = write_brave_policy(state_dir)?;
        let bridge = Bridge::bind(socket_path()?)?;
        let software_shm = matches!(render, BrowserRenderConfig::SoftwareShmQa);
        let compositor = BrowserCompositor::launch(render)?;
        let session = compositor.open(BrowserWindow, (1280, 720))?;

        let custom_browser = std::env::var_os("RHO_CHROME_BIN").is_some();
        let mut command = if custom_browser {
            Command::new(chrome_wrapper())
        } else {
            let mut command = Command::new(bubblewrap_wrapper());
            command
                // Bubblewrap is a mount boundary, not the browser sandbox: expose the
                // host normally, but overlay /etc and mask host Brave policy so Rho's
                // mandatory policy is exclusive to this browser process tree.
                .args(["--bind", "/", "/", "--dev-bind", "/dev", "/dev"])
                .args(["--overlay-src", "/etc", "--tmp-overlay", "/etc"])
                .args([
                    "--dir",
                    "/etc/brave",
                    "--tmpfs",
                    "/etc/brave/policies",
                    "--dir",
                    "/etc/brave/policies/managed",
                    "--ro-bind",
                ])
                .arg(&policy)
                .arg("/etc/brave/policies/managed/rho.json")
                .arg("--")
                .arg(chrome_wrapper())
                // A Bubblewrap user namespace cannot use Brave's SUID helper. Brave
                // retains its namespace and Seccomp-BPF sandbox inside this boundary.
                .arg("--disable-setuid-sandbox");
            command
        };
        let disabled_features = if custom_browser {
            "DisableLoadExtensionCommandLineSwitch,WaylandOverlayDelegation"
        } else {
            // The Nix Brave wrapper supplies the first two, but Chromium honors
            // only the final --disable-features argument.
            "OutdatedBuildDetector,UseChromeOSDirectVideoDecoder,DisableLoadExtensionCommandLineSwitch,WaylandOverlayDelegation"
        };
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
            // Chrome 137–141 gates command-line extension loading behind this
            // feature. Newer branded builds require one manual Load unpacked;
            // Chromium and Chrome for Testing continue to honor the switch.
            .arg(format!("--disable-features={disabled_features}"))
            .arg(format!("--load-extension={}", extension.display()))
            .arg(format!(
                "--xdg-activation-token={}",
                session.activation_token()
            ))
            .arg(format!("--user-data-dir={}", profile.display()));
        // Chrome's experimental vertical tabs need a feature switch. Brave Origin's
        // native implementation is selected by profile preferences instead;
        // enabling Chromium's separate implementation crashes pinned Brave Origin.
        if custom_browser {
            command.arg("--enable-features=VerticalTabs");
        }
        if software_shm {
            command.arg("--disable-gpu");
        }
        if !custom_browser {
            // Brave only discovers per-user native messaging hosts under its
            // XDG config tree, not under --user-data-dir.
            command.env("XDG_CONFIG_HOME", &brave_config);
        }
        command.process_group(0);
        let child = command.spawn().context("launch pinned Brave Origin")?;
        tracing::info!(pid = child.id(), "launched Brave Origin browser");
        Ok((
            Self {
                compositor: Mutex::new(Some(compositor)),
                bridge: Mutex::new(Some(Arc::new(bridge))),
                _profile: profile,
                profile_lock: Mutex::new(Some(profile_lock)),
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

    /// Starts one non-blocking teardown of Chrome and its private compositor.
    /// The profile lock is released only after Chrome has been reaped.
    pub(crate) fn shutdown_background(&self) {
        if !begin_shutdown(&self.shutdown_started) {
            return;
        }
        let child = self.chrome.lock().unwrap().take();
        let compositor = self.compositor.lock().unwrap().take();
        let bridge = self.bridge.lock().unwrap().take();
        let profile_lock = self.profile_lock.lock().unwrap().take();
        thread::spawn(move || {
            if let Some(mut child) = child {
                unsafe { libc::kill(-(child.id() as i32), libc::SIGTERM) };
                let _ = child.wait();
            }
            drop(compositor);
            drop(bridge);
            drop(profile_lock);
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

/// Mandatory policy mounted privately into Brave Origin's Linux policy
/// directory.
const BRAVE_POLICY: &str = r#"{
  "BackgroundModeEnabled": false,
  "BraveAIChatEnabled": false,
  "BraveNewsDisabled": true,
  "BraveP3AEnabled": false,
  "BravePlaylistEnabled": false,
  "BraveRewardsDisabled": true,
  "BraveSpeedreaderEnabled": false,
  "BraveStatsPingEnabled": false,
  "BraveTalkDisabled": true,
  "BraveVPNDisabled": true,
  "BraveWalletDisabled": true,
  "BraveWaybackMachineEnabled": false,
  "BraveWebDiscoveryEnabled": false,
  "CommandLineFlagSecurityWarningsEnabled": false,
  "DefaultBrowserSettingEnabled": false,
  "HighEfficiencyModeEnabled": true,
  "MemorySaverModeSavings": 2,
  "MetricsReportingEnabled": false,
  "SyncDisabled": true,
  "TorDisabled": true
}"#;

fn write_brave_policy(state_dir: &Path) -> Result<PathBuf> {
    let path = state_dir.join("brave-policy.json");
    std::fs::write(&path, BRAVE_POLICY)?;
    Ok(path)
}

fn configure_browser_chrome(profile: &Path) -> Result<()> {
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
    vertical.insert("expand_on_hover".into(), true.into());
    set_preference(
        &mut root,
        &["brave", "tabs", "vertical_tabs_enabled"],
        true.into(),
    )?;
    set_preference(
        &mut root,
        &["brave", "tabs", "vertical_tabs_collapsed"],
        true.into(),
    )?;
    set_preference(
        &mut root,
        &[
            "brave",
            "tabs",
            "vertical_tabs_hide_completely_when_collapsed",
        ],
        true.into(),
    )?;
    set_preference(
        &mut root,
        &["brave", "always_show_bookmark_bar_on_ntp"],
        false.into(),
    )?;
    set_preference(
        &mut root,
        &["bookmark_bar", "show_tab_groups"],
        false.into(),
    )?;
    set_preference(&mut root, &["auto_pin_new_tab_groups"], false.into())?;
    // Rho owns and rewrites this isolated profile's unpacked extension. Keep
    // developer mode enabled so Brave reloads the command-line extension from
    // disk on startup instead of retaining an older registered service worker.
    set_preference(
        &mut root,
        &["extensions", "ui", "developer_mode"],
        true.into(),
    )?;
    // Rho terminates the managed browser process with SIGTERM and owns durable
    // page restoration. Without this offline reset Brave mislabels that managed
    // shutdown as a crash and overlays a Restore pages bubble on the next run.
    set_preference(&mut root, &["profile", "exit_type"], "Normal".into())?;
    let temporary = preferences.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec(&root)?)?;
    std::fs::rename(temporary, preferences)?;

    // Origin otherwise intercepts the first browser window with a product
    // onboarding dialog. Linux explicitly offers the branded build for free;
    // record that local choice before launch so extension-owned pages remain
    // the only windows in Rho's private compositor.
    let local_state = profile.join("Local State");
    let mut root = match std::fs::read(&local_state) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .context("decode Brave Origin Local State")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.into()),
    };
    set_preference(
        &mut root,
        &["brave", "origin", "free_tier_accepted"],
        true.into(),
    )?;
    let temporary = profile.join("Local State.tmp");
    std::fs::write(&temporary, serde_json::to_vec(&root)?)?;
    std::fs::rename(temporary, local_state)?;
    Ok(())
}

fn set_preference(
    root: &mut serde_json::Value,
    path: &[&str],
    value: serde_json::Value,
) -> Result<()> {
    let (name, parents) = path.split_last().context("preference path is empty")?;
    let mut current = root;
    for parent in parents {
        let object = current
            .as_object_mut()
            .context("Chrome Preferences entry is not an object")?;
        current = object
            .entry((*parent).to_owned())
            .or_insert_with(|| json!({}));
    }
    current
        .as_object_mut()
        .context("Chrome Preferences parent is not an object")?
        .insert((*name).to_owned(), value);
    Ok(())
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

    #[test]
    fn configures_hidden_brave_tabs_without_losing_other_preferences() {
        let temp = tempfile::tempdir().unwrap();
        let default = temp.path().join("Default");
        std::fs::create_dir(&default).unwrap();
        std::fs::write(default.join("Preferences"), br#"{"other":{"kept":true}}"#).unwrap();
        configure_browser_chrome(temp.path()).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(default.join("Preferences")).unwrap()).unwrap();
        assert_eq!(value["other"]["kept"], true);
        assert_eq!(value["vertical_tabs"]["enabled"], true);
        assert_eq!(value["vertical_tabs"]["collapsed_state"], true);
        assert_eq!(value["vertical_tabs"]["expand_on_hover"], true);
        assert_eq!(value["brave"]["tabs"]["vertical_tabs_enabled"], true);
        assert_eq!(value["brave"]["tabs"]["vertical_tabs_collapsed"], true);
        assert_eq!(
            value["brave"]["tabs"]["vertical_tabs_hide_completely_when_collapsed"],
            true
        );
        assert_eq!(value["brave"]["always_show_bookmark_bar_on_ntp"], false);
        assert_eq!(value["bookmark_bar"]["show_tab_groups"], false);
        assert_eq!(value["auto_pin_new_tab_groups"], false);
        assert_eq!(value["extensions"]["ui"]["developer_mode"], true);
        assert_eq!(value["profile"]["exit_type"], "Normal");
        let local_state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(temp.path().join("Local State")).unwrap())
                .unwrap();
        assert_eq!(local_state["brave"]["origin"]["free_tier_accepted"], true);
    }

    #[test]
    fn brave_policy_disables_consumer_features_and_telemetry() {
        let policy: serde_json::Value = serde_json::from_str(BRAVE_POLICY).unwrap();
        assert_eq!(policy["BraveRewardsDisabled"], true);
        assert_eq!(policy["BraveWalletDisabled"], true);
        assert_eq!(policy["BraveVPNDisabled"], true);
        assert_eq!(policy["BraveAIChatEnabled"], false);
        assert_eq!(policy["BraveNewsDisabled"], true);
        assert_eq!(policy["BraveP3AEnabled"], false);
        assert_eq!(policy["BraveStatsPingEnabled"], false);
        assert_eq!(policy["CommandLineFlagSecurityWarningsEnabled"], false);
        assert_eq!(policy["HighEfficiencyModeEnabled"], true);
        assert_eq!(policy["MemorySaverModeSavings"], 2);
        assert_eq!(policy["SyncDisabled"], true);
    }
}

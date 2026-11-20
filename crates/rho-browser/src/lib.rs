//! Native web-page resources for rho.
//!
//! The bundled extension owns client-local page identity and persistence; this
//! crate owns browser runtime integration and GPUI page views, and with the
//! `ui` feature the system that plugs pages into the frame ([`ui`]). The
//! browser runs in the GUI process; the agent host knows nothing of it.

#![cfg(target_os = "linux")]

pub mod native_host;
mod runtime;
mod store;
mod view;

#[cfg(feature = "ui")]
pub mod ui;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use gpui::{App, AppContext as _, Entity, Task};
pub use native_host::{
    ExtensionCommandStats, PageMetadata, TabStateEvent, snapshot_extension_command_stats,
    snapshot_tab_state_events,
};
use rho_browser_wayland::{BrowserRenderConfig, DmaBufConfig};
pub use rho_browser_wayland::{
    BrowserTiming, BrowserTimingKind, ExtensionFrameStats, record_browser_timing,
    snapshot_browser_timings, snapshot_extension_frame_stats,
};
use runtime::BrowserRuntime;
pub use store::{PageId, PageRecord};
pub use view::{
    BrowserModel as PageModel, BrowserView as PageView, HandoffEvent, PageMetadataChanged,
    snapshot_handoff_events,
};

/// The browser this process runs: one Chrome, launched the first time a
/// page is wanted and again if it died, and the one page model every page
/// view shares. Whoever owns this owns the browser; dropping it shuts
/// Chrome down.
pub struct Browser {
    state_dir: PathBuf,
    socket_path: PathBuf,
    running: Option<Running>,
}

struct Running {
    runtime: Arc<BrowserRuntime>,
    model: Entity<PageModel>,
}

impl Browser {
    /// A browser keeping its state under `state_dir`, its extension
    /// talking to it over `socket_path`. Nothing launches yet.
    pub fn new(state_dir: PathBuf, socket_path: PathBuf) -> Self {
        Self {
            state_dir,
            socket_path,
            running: None,
        }
    }

    /// The page model, launching the browser when none runs.
    pub fn model(&mut self, cx: &mut App) -> Result<Entity<PageModel>> {
        Ok(self.running(cx)?.model.clone())
    }

    fn running(&mut self, cx: &mut App) -> Result<&Running> {
        if let Some(running) = &self.running
            && running.runtime.chrome_exited()
        {
            tracing::warn!("browser process died without a compositor event; relaunching");
            running.runtime.shutdown_background();
            self.running = None;
        }
        if self.running.is_none() {
            let render = render_config();
            let (runtime, session) =
                BrowserRuntime::launch(&self.state_dir, &self.socket_path, render)?;
            let runtime = Arc::new(runtime);
            let model = cx.new(|cx| PageModel::new(runtime.clone(), session, cx));
            self.running = Some(Running { runtime, model });
        }
        Ok(self.running.as_ref().expect("launched above"))
    }

    pub fn create_page(&mut self, launch_url: String, cx: &mut App) -> Task<Result<PageRecord>> {
        let runtime = self.running(cx).map(|running| running.runtime.clone());
        cx.background_spawn(async move { runtime?.create_page(&launch_url) })
    }

    /// Brings `id` up in the browser, launching it when none runs.
    pub fn open_page(&mut self, id: PageId, cx: &mut App) -> Result<Entity<PageModel>> {
        let model = self.model(cx)?;
        model.update(cx, |model, cx| model.focus(id, cx));
        Ok(model)
    }

    /// Closes `id`'s tab; `None` when no browser runs, so there is no tab.
    pub fn close_page_if_running(&self, id: PageId, cx: &mut App) -> Option<Task<Result<()>>> {
        let running = self.running.as_ref()?;
        let runtime = running.runtime.clone();
        let close_barrier = running.model.update(cx, |model, _| model.prepare_close(id));
        Some(cx.background_spawn(async move {
            if let Some(close_barrier) = close_barrier? {
                close_barrier.await?;
            }
            runtime.close_page(id)
        }))
    }
}

fn render_config() -> BrowserRenderConfig {
    match gpui::linux_dmabuf_device() {
        Some(device) => BrowserRenderConfig::DmaBuf(DmaBufConfig {
            render_node: device.render_node.clone(),
            device_id: device.device_id,
            formats: device
                .formats
                .iter()
                .map(|format| (format.fourcc, format.modifier))
                .collect(),
        }),
        None => {
            tracing::info!(
                "GPUI's Vulkan device does not support DMA-BUF import; drawing browser pages in software"
            );
            BrowserRenderConfig::SoftwareShm
        }
    }
}

/// What a page is called while the browser has it open: its title, or the
/// host it is on.
pub fn live_page_name(id: PageId) -> Option<String> {
    let metadata = native_host::page_metadata(id)?;
    let title = metadata
        .title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|character| !character.is_control())
        .take(80)
        .collect::<String>();
    if !title.is_empty() {
        return Some(title);
    }
    url::Url::parse(&metadata.url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
}

/// Every tab the browser has open.
pub fn live_pages() -> Vec<(PageId, PageMetadata)> {
    native_host::page_metadata_entries()
}

#[cfg(test)]
mod page_name_tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn live_name_prefers_title_then_url_host() {
        let titled: PageId = "web-00000000-0000-4000-8000-000000000101".parse().unwrap();
        native_host::record_page_metadata(&json!({
            "event": "page-metadata",
            "page_id": titled.to_string(),
            "title": "Live title",
            "url": "https://fallback.example/path",
        }));
        assert_eq!(live_page_name(titled).as_deref(), Some("Live title"));

        let untitled: PageId = "web-00000000-0000-4000-8000-000000000102".parse().unwrap();
        native_host::record_page_metadata(&json!({
            "event": "page-metadata",
            "page_id": untitled.to_string(),
            "title": "",
            "url": "https://fallback.example/path",
        }));
        assert_eq!(
            live_page_name(untitled).as_deref(),
            Some("fallback.example")
        );
    }
}

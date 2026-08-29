//! Native web-page resources for rho.
//!
//! The bundled extension owns client-local page identity and persistence; this
//! crate owns browser runtime integration and GPUI page views. The daemon and
//! Desk remain unaware of client-local browser processes.

#![cfg(target_os = "linux")]

pub mod native_host;
mod runtime;
mod store;
mod view;

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use gpui::{App, AppContext as _, BorrowAppContext as _, Entity, Global, Task};
pub use native_host::{
    ExtensionCommandStats, TabStateEvent, snapshot_extension_command_stats,
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
    BrowserModel as PageModel, BrowserView as PageView, HandoffEvent, snapshot_handoff_events,
};

pub struct WebState {
    state_dir: std::path::PathBuf,
    runtime: Option<Arc<BrowserRuntime>>,
    model: Option<Entity<PageModel>>,
}

impl Global for WebState {}

pub fn init(state_dir: &Path, cx: &mut App) {
    cx.set_global(WebState {
        state_dir: state_dir.to_owned(),
        runtime: None,
        model: None,
    });
}

fn runtime(cx: &mut App) -> Result<Arc<BrowserRuntime>> {
    if let Some(runtime) = cx.global::<WebState>().runtime.clone() {
        if !runtime.chrome_exited() {
            return Ok(runtime);
        }
        tracing::warn!("browser process died without a compositor event; relaunching");
        runtime.shutdown_background();
        reset_runtime(&runtime, cx);
    }
    let render = match gpui::linux_dmabuf_device() {
        Some(device) => BrowserRenderConfig::DmaBuf(DmaBufConfig {
            render_node: device.render_node.clone(),
            device_id: device.device_id,
            formats: device
                .formats
                .iter()
                .map(|format| (format.fourcc, format.modifier))
                .collect(),
        }),
        None if std::env::var("RHO_BROWSER_SOFTWARE_SHM").as_deref() == Ok("1") => {
            tracing::warn!("using opt-in software SHM browser rendering");
            BrowserRenderConfig::SoftwareShmQa
        }
        None => {
            return Err(anyhow::anyhow!(
                "GPUI's Vulkan device does not support DMA-BUF import"
            ));
        }
    };
    let state_dir = cx.global::<WebState>().state_dir.clone();
    let (runtime, session) = BrowserRuntime::launch(&state_dir, render)?;
    let runtime = Arc::new(runtime);
    let model = cx.new(|cx| PageModel::new(runtime.clone(), session, cx));
    cx.update_global::<WebState, _>(|web, _| {
        web.runtime = Some(runtime.clone());
        web.model = Some(model);
    });
    Ok(runtime)
}

fn reset_runtime(expected: &Arc<BrowserRuntime>, cx: &mut App) {
    cx.update_global::<WebState, _>(|web, _| {
        if web
            .runtime
            .as_ref()
            .is_some_and(|runtime| Arc::ptr_eq(runtime, expected))
        {
            web.runtime = None;
            web.model = None;
        }
    });
}

pub fn create_page(launch_url: String, cx: &mut App) -> Task<Result<PageRecord>> {
    let runtime = runtime(cx);
    cx.background_spawn(async move { runtime?.create_page(&launch_url) })
}

pub fn open_page(id: PageId, cx: &mut App) -> Option<Entity<PageModel>> {
    runtime(cx).ok()?;
    let model = cx.global::<WebState>().model.clone()?;
    model.update(cx, |model, cx| model.focus(id, cx));
    Some(model)
}

pub fn close_page(id: PageId, cx: &mut App) -> Task<Result<()>> {
    let runtime = runtime(cx);
    close_page_with_runtime(id, runtime, cx)
}

pub fn close_page_if_running(id: PageId, cx: &mut App) -> Option<Task<Result<()>>> {
    let runtime = cx.global::<WebState>().runtime.clone()?;
    Some(close_page_with_runtime(id, Ok(runtime), cx))
}

fn close_page_with_runtime(
    id: PageId,
    runtime: Result<Arc<BrowserRuntime>>,
    cx: &mut App,
) -> Task<Result<()>> {
    let model = cx.global::<WebState>().model.clone();
    let close_barrier = model.map(|model| model.update(cx, |model, _| model.prepare_close(id)));
    cx.background_spawn(async move {
        if let Some(close_barrier) = close_barrier
            && let Some(close_barrier) = close_barrier?
        {
            close_barrier.await?;
        }
        runtime?.close_page(id)
    })
}

pub fn list_pages_if_running(cx: &mut App) -> Option<Task<Result<Vec<PageRecord>>>> {
    let runtime = cx.global::<WebState>().runtime.clone()?;
    Some(cx.background_spawn(async move { runtime.list_pages() }))
}

pub fn page_handle(id: PageId, _cx: &App) -> String {
    id.to_string()
}

pub fn page_name(page: &PageRecord) -> String {
    let host = url::Url::parse(&page.launch_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| page.launch_url.clone());
    let stem = host.strip_prefix("www.").unwrap_or(&host);
    let stem = stem
        .split('.')
        .next()
        .unwrap_or(stem)
        .replace(['-', '_'], " ");
    let mut characters = stem.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => "Web page".to_owned(),
    }
}

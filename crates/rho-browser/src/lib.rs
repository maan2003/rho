//! Native web-page resources for rho.
//!
//! This crate owns client-local page identity and persistence, browser runtime
//! integration, and GPUI page views. The daemon and Desk remain unaware of
//! client-local browser processes.

#![cfg(target_os = "linux")]

mod runtime;
mod store;
mod view;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use gpui::{App, AppContext as _, BorrowAppContext as _, Entity, Global, Task, WeakEntity};
use rho_browser_wayland::DmaBufConfig;
use runtime::BrowserRuntime;
pub use store::{PageId, PageRecord, WebStore, WindowId, WindowRecord};
pub use view::{BrowserModel as PageModel, BrowserView as PageView};

pub struct WebState {
    store: WebStore,
    state_dir: std::path::PathBuf,
    runtime: Option<Arc<BrowserRuntime>>,
    models: HashMap<PageId, WeakEntity<PageModel>>,
}

impl Global for WebState {}

pub fn init(store: WebStore, state_dir: &Path, cx: &mut App) {
    cx.set_global(WebState {
        store,
        state_dir: state_dir.to_owned(),
        runtime: None,
        models: HashMap::new(),
    });
}

fn runtime(cx: &mut App) -> Result<Arc<BrowserRuntime>> {
    if let Some(runtime) = cx.global::<WebState>().runtime.clone() {
        return Ok(runtime);
    }
    let dma_buf = gpui::linux_dmabuf_device().map(|device| DmaBufConfig {
        render_node: device.render_node.clone(),
        device_id: device.device_id,
        formats: device
            .formats
            .iter()
            .map(|format| (format.fourcc, format.modifier))
            .collect(),
    });
    let state_dir = cx.global::<WebState>().state_dir.clone();
    let runtime = Arc::new(BrowserRuntime::launch(&state_dir, dma_buf)?);
    cx.update_global::<WebState, _>(|web, _| web.runtime = Some(runtime.clone()));
    Ok(runtime)
}

pub fn create_page(launch_url: String, cx: &mut App) -> Task<Result<PageRecord>> {
    let store = cx.global::<WebState>().store.clone();
    cx.background_spawn(async move { store.create_page(launch_url).await })
}

pub fn open_page_record(record: PageRecord, cx: &mut App) -> Entity<PageModel> {
    if let Some(model) = cx
        .global::<WebState>()
        .models
        .get(&record.id)
        .and_then(WeakEntity::upgrade)
    {
        return model;
    }
    let id = record.id;
    let launch = runtime(cx).and_then(|runtime| runtime.open(&record.launch_url, (1280, 720)));
    let model = cx.new(|cx| PageModel::new_record(record, launch, cx));
    cx.update_global::<WebState, _>(|web, _| {
        web.models.insert(id, model.downgrade());
    });
    model
}

pub fn open_page(id: PageId, cx: &mut App) -> Option<Entity<PageModel>> {
    if let Some(model) = cx
        .global::<WebState>()
        .models
        .get(&id)
        .and_then(WeakEntity::upgrade)
    {
        return Some(model);
    }
    let record = cx.global::<WebState>().store.get_page(id)?;
    Some(open_page_record(record, cx))
}

pub fn pages(cx: &App) -> Vec<PageRecord> {
    cx.global::<WebState>().store.list_pages()
}

pub fn page_handle(id: PageId, cx: &App) -> String {
    cx.global::<WebState>().store.page_handle(id)
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

//! Browser GPUI root for the portable Rho views.
//!
//! The direct iroh connection is enabled in a later stage. Until then this
//! root owns the same dashboard view and shared registry model, so a
//! `gpui_web` application can mount the real rho-gui view graph without
//! pulling in native connection, project, shell, terminal, audio, or CLI code.

use gpui::prelude::*;
use gpui::{Context, Render, Window, div, px};
use rho_ui_proto::AgentId;
use theme::ActiveTheme as _;

use crate::dashboard::Dashboard;
use crate::registry::AgentRegistry;

/// Browser-side owner of the portable dashboard and transcript views.
pub struct Workspace {
    dashboard: Dashboard,
    registry: AgentRegistry,
}

impl Workspace {
    /// Construct the disconnected browser client. Stage 2 will populate the
    /// registry over the same direct iroh protocol used by `webui` today.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            dashboard: Dashboard::new(window, cx),
            registry: AgentRegistry::default(),
        }
    }

    pub fn registry_mut(&mut self) -> &mut AgentRegistry {
        &mut self.registry
    }

    pub fn dashboard(&self) -> &Dashboard {
        &self.dashboard
    }

    pub(crate) fn finish_initial_agent_load(
        &mut self,
        _agent_id: AgentId,
        _cx: &mut Context<Self>,
    ) {
    }

    pub(crate) fn mark_draft_active_from_edit(&mut self, _cx: &mut Context<Self>) {}

    pub(crate) fn refresh_minibuffer(&mut self, _cx: &mut Context<Self>) {}
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.dashboard.sync(&self.registry, window, cx);
        div()
            .id("rho-gui")
            .size_full()
            .p(px(2.))
            .bg(cx.theme().colors().editor_background)
            .key_context("RhoGui")
            .child(self.dashboard.editor().clone())
    }
}

/// Wall-clock milliseconds used by duration rendering in portable views.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

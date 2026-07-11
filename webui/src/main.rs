//! Rho web UI: a static Leptos app that connects to the rho daemon over
//! iroh (`rho_webui_messages::ALPN`) and renders a click-oriented
//! agent list + chat surface. Built with trunk; the `dist/` output is
//! hostable on any static host.

mod conn;
mod md;
mod ui;

use leptos::prelude::*;
use rho_webui_messages::{AgentState, FromBrowser, Topic, Workdir};

/// Connection lifecycle as shown to the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// No daemon endpoint id known yet; ask for one.
    NeedDaemon,
    /// A daemon id is known, but WebAuthn waits for an explicit user gesture.
    Unlock(String),
    Connecting,
    /// Connected but not yet enrolled; show `rho iroh approve <code>`.
    Enroll(String),
    Online,
    Failed(String),
}

#[derive(Clone, Copy)]
pub struct App {
    pub phase: RwSignal<Phase>,
    pub topics: RwSignal<Vec<Topic>>,
    pub workdirs: RwSignal<Vec<Workdir>>,
    /// Encoded id of the agent open in the chat pane.
    pub selected: RwSignal<Option<String>>,
    /// Transcript of the selected agent, once the daemon has sent it.
    pub state: RwSignal<Option<AgentState>>,
    /// Mobile: chat pane covers the agent list.
    pub chat_open: RwSignal<bool>,
    pub show_new_agent: RwSignal<bool>,
    pub toast: RwSignal<Option<String>>,
    sender: StoredValue<futures::channel::mpsc::UnboundedSender<FromBrowser>>,
}

impl App {
    fn new(sender: futures::channel::mpsc::UnboundedSender<FromBrowser>) -> Self {
        Self {
            phase: RwSignal::new(Phase::NeedDaemon),
            topics: RwSignal::new(Vec::new()),
            workdirs: RwSignal::new(Vec::new()),
            selected: RwSignal::new(None),
            state: RwSignal::new(None),
            chat_open: RwSignal::new(false),
            show_new_agent: RwSignal::new(false),
            toast: RwSignal::new(None),
            sender: StoredValue::new(sender),
        }
    }

    pub fn send(&self, message: FromBrowser) {
        let _ = self.sender.get_value().unbounded_send(message);
    }

    pub fn select(&self, agent_id: String) {
        self.send(FromBrowser::Select {
            agent_id: agent_id.clone(),
        });
        self.state.set(None);
        self.selected.set(Some(agent_id));
        self.chat_open.set(true);
    }

    pub fn show_toast(&self, message: String) {
        self.toast.set(Some(message));
        let toast = self.toast;
        leptos::task::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(6000).await;
            toast.set(None);
        });
    }
}

fn main() {
    console_error_panic_hook::set_once();
    let (sender, receiver) = futures::channel::mpsc::unbounded();
    let app = App::new(sender);
    if is_framed() {
        app.phase.set(Phase::Failed(
            "Rho cannot run inside another page. Open it in a top-level tab.".to_owned(),
        ));
    }
    // Mount first: it initializes the wasm executor `conn` spawns onto.
    leptos::mount::mount_to_body(move || ui::Root(app));
    conn::init(app, receiver);
}

fn is_framed() -> bool {
    let Some(window) = web_sys::window() else {
        return true;
    };
    match window.top() {
        Ok(Some(top)) => !js_sys::Object::is(top.as_ref(), window.as_ref()),
        _ => true,
    }
}

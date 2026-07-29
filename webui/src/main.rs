//! Rho web UI: a static Leptos client for the daemon's native `rho/ui/3`
//! protocol over iroh.

mod conn;
mod md;
mod ui;

use futures::channel::mpsc::UnboundedSender;
use leptos::prelude::*;
use rho_registry::AgentRegistry;
use rho_registry::session::AgentSubscriptions;
use rho_registry::store::AgentStore;
use rho_ui_proto::{AgentId, ClientMessage, UiProject};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    NeedDaemon,
    Unlock(String),
    Connecting,
    Enroll(String),
    Online,
    Failed(String),
}

#[derive(Clone, Copy)]
pub struct App {
    pub phase: RwSignal<Phase>,
    pub registry: StoredValue<AgentRegistry>,
    /// Invalidates views after an in-place registry mutation.
    pub registry_epoch: RwSignal<u64>,
    pub projects: RwSignal<Vec<UiProject>>,
    pub selected: RwSignal<Option<AgentId>>,
    pub store: StoredValue<AgentStore>,
    pub state_epoch: RwSignal<u64>,
    pub subscriptions: StoredValue<AgentSubscriptions>,
    pub chat_open: RwSignal<bool>,
    pub show_new_agent: RwSignal<bool>,
    pub toast: RwSignal<Option<String>>,
    sender: StoredValue<UnboundedSender<ClientMessage>>,
}

impl App {
    fn new(sender: UnboundedSender<ClientMessage>) -> Self {
        Self {
            phase: RwSignal::new(Phase::NeedDaemon),
            registry: StoredValue::new(AgentRegistry::default()),
            registry_epoch: RwSignal::new(0),
            projects: RwSignal::new(Vec::new()),
            selected: RwSignal::new(None),
            store: StoredValue::new(AgentStore::default()),
            state_epoch: RwSignal::new(0),
            subscriptions: StoredValue::new(AgentSubscriptions::default()),
            chat_open: RwSignal::new(false),
            show_new_agent: RwSignal::new(false),
            toast: RwSignal::new(None),
            sender: StoredValue::new(sender),
        }
    }

    pub fn send(&self, message: ClientMessage) {
        let _ = self.sender.get_value().unbounded_send(message);
    }

    pub fn mutate_registry(&self, f: impl FnOnce(&mut AgentRegistry)) {
        self.registry.update_value(f);
        self.registry_epoch.update(|epoch| *epoch += 1);
    }

    pub fn select(&self, agent_id: AgentId) {
        self.subscribe_agent(agent_id);
        // Weight the on-screen agent's state stream above the others.
        self.send(ClientMessage::AgentStreamFocus {
            agent_id: Some(agent_id),
        });
        self.mutate_registry(|registry| registry.select_agent(agent_id));
        self.selected.set(Some(agent_id));
        self.chat_open.set(true);
        self.show_new_agent.set(false);
    }

    /// Manage this client's bounded transcript subscription set with the
    /// same LRU policy as the native GUI.
    fn subscribe_agent(&self, agent_id: AgentId) {
        let mut action = (false, None);
        self.subscriptions
            .update_value(|subscriptions| action = subscriptions.touch(agent_id));
        let (subscribe, evicted) = action;
        if let Some(evicted) = evicted {
            self.send(ClientMessage::UnsubscribeAgents {
                agent_ids: vec![evicted],
            });
        }
        if subscribe {
            self.send(ClientMessage::SubscribeAgents {
                agent_ids: vec![agent_id],
            });
        }
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

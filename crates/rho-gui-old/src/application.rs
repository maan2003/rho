//! rho-gui as the desktop's one running rho: it owns `dev.rho.Gui` on the
//! session bus and answers `org.freedesktop.Application` there, the freedesktop
//! Desktop Entry spec's D-Bus activation. Only one rho-gui may run (it holds
//! the client database), so whatever would start a second one hands the first
//! what it was started with instead: drv-appd through the app's shim, a desktop
//! through `DBusActivatable`, or a second `rho-gui <link>`, which finds the
//! name taken.
//!
//! Everything that arrives here is untrusted. A link only opens the prompt that
//! asks the user to name its host; nothing attaches without them.

use std::collections::HashMap;

use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{App, Window, WindowHandle};
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::zvariant::{OwnedValue, Value};

use crate::workspace::Workspace;

/// The desktop app id, which the spec makes the bus name.
pub const NAME: &str = "dev.rho.Gui";
/// The spec's object path for [`NAME`].
const PATH: &str = "/dev/rho/Gui";
const INTERFACE: &str = "org.freedesktop.Application";
/// The `platform_data` key of a Wayland xdg-activation token.
const ACTIVATION_TOKEN: &str = "activation-token";

/// What the running rho-gui is asked.
#[derive(Debug)]
enum Request {
    /// Bring the window forward and ask about each link.
    Open {
        uris: Vec<String>,
        token: Option<String>,
    },
    /// Bring the window forward.
    Activate { token: Option<String> },
}

/// What [`claim`] found.
pub enum Claim {
    /// The name is this process's. What is asked of it waits in here until
    /// [`serve`] has a window to answer with, so nothing that comes during
    /// startup is lost.
    Ours(Service),
    /// Another rho-gui owns it and was handed what this one was started with.
    HandedOff,
}

pub struct Service {
    /// Owns the name and serves the interface while it lives.
    _bus: zbus::blocking::Connection,
    requests: UnboundedReceiver<Request>,
}

struct Application {
    requests: UnboundedSender<Request>,
}

fn token_of(platform_data: &HashMap<String, OwnedValue>) -> Option<String> {
    platform_data
        .get(ACTIVATION_TOKEN)
        .and_then(|value| String::try_from(value.clone()).ok())
        .filter(|token| !token.is_empty())
}

#[zbus::interface(name = "org.freedesktop.Application")]
impl Application {
    fn activate(&self, platform_data: HashMap<String, OwnedValue>) {
        let token = token_of(&platform_data);
        let _ = self.requests.unbounded_send(Request::Activate { token });
    }

    fn open(&self, uris: Vec<String>, platform_data: HashMap<String, OwnedValue>) {
        let token = token_of(&platform_data);
        let _ = self.requests.unbounded_send(Request::Open { uris, token });
    }

    fn activate_action(
        &self,
        action_name: String,
        _parameter: Vec<OwnedValue>,
        _platform_data: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(format!(
            "rho-gui has no action {action_name:?}"
        )))
    }
}

/// Takes [`NAME`] on the session bus, before anything else of rho-gui starts.
/// With `link`, the link this process was started with: it is the first
/// request when the name is ours, and it is handed to the owner when it is not.
pub fn claim(link: Option<String>) -> zbus::Result<Claim> {
    let (sender, requests) = unbounded();
    if let Some(link) = &link {
        let _ = sender.unbounded_send(Request::Open {
            uris: vec![link.clone()],
            token: None,
        });
    }
    let bus = zbus::blocking::connection::Builder::session()?
        .serve_at(PATH, Application { requests: sender })?
        .build()?;
    // zbus answers `Exists` with `NameTaken`.
    match bus.request_name_with_flags(NAME, RequestNameFlags::DoNotQueue.into()) {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
            Ok(Claim::Ours(Service {
                _bus: bus,
                requests,
            }))
        }
        Ok(RequestNameReply::Exists | RequestNameReply::InQueue) | Err(zbus::Error::NameTaken) => {
            hand_off(&bus, link)?;
            Ok(Claim::HandedOff)
        }
        Err(error) => Err(error),
    }
}

/// Asks the rho-gui that owns [`NAME`] to open `link`, or to come forward,
/// with the activation token this process was started with, if any.
fn hand_off(bus: &zbus::blocking::Connection, link: Option<String>) -> zbus::Result<()> {
    let mut platform_data: HashMap<&str, Value<'_>> = HashMap::new();
    if let Ok(token) = std::env::var("XDG_ACTIVATION_TOKEN") {
        platform_data.insert(ACTIVATION_TOKEN, Value::from(token));
    }
    match link {
        Some(link) => bus.call_method(
            Some(NAME),
            PATH,
            Some(INTERFACE),
            "Open",
            &(vec![link], platform_data),
        )?,
        None => bus.call_method(
            Some(NAME),
            PATH,
            Some(INTERFACE),
            "Activate",
            &(platform_data,),
        )?,
    };
    Ok(())
}

/// Answers what is asked of the name with `window`, from now on and what
/// waited for it first. Only `rho://` links are taken; each opens the prompt
/// that asks the user to name the host it reaches.
pub fn serve(service: Service, window: WindowHandle<Workspace>, cx: &mut App) {
    let Service {
        _bus: bus,
        mut requests,
    } = service;
    cx.spawn(async move |cx| {
        // The name goes with the connection; it is held as long as requests
        // come.
        let _bus = bus;
        while let Some(request) = requests.next().await {
            tracing::info!(?request, "asked over {INTERFACE}");
            let _ = window.update(cx, |workspace, window, cx| match request {
                Request::Activate { token } => raise(window, token),
                Request::Open { uris, token } => {
                    raise(window, token);
                    for uri in uris {
                        if uri.starts_with(rho_iroh_auth::PAIRING_SCHEME) {
                            workspace.prompt_link_name(uri, window, cx);
                        } else {
                            tracing::warn!(%uri, "asked to open a URI that is no pairing link");
                        }
                    }
                }
            });
        }
    })
    .detach();
}

fn raise(window: &mut Window, token: Option<String>) {
    match token {
        Some(token) => window.activate_window_with_token(token),
        None => window.activate_window(),
    }
}

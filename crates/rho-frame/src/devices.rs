//! What the user says about the devices a host trusts (`hosts.Device`):
//! each device's key, which the host trusts once a pairing link or an
//! enrollment code made it, and which a lost device's owner takes back.
//! From `Commands` on any screen, asking for a host when more than one is
//! attached.

use std::rc::Rc;

use gpui::{Context, Window};
use rho_agent_hosts::HostId;
use rho_agent_hosts::protocol::{IrohDevice, IrohDevices, IrohRevoke};
use rho_window::style::StyleClass;

use crate::command::{Command, Spelling};
use crate::frame::Frame;
use crate::gccp::{matching, tell};
use crate::minibuffer::Candidate;

const LIST: &str = "list devices";
const REVOKE: &str = "revoke device";

pub static LIST_DEVICES: Spelling = Spelling {
    name: LIST,
    aliases: &[("devices", 9), ("trusted devices", 7), ("my devices", 6)],
    argument: None,
};

pub static REVOKE_DEVICE: Spelling = Spelling {
    name: REVOKE,
    aliases: &[
        ("lost device", 8),
        ("lost laptop", 7),
        ("lost phone", 7),
        ("untrust device", 6),
    ],
    argument: None,
};

/// The device commands, each asking for a host when more than one is
/// attached.
pub(crate) fn commands() -> Vec<Command> {
    vec![
        Command::simple(&LIST_DEVICES, |frame, window, cx| {
            frame.choose_host(LIST, Rc::new(list), window, cx)
        }),
        Command::simple(&REVOKE_DEVICE, |frame, window, cx| {
            frame.choose_host(REVOKE, Rc::new(revoke), window, cx)
        }),
    ]
}

/// Every device the host trusts, one row each by its key; enter copies
/// the key.
fn list(frame: &mut Frame, host: HostId, window: &mut Window, cx: &mut Context<Frame>) {
    let answer = frame.hosts().call(host, IrohDevices);
    frame.when_host_answers(
        LIST,
        host,
        answer,
        move |frame, devices, window, cx| {
            let name = frame.hosts().host_label(host);
            if devices.is_empty() {
                return frame.echo(
                    &format!("{LIST}: {name} trusts no device's key"),
                    StyleClass::SystemInfo,
                    cx,
                );
            }
            let rows: Rc<Vec<Candidate>> = Rc::new(devices.iter().map(row).collect());
            let offered = rows.clone();
            frame.open_prompt(
                format!("devices {name} trusts · enter copies:"),
                Rc::new(move |_, needle, _| matching(&offered, needle)),
                Rc::new(move |frame, input, _, cx| {
                    let input = input.trim();
                    match rows.iter().any(|row| row.value == input) {
                        true => {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                input.to_owned(),
                            ));
                            frame.echo(&format!("copied: {input}"), StyleClass::SystemInfo, cx);
                        }
                        false => frame.echo(
                            &format!("{LIST}: {name} trusts no device {input:?}"),
                            StyleClass::SystemInfo,
                            cx,
                        ),
                    }
                }),
                window,
                cx,
            );
        },
        window,
        cx,
    );
}

/// Takes back the host's trust in a device other than this one, chosen
/// from those it trusts: the device must enroll again to reach the host.
fn revoke(frame: &mut Frame, host: HostId, window: &mut Window, cx: &mut Context<Frame>) {
    let answer = frame.hosts().call(host, IrohDevices);
    frame.when_host_answers(
        REVOKE,
        host,
        answer,
        move |frame, devices, window, cx| {
            let name = frame.hosts().host_label(host);
            let (asking, others): (Vec<IrohDevice>, Vec<IrohDevice>) =
                devices.into_iter().partition(|device| device.asking);
            if others.is_empty() {
                return frame.echo(
                    &format!("{REVOKE}: {name} trusts no other device"),
                    StyleClass::SystemInfo,
                    cx,
                );
            }
            let offered: Vec<Candidate> = others.iter().map(row).collect();
            frame.open_prompt(
                format!("revoke a device {name} trusts:"),
                Rc::new(move |_, needle, _| matching(&offered, needle)),
                Rc::new(move |frame, input, _, cx| {
                    let input = input.trim();
                    if asking.iter().any(|device| device.endpoint_id == input) {
                        return frame.echo(
                            &format!("{REVOKE}: that is this device · revoke it from another"),
                            StyleClass::SystemInfo,
                            cx,
                        );
                    }
                    if !others.iter().any(|device| device.endpoint_id == input) {
                        return frame.echo(
                            &format!("{REVOKE}: {name} trusts no device {input:?}"),
                            StyleClass::SystemInfo,
                            cx,
                        );
                    }
                    let said = format!("revoked: {name} no longer trusts {input}");
                    let answer = frame.hosts().call(
                        host,
                        IrohRevoke {
                            endpoint_id: input.to_owned(),
                        },
                    );
                    tell(
                        frame,
                        REVOKE,
                        host,
                        async move { answer.await.map(|_| ()) },
                        said,
                        cx,
                    );
                }),
                window,
                cx,
            );
        },
        window,
        cx,
    );
}

/// A device as a row: its key, and when the host came to trust it.
fn row(device: &IrohDevice) -> Candidate {
    let since = jiff::Timestamp::from_second(device.trusted_at_unix_secs as i64).map_or_else(
        |_| "trusted".to_owned(),
        |at| format!("trusted since {}", at.strftime("%Y-%m-%d")),
    );
    Candidate {
        value: device.endpoint_id.clone(),
        description: match device.asking {
            true => format!("this device · {since}"),
            false => since,
        },
    }
}

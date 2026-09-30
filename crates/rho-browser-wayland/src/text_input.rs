//! Minimal text-input-v3 transport for explicit, user-committed browser text.
//! Chromium owns field focus and Enable/Disable; this is not a second editor.
//! Whole-text commits leave replacement/selection behavior to Chromium.

use smithay::reexports::wayland_protocols::wp::text_input::zv3::server::{
    zwp_text_input_manager_v3::{self, ZwpTextInputManagerV3},
    zwp_text_input_v3::{self, ZwpTextInputV3},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    protocol::wl_surface::WlSurface,
};
use super::{BrowserPageKey, State};

#[derive(Default)]
struct CommitState {
    serial: u32,
    enabled: bool,
    pending_enabled: Option<bool>,
}

impl CommitState {
    fn commit(&mut self) {
        self.serial = self.serial.wrapping_add(1);
        if let Some(enabled) = self.pending_enabled.take() {
            self.enabled = enabled;
        }
    }
}

struct TextInput {
    resource: ZwpTextInputV3,
    state: CommitState,
}

#[derive(Default)]
pub(super) struct BrowserTextInput {
    focus: Option<WlSurface>,
    inputs: Vec<TextInput>,
}

impl BrowserTextInput {
    pub(super) fn focus_changed(&mut self, focus: Option<&WlSurface>) {
        if self.focus.as_ref() == focus {
            return;
        }
        for input in &mut self.inputs {
            if let Some(old) = &self.focus
                && old.id().same_client_as(&input.resource.id())
            {
                input.resource.leave(old);
            }
            // Text-input-v3 state belongs to the entered surface, not the
            // client's next field or popup.
            input.state.enabled = false;
            input.state.pending_enabled = None;
            if let Some(new) = focus
                && new.id().same_client_as(&input.resource.id())
            {
                input.resource.enter(new);
            }
        }
        self.focus = focus.cloned();
    }

    pub(super) fn type_text(&self, text: String) -> anyhow::Result<()> {
        let focus = self
            .focus
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("browser has no focused text field"))?;
        let input = self
            .inputs
            .iter()
            .find(|input| {
                input.state.enabled
                    && input.resource.is_alive()
                    && focus.id().same_client_as(&input.resource.id())
            })
            .ok_or_else(|| anyhow::anyhow!("click a browser text field before typing"))?;
        input.resource.commit_string(Some(text));
        input.resource.done(input.state.serial);
        Ok(())
    }
}

impl<K: BrowserPageKey> GlobalDispatch<ZwpTextInputManagerV3, ()> for State<K> {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwpTextInputManagerV3>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl<K: BrowserPageKey> Dispatch<ZwpTextInputManagerV3, ()> for State<K> {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpTextInputManagerV3,
        request: zwp_text_input_manager_v3::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwp_text_input_manager_v3::Request::GetTextInput { id, .. } = request {
            let resource = data_init.init(id, ());
            if let Some(focus) = &state.text_input.focus
                && focus.id().same_client_as(&resource.id())
            {
                resource.enter(focus);
            }
            state.text_input.inputs.push(TextInput {
                resource,
                state: CommitState::default(),
            });
        }
    }
}

impl<K: BrowserPageKey> Dispatch<ZwpTextInputV3, ()> for State<K> {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &ZwpTextInputV3,
        request: zwp_text_input_v3::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let focused = state
            .text_input
            .focus
            .as_ref()
            .is_some_and(|focus| focus.id().same_client_as(&resource.id()));
        let Some(input) = state
            .text_input
            .inputs
            .iter_mut()
            .find(|input| input.resource == *resource)
        else {
            return;
        };
        match request {
            // Commit serials count even when there is no entered surface.
            zwp_text_input_v3::Request::Commit => input.state.commit(),
            zwp_text_input_v3::Request::Enable if focused => {
                input.state.pending_enabled = Some(true)
            }
            zwp_text_input_v3::Request::Disable if focused => {
                input.state.pending_enabled = Some(false)
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _: smithay::reexports::wayland_server::backend::ClientId,
        resource: &ZwpTextInputV3,
        _: &(),
    ) {
        state
            .text_input
            .inputs
            .retain(|input| input.resource != *resource);
    }
}

pub(super) fn create_global<K: BrowserPageKey>(dh: &DisplayHandle) {
    dh.create_global::<State<K>, ZwpTextInputManagerV3, _>(1, ());
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestState;
    struct TestClient;
    impl smithay::reexports::wayland_server::backend::ClientData for TestClient {}

    impl Dispatch<ZwpTextInputV3, ()> for TestState {
        fn request(
            _: &mut Self,
            _: &Client,
            _: &ZwpTextInputV3,
            _: zwp_text_input_v3::Request,
            _: &(),
            _: &DisplayHandle,
            _: &mut DataInit<'_, Self>,
        ) {
        }
    }
    impl Dispatch<WlSurface, ()> for TestState {
        fn request(
            _: &mut Self,
            _: &Client,
            _: &WlSurface,
            _: smithay::reexports::wayland_server::protocol::wl_surface::Request,
            _: &(),
            _: &DisplayHandle,
            _: &mut DataInit<'_, Self>,
        ) {
        }
    }

    fn events(socket: &mut std::os::unix::net::UnixStream) -> Vec<(u16, Vec<u8>)> {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            match socket.read(&mut chunk) {
                Ok(0) => break,
                Ok(len) => bytes.extend_from_slice(&chunk[..len]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("{error}"),
            }
        }
        let mut result = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let header = u32::from_ne_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
            let len = (header >> 16) as usize;
            result.push((header as u16, bytes[offset + 8..offset + len].to_vec()));
            offset += len;
        }
        result
    }

    #[test]
    fn whole_unicode_commit_targets_enabled_client_and_emits_matching_done_serial() {
        use std::sync::Arc;

        use smithay::reexports::wayland_server::Display;
        let display = Display::<TestState>::new().unwrap();
        let mut dh = display.handle();
        let (server, mut socket) = std::os::unix::net::UnixStream::pair().unwrap();
        socket.set_nonblocking(true).unwrap();
        let client = dh.insert_client(server, Arc::new(TestClient)).unwrap();
        let surface = client
            .create_resource::<WlSurface, _, TestState>(&dh, 1, ())
            .unwrap();
        let resource = client
            .create_resource::<ZwpTextInputV3, _, TestState>(&dh, 1, ())
            .unwrap();
        let mut browser = BrowserTextInput::default();
        browser.inputs.push(TextInput {
            resource,
            state: CommitState::default(),
        });
        assert!(browser.type_text("No focus".into()).is_err());
        browser.focus_changed(Some(&surface));
        dh.flush_clients().unwrap();
        events(&mut socket); // Enter
        assert!(browser.type_text("No enabled field".into()).is_err());
        browser.inputs[0].state.pending_enabled = Some(true);
        assert!(
            browser
                .type_text("Enable is not yet committed".into())
                .is_err()
        );
        browser.inputs[0].state.commit();
        let text = " asymmetric 中文 😀 ";
        browser.type_text(text.into()).unwrap();
        dh.flush_clients().unwrap();
        let sent = events(&mut socket);
        assert_eq!(sent.len(), 2);
        // text-input-v3: commit_string opcode 3, done opcode 5.
        assert_eq!(sent[0].0, 3);
        let len = u32::from_ne_bytes(sent[0].1[..4].try_into().unwrap()) as usize;
        assert_eq!(&sent[0].1[4..4 + len - 1], text.as_bytes());
        assert_eq!(sent[1], (5, 1u32.to_ne_bytes().to_vec()));

        browser.inputs[0].state.pending_enabled = Some(false);
        browser.inputs[0].state.commit();
        assert!(browser.type_text("Disabled field".into()).is_err());
        dh.flush_clients().unwrap();
        assert!(events(&mut socket).is_empty());

        // A different entered surface invalidates the previous enabled state.
        browser.inputs[0].state.pending_enabled = Some(true);
        browser.inputs[0].state.commit();
        let second = client
            .create_resource::<WlSurface, _, TestState>(&dh, 1, ())
            .unwrap();
        browser.focus_changed(Some(&second));
        assert!(
            browser
                .type_text("Previous field must not receive this".into())
                .is_err()
        );

        // A different client's focused surface cannot use this client's field.
        browser.inputs[0].state.pending_enabled = Some(true);
        browser.inputs[0].state.commit();
        let (other_server, _other_socket) = std::os::unix::net::UnixStream::pair().unwrap();
        let other_client = dh
            .insert_client(other_server, Arc::new(TestClient))
            .unwrap();
        let other_surface = other_client
            .create_resource::<WlSurface, _, TestState>(&dh, 1, ())
            .unwrap();
        browser.focus_changed(Some(&other_surface));
        assert!(browser.type_text("Other client".into()).is_err());
        browser.focus_changed(None);
        assert!(browser.type_text("Focus lost".into()).is_err());
    }

    #[test]
    fn enable_and_disable_are_double_buffered_and_serials_include_inactive_commits() {
        let mut state = CommitState::default();
        state.commit();
        state.pending_enabled = Some(true);
        assert!(!state.enabled);
        assert_eq!(state.serial, 1);
        state.commit();
        assert!(state.enabled);
        assert_eq!(state.serial, 2);
        state.pending_enabled = Some(false);
        assert!(state.enabled);
        state.commit();
        assert!(!state.enabled);
        assert_eq!(state.serial, 3);
        state.serial = u32::MAX;
        state.commit();
        assert_eq!(state.serial, 0);
    }
}

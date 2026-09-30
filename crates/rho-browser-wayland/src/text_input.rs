//! The nested text-input-v3 endpoint. Rho is the input method: no nested
//! input-method process or synthetic key spelling is needed.
use super::*;
use smithay::reexports::wayland_protocols::wp::text_input::zv3::server::{
    zwp_text_input_manager_v3::{self, ZwpTextInputManagerV3},
    zwp_text_input_v3::{self, ZwpTextInputV3},
};
use smithay::reexports::wayland_server::{GlobalDispatch, New};

/// State supplied by Chromium for its focused editable field. Offsets are
/// UTF-8 bytes, as required by text-input-v3, not GPUI UTF-16 positions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextInputState {
    /// Changes whenever an editable field is enabled, disabled or loses focus.
    pub generation: u64,
    pub enabled: bool,
    pub surrounding: Option<(String, u32, u32)>,
    pub cursor_rectangle: Option<(i32, i32, i32, i32)>,
}

/// One atomic input-method update, applied only to its originating field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextInputUpdate {
    pub generation: u64,
    pub commit: Option<String>,
    /// Text and UTF-8 cursor positions within the composition.
    pub preedit: Option<(String, i32, i32)>,
    pub delete: Option<(u32, u32)>,
}

#[derive(Default)]
struct Pending {
    enabled: Option<bool>,
    surrounding: Option<(String, u32, u32)>,
    rectangle: Option<(i32, i32, i32, i32)>,
}
struct Instance {
    resource: ZwpTextInputV3,
    serial: u32,
    pending: Pending,
    current: TextInputState,
    preedit: (String, i32, i32),
}

#[derive(Default)]
pub(super) struct TextInputs {
    instances: Vec<Instance>,
    focus: Option<WlSurface>,
    generation: u64,
}
impl TextInputs {
    pub(super) fn focused_surface(&self) -> Option<&WlSurface> {
        self.focus.as_ref()
    }
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }
    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.generation
    }
    pub(super) fn focus(&mut self, surface: Option<WlSurface>) {
        if self.focus == surface {
            return;
        }
        let generation = self.next_generation();
        for entry in &mut self.instances {
            if let Some(old) = &self.focus
                && old.id().same_client_as(&entry.resource.id())
            {
                entry.resource.leave(old);
            }
            entry.current = TextInputState {
                generation,
                ..Default::default()
            };
            entry.pending = Pending::default();
            entry.preedit = Default::default();
            if let Some(new) = &surface
                && new.id().same_client_as(&entry.resource.id())
            {
                entry.resource.enter(new);
            }
        }
        self.focus = surface;
    }
    pub(super) fn update(&mut self, update: TextInputUpdate) {
        for entry in &mut self.instances {
            if !entry.current.enabled || entry.current.generation != update.generation {
                continue;
            }
            if let Some((before, after)) = update.delete {
                entry.resource.delete_surrounding_text(before, after);
            }
            if let Some(text) = &update.commit {
                entry.resource.commit_string(Some(text.clone()));
            }
            if let Some(preedit) = &update.preedit {
                entry.preedit = preedit.clone();
            }
            entry.resource.preedit_string(
                Some(entry.preedit.0.clone()),
                entry.preedit.1,
                entry.preedit.2,
            );
            entry.resource.done(entry.serial);
        }
    }
    pub(super) fn clear_preedit(&mut self) {
        for entry in &mut self.instances {
            if entry.current.enabled {
                entry.preedit = Default::default();
                entry.resource.preedit_string(Some(String::new()), 0, 0);
                entry.resource.done(entry.serial);
            }
        }
    }
}
impl<K: BrowserPageKey> State<K> {
    pub(super) fn publish_text_input(&self) {
        let focused = self
            .text_inputs
            .focus
            .as_ref()
            .and_then(|surface| self.window_id_for_surface(surface));
        if let Some(id) = focused {
            let current = self
                .text_inputs
                .instances
                .iter()
                .find(|entry| entry.current.enabled)
                .map(|entry| entry.current.clone())
                .unwrap_or(TextInputState {
                    generation: self.text_inputs.generation,
                    ..Default::default()
                });
            self.windows[&id]
                .events
                .send(BrowserEvent::TextInput(current));
        }
    }
}

impl<K: BrowserPageKey> GlobalDispatch<ZwpTextInputManagerV3, ()> for State<K> {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwpTextInputManagerV3>,
        _: &(),
        data: &mut DataInit<'_, Self>,
    ) {
        data.init(resource, ());
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
        data: &mut DataInit<'_, Self>,
    ) {
        if let zwp_text_input_manager_v3::Request::GetTextInput { id, seat } = request {
            if Seat::<State<K>>::from_resource(&seat).as_ref() != Some(&state._seat) {
                return;
            }
            let resource = data.init(id, ());
            if let Some(focus) = &state.text_inputs.focus
                && focus.id().same_client_as(&resource.id())
            {
                resource.enter(focus);
            }
            state.text_inputs.instances.push(Instance {
                resource,
                serial: 0,
                pending: Pending::default(),
                current: TextInputState::default(),
                preedit: Default::default(),
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
        let Some(index) = state
            .text_inputs
            .instances
            .iter()
            .position(|entry| entry.resource == *resource)
        else {
            return;
        };
        if matches!(request, zwp_text_input_v3::Request::Commit) {
            state.text_inputs.instances[index].serial =
                state.text_inputs.instances[index].serial.wrapping_add(1);
        }
        if !state
            .text_inputs
            .focus
            .as_ref()
            .is_some_and(|focus| focus.id().same_client_as(&resource.id()))
        {
            return;
        }
        let pending = &mut state.text_inputs.instances[index].pending;
        match request {
            zwp_text_input_v3::Request::Enable => pending.enabled = Some(true),
            zwp_text_input_v3::Request::Disable => pending.enabled = Some(false),
            zwp_text_input_v3::Request::SetSurroundingText {
                text,
                cursor,
                anchor,
            } => {
                if cursor >= 0
                    && anchor >= 0
                    && text.is_char_boundary(cursor as usize)
                    && text.is_char_boundary(anchor as usize)
                {
                    pending.surrounding = Some((text, cursor as u32, anchor as u32));
                }
            }
            zwp_text_input_v3::Request::SetCursorRectangle {
                x,
                y,
                width,
                height,
            } => {
                pending.rectangle = Some((x, y, width, height));
            }
            zwp_text_input_v3::Request::Commit => {
                let pending = std::mem::take(pending);
                let generation = pending.enabled.map(|_| state.text_inputs.next_generation());
                if pending.enabled == Some(true) {
                    for entry in &mut state.text_inputs.instances {
                        entry.current.enabled = false;
                    }
                }
                let entry = &mut state.text_inputs.instances[index];
                if let Some(enabled) = pending.enabled {
                    entry.preedit = Default::default();
                    entry.current = TextInputState {
                        enabled,
                        generation: generation.unwrap(),
                        ..Default::default()
                    };
                }
                if let Some(surrounding) = pending.surrounding {
                    entry.current.surrounding = Some(surrounding);
                }
                if let Some(rectangle) = pending.rectangle {
                    entry.current.cursor_rectangle = Some(rectangle);
                }
                // Chromium waits for its commit serial before sending its next
                // surrounding/caret update. Preserve composition in this ack.
                entry.resource.preedit_string(
                    Some(entry.preedit.0.clone()),
                    entry.preedit.1,
                    entry.preedit.2,
                );
                entry.resource.done(entry.serial);
                state.publish_text_input();
            }
            _ => {}
        }
    }
    fn destroyed(state: &mut Self, _: ClientId, resource: &ZwpTextInputV3, _: &()) {
        state
            .text_inputs
            .instances
            .retain(|entry| entry.resource != *resource);
        state.publish_text_input();
    }
}

//! Native remote application window. Closing it drops the video subscription.
use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::*;
use rho_desktop_proto::Input;
use theme::ActiveTheme as _;

/// A decoded image and the one frame the renderer draws it from: the
/// renderer knows a frame by its identity, so an image gets exactly one.
struct Shown {
    image: Arc<rho_desktop_client::viewer::Image>,
    #[cfg_attr(not(target_os = "linux"), expect(dead_code))]
    render: VideoFrame,
}

impl Shown {
    fn new(image: Arc<rho_desktop_client::viewer::Image>) -> anyhow::Result<Self> {
        let render = VideoFrame::new(Arc::new(Planes(image.clone())))?;
        Ok(Self { image, render })
    }
}

struct Planes(Arc<rho_desktop_client::viewer::Image>);

impl Yuv444Data for Planes {
    fn size(&self) -> (u32, u32) {
        (self.0.planes.width() as u32, self.0.planes.height() as u32)
    }
    fn plane(&self, index: usize) -> &[u8] {
        self.0.planes.plane(index)
    }
    fn stride(&self, index: usize) -> u32 {
        self.0.planes.stride(index) as u32
    }
}

/// Native IME composition is local until commit; the remote window remains
/// the only owner of its document and selection.
struct RemoteTextInput {
    input: tokio::sync::mpsc::Sender<Input>,
    enabled: bool,
    composition: String,
    selection: Range<usize>,
    modifiers: Modifiers,
}

enum TextInputEvent {
    Changed,
    Failed,
}

impl EventEmitter<TextInputEvent> for RemoteTextInput {}

impl RemoteTextInput {
    fn new(input: tokio::sync::mpsc::Sender<Input>) -> Self {
        Self {
            input,
            enabled: false,
            composition: String::new(),
            selection: 0..0,
            modifiers: Modifiers::default(),
        }
    }

    fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.enabled = enabled;
        if !enabled {
            self.composition.clear();
            self.selection = 0..0;
            self.modifiers = Modifiers::default();
        }
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }
}

impl EntityInputHandler for RemoteTextInput {
    fn accepts_text_input(&self, _: &mut Window, _: &mut Context<Self>) -> bool {
        self.enabled
    }

    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.composition.encode_utf16().collect::<Vec<_>>();
        let mut start = range.start.min(text.len());
        let mut end = range.end.max(start).min(text.len());
        // An IME may request a boundary inside a UTF-16 surrogate pair.
        if start < text.len() && (0xdc00..=0xdfff).contains(&text[start]) {
            start -= 1;
        }
        if end < text.len() && (0xdc00..=0xdfff).contains(&text[end]) {
            end += 1;
        }
        *adjusted = Some(start..end);
        Some(String::from_utf16_lossy(&text[start..end]))
    }

    fn selected_text_range(
        &mut self,
        ignore_disabled: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        (self.enabled || ignore_disabled).then(|| UTF16Selection {
            range: self.selection.clone(),
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        (!self.composition.is_empty()).then(|| 0..self.composition.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.composition.clear();
        self.selection = 0..0;
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled {
            return;
        }
        self.unmark_text(window, cx);
        let inputs = if self.modifiers.number_of_modifiers() == 0 {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![Input::Text(text.to_owned())]
            }
        } else {
            text.chars()
                .map(|character| remote_key(&character.to_string(), self.modifiers))
                .collect()
        };
        for input in inputs {
            if self.input.try_send(input).is_err() {
                cx.emit(TextInputEvent::Failed);
                break;
            }
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled {
            return;
        }
        self.composition = text.to_owned();
        let len = text.encode_utf16().count();
        self.selection = selected
            .map(|range| {
                let start = range.start.min(len);
                start..range.end.max(start).min(len)
            })
            .unwrap_or(len..len);
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(Bounds::new(bounds.bottom_left(), size(px(1.), px(1.))))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }

    fn text_length_utf16(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        Some(self.composition.encode_utf16().count())
    }
}

fn phone_layout(size: Size<Pixels>) -> bool {
    size.width <= px(600.) || (size.height <= px(600.) && size.width <= px(1000.))
}

const TOUCH_KEYS: &[(&str, &str)] = &[
    ("escape", "Esc"),
    ("tab", "Tab"),
    ("enter", "Enter"),
    ("backspace", "⌫"),
    ("delete", "Delete"),
    ("left", "←"),
    ("up", "↑"),
    ("down", "↓"),
    ("right", "→"),
    ("home", "Home"),
    ("end", "End"),
    ("pageup", "Page ↑"),
    ("pagedown", "Page ↓"),
    ("insert", "Insert"),
    ("f1", "F1"),
    ("f2", "F2"),
    ("f3", "F3"),
    ("f4", "F4"),
    ("f5", "F5"),
    ("f6", "F6"),
    ("f7", "F7"),
    ("f8", "F8"),
    ("f9", "F9"),
    ("f10", "F10"),
    ("f11", "F11"),
    ("f12", "F12"),
    ("c", "C"),
    ("v", "V"),
    ("a", "A"),
    ("z", "Z"),
];

fn remote_key(key: &str, modifiers: Modifiers) -> Input {
    let mut chord = String::new();
    for (active, prefix) in [
        (modifiers.control, "ctrl+"),
        (modifiers.alt, "alt+"),
        (modifiers.shift, "shift+"),
        (modifiers.platform, "super+"),
    ] {
        if active {
            chord.push_str(prefix);
        }
    }
    chord.push_str(key);
    Input::Key(chord)
}

pub struct WaylandView {
    viewer: rho_desktop_client::viewer::Viewer,
    image: Option<Rc<Shown>>,
    frozen: Option<Rc<Shown>>,
    strokes: Vec<Vec<(u32, u32)>>,
    drawing: bool,
    target: Option<Entity<rho_agents_view::AgentModel>>,
    status: Option<String>,
    size: (usize, usize),
    bounds: Rc<Cell<Bounds<Pixels>>>,
    focus: FocusHandle,
    text_input: Entity<RemoteTextInput>,
    show_keys: bool,
    drag_mode: bool,
    touch_contact: Option<TouchId>,
    _text_events: Subscription,
    error: Option<String>,
    first_paint: Rc<Cell<bool>>,
    painted: Rc<Cell<Option<rho_desktop_proto::FrameId>>>,
    _updates: Task<()>,
    _activation: Subscription,
}
impl WaylandView {
    pub fn new(
        viewer: rho_desktop_client::viewer::Viewer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let started = viewer.started_at;
        let desktop_id = viewer.desktop_id;
        tracing::info!(
            desktop_id,
            elapsed_ms = started.elapsed().as_millis(),
            "desktop viewer attached to GUI"
        );
        let mut images = viewer.images.clone();
        let mut errors = viewer.errors.clone();
        let updates = cx.spawn(async move |this, cx| {
            loop {
                let changed = {
                    let image = Box::pin(images.changed());
                    let error = Box::pin(errors.changed());
                    match futures::future::select(image, error).await {
                        futures::future::Either::Left((result, _)) => result.is_ok(),
                        futures::future::Either::Right((result, _)) => result.is_ok(),
                    }
                };
                if !changed {
                    break;
                }
                let image = images.borrow_and_update().clone();
                let error = errors.borrow_and_update().clone();
                if this
                    .update(cx, |this, cx| {
                        this.error = error;
                        if let Some(image) = image {
                            if this.image.is_none() {
                                tracing::info!(
                                    desktop_id,
                                    elapsed_ms = started.elapsed().as_millis(),
                                    "desktop first frame delivered to GUI"
                                );
                            }
                            match Shown::new(image) {
                                Ok(shown) => {
                                    if this.frozen.is_none() {
                                        this.size = (shown.image.width, shown.image.height);
                                    }
                                    this.image = Some(Rc::new(shown));
                                }
                                Err(error) => this.error = Some(format!("{error:#}")),
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let text_input = cx.new(|_| RemoteTextInput::new(viewer.input.clone()));
        let text_events = cx.subscribe(&text_input, |this, _, event, cx| {
            if matches!(event, TextInputEvent::Failed) {
                this.viewer.disconnect();
                this.error =
                    Some("Viewer disconnected because input could not be delivered".into());
            }
            cx.notify();
        });
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.finish_touch(true, cx);
                this.viewer.motion.send_replace(None);
                this.send(Input::ReleaseAll, cx);
                this.drawing = false;
            }
        });
        Self {
            first_paint: Rc::new(Cell::new(true)),
            painted: Rc::new(Cell::new(None)),
            _activation: activation,
            viewer,
            image: None,
            frozen: None,
            strokes: Vec::new(),
            drawing: false,
            target: None,
            status: None,
            size: (1, 1),
            bounds: Rc::new(Cell::new(Bounds::default())),
            focus,
            text_input,
            show_keys: false,
            drag_mode: false,
            touch_contact: None,
            _text_events: text_events,
            error: None,
            _updates: updates,
        }
    }
    pub fn with_target(mut self, target: Option<Entity<rho_agents_view::AgentModel>>) -> Self {
        self.target = target;
        self
    }
    pub(crate) fn toggle_annotation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        self.annotate(cx);
    }
    fn annotate(&mut self, cx: &mut Context<Self>) {
        self.finish_touch(true, cx);
        if self.frozen.is_some() {
            self.frozen = None;
            self.strokes.clear();
            self.drawing = false;
            if let Some(image) = self.viewer.images.borrow().as_ref() {
                self.size = (image.width, image.height);
            }
        } else {
            self.text_input
                .update(cx, |input, cx| input.set_enabled(false, cx));
            self.send(Input::ReleaseAll, cx);
            self.viewer.motion.send_replace(None);
            self.frozen = self.image.clone();
            self.status = None;
        }
        cx.notify();
    }
    fn export(&mut self, attach: bool, cx: &mut Context<Self>) {
        let Some(image) = self.frozen.as_ref() else {
            return;
        };
        let mut pixels = match image.image.export_bgra() {
            Ok(pixels) => pixels,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        for stroke in &self.strokes {
            if let Some(&point) = stroke.first() {
                draw_line(&mut pixels, self.size, point, point);
            }
            for pair in stroke.windows(2) {
                draw_line(&mut pixels, self.size, pair[0], pair[1]);
            }
        }
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        let mut png = std::io::Cursor::new(Vec::new());
        let image =
            image::RgbaImage::from_raw(self.size.0 as u32, self.size.1 as u32, pixels).unwrap();
        if let Err(error) = image.write_to(&mut png, image::ImageFormat::Png) {
            self.error = Some(error.to_string());
            cx.notify();
            return;
        }
        let bytes = png.into_inner();
        if attach {
            if let Some(target) = &self.target {
                target.update(cx, |model, cx| {
                    model.add_image("image/png".into(), bytes, cx)
                });
                self.status = Some("Added to agent prompt".into());
            }
        } else {
            let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes);
            cx.write_to_clipboard(ClipboardItem::new_image(&image));
            self.status = Some("Annotation copied".into());
        }
        cx.notify();
    }
    fn send(&mut self, input: Input, cx: &mut Context<Self>) {
        if self.viewer.input.try_send(input).is_err() {
            self.viewer.disconnect();
            self.error = Some("Viewer disconnected because input could not be delivered".into());
            cx.notify();
        }
    }
    /// Claim only drawing and explicit pointer dragging. Normal touch pan is
    /// left to GPUI's scroll translation; claimed contacts must not also click.
    fn touch(&mut self, event: &TouchEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.frozen.is_none() && !self.drag_mode && self.touch_contact.is_none() {
            return;
        }
        window.prevent_default();
        cx.stop_propagation();
        match event.phase {
            TouchPhase::Started if self.touch_contact.is_none() => {
                let Some(position) = self.position(event.position) else {
                    return;
                };
                self.touch_contact = Some(event.id);
                if self.frozen.is_some() {
                    self.strokes.push(vec![position]);
                } else {
                    self.viewer.motion.send_replace(None);
                    self.send(
                        Input::Move {
                            x: position.0,
                            y: position.1,
                        },
                        cx,
                    );
                    self.send(
                        Input::Button {
                            button: 0x110,
                            pressed: true,
                        },
                        cx,
                    );
                }
                cx.notify();
            }
            TouchPhase::Moved if self.touch_contact == Some(event.id) => {
                if let Some(position) = self.position(event.position) {
                    if self.frozen.is_some() {
                        let stroke = self.strokes.last_mut().unwrap();
                        if stroke.last() != Some(&position) {
                            stroke.push(position);
                        }
                    } else {
                        self.send(
                            Input::Move {
                                x: position.0,
                                y: position.1,
                            },
                            cx,
                        );
                    }
                    cx.notify();
                }
            }
            TouchPhase::Ended | TouchPhase::Cancelled if self.touch_contact == Some(event.id) => {
                if event.phase == TouchPhase::Ended
                    && let Some(position) = self.position(event.position)
                {
                    if self.frozen.is_some() {
                        let stroke = self.strokes.last_mut().unwrap();
                        if stroke.last() != Some(&position) {
                            stroke.push(position);
                        }
                    } else {
                        self.send(
                            Input::Move {
                                x: position.0,
                                y: position.1,
                            },
                            cx,
                        );
                    }
                }
                self.finish_touch(event.phase == TouchPhase::Cancelled, cx);
            }
            _ => {}
        }
    }

    fn finish_touch(&mut self, cancelled: bool, cx: &mut Context<Self>) {
        if self.touch_contact.take().is_none() {
            return;
        }
        if self.frozen.is_some() {
            if cancelled {
                self.strokes.pop();
            }
        } else {
            self.send(
                Input::Button {
                    button: 0x110,
                    pressed: false,
                },
                cx,
            );
        }
        cx.notify();
    }

    fn render_phone_controls(&self, viewport_height: Pixels, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let touch_modifiers = self.text_input.read(cx).modifiers;
        let button = |id: &'static str, label: &'static str, selected: bool| {
            div()
                .id(id)
                .min_h(px(48.))
                .min_w(px(72.))
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px_2()
                .border_1()
                .rounded_md()
                .border_color(if selected {
                    colors.text_accent
                } else {
                    colors.border
                })
                .text_color(if selected {
                    colors.text_accent
                } else {
                    colors.text
                })
                .cursor_pointer()
                .child(label)
        };
        let mut panel = div()
            .id("remote-phone-controls-content")
            .flex()
            .flex_col()
            .gap_2();
        let toolbar;
        if self.frozen.is_some() {
            let mut actions = div()
                .flex()
                .flex_wrap()
                .gap_2()
                .child(
                    button("remote-phone-undo", "Undo", false).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.strokes.pop();
                            cx.notify();
                        },
                    )),
                )
                .child(
                    button("remote-phone-copy", "Copy", false).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.export(false, cx);
                        },
                    )),
                )
                .child(
                    button("remote-phone-live", "Live", true).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.annotate(cx);
                        },
                    )),
                );
            if self.target.is_some() {
                actions = actions.child(
                    button("remote-phone-attach", "Attach", false)
                        .on_click(cx.listener(|this, _, _, cx| this.export(true, cx))),
                );
            }
            toolbar = actions.into_any_element();
            if let Some(status) = &self.status {
                panel = panel.child(div().text_color(colors.text_muted).child(status.clone()));
            }
        } else {
            let enabled = self.text_input.read(cx).enabled;
            if self.show_keys {
                let mut modifiers = div().flex().flex_wrap().gap_2();
                for (index, label, selected) in [
                    (0, "Ctrl", touch_modifiers.control),
                    (1, "Alt", touch_modifiers.alt),
                    (2, "Shift", touch_modifiers.shift),
                    (3, "Super", touch_modifiers.platform),
                ] {
                    modifiers = modifiers.child(
                        button(
                            ["remote-ctrl", "remote-alt", "remote-shift", "remote-super"][index],
                            label,
                            selected,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.text_input.update(cx, |input, cx| {
                                let flag = match index {
                                    0 => &mut input.modifiers.control,
                                    1 => &mut input.modifiers.alt,
                                    2 => &mut input.modifiers.shift,
                                    _ => &mut input.modifiers.platform,
                                };
                                *flag = !*flag;
                                cx.emit(TextInputEvent::Changed);
                            });
                            cx.notify();
                        })),
                    );
                }
                let mut keys = div().flex().flex_wrap().gap_2();
                for (index, &(key, label)) in TOUCH_KEYS.iter().enumerate() {
                    keys = keys.child(
                        div()
                            .id(("remote-phone-key", index as usize))
                            .min_h(px(48.))
                            .min_w(px(56.))
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .border_1()
                            .border_color(colors.border)
                            .rounded_md()
                            .cursor_pointer()
                            .child(label)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let modifiers = this.text_input.read(cx).modifiers;
                                this.send(remote_key(key, modifiers), cx);
                                window.focus(&this.focus, cx);
                            })),
                    );
                }
                panel = panel
                    .child(modifiers)
                    .child(
                        div()
                            .text_color(colors.text_muted)
                            .child("Modifiers apply to typing and these keys"),
                    )
                    .child(keys);
            }
            if touch_modifiers.number_of_modifiers() > 0 {
                let active = [
                    (touch_modifiers.control, "Ctrl"),
                    (touch_modifiers.alt, "Alt"),
                    (touch_modifiers.shift, "Shift"),
                    (touch_modifiers.platform, "Super"),
                ]
                .into_iter()
                .filter_map(|(active, label)| active.then_some(label))
                .collect::<Vec<_>>()
                .join(" + ");
                panel = panel.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_color(colors.text_accent)
                                .child(format!("Active: {active}")),
                        )
                        .child(
                            button("remote-phone-clear-modifiers", "Clear modifiers", false)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.text_input.update(cx, |input, cx| {
                                        input.modifiers = Modifiers::default();
                                        cx.emit(TextInputEvent::Changed);
                                    });
                                    cx.notify();
                                })),
                        ),
                );
            }
            toolbar = div()
                .flex()
                .flex_wrap()
                .gap_2()
                .child(
                    button("remote-phone-keyboard", "Keyboard", enabled).on_click(cx.listener(
                        |this, _, window, cx| {
                            this.text_input
                                .update(cx, |input, cx| input.set_enabled(!input.enabled, cx));
                            window.focus(&this.focus, cx);
                            cx.notify();
                        },
                    )),
                )
                .child(
                    button("remote-phone-keys", "Keys", self.show_keys).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.show_keys = !this.show_keys;
                            cx.notify();
                        },
                    )),
                )
                .child(
                    button("remote-phone-drag", "Drag", self.drag_mode).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.finish_touch(true, cx);
                            this.drag_mode = !this.drag_mode;
                            cx.notify();
                        },
                    )),
                )
                .into_any_element();
            let composition = &self.text_input.read(cx).composition;
            if !composition.is_empty() {
                panel = panel.child(
                    div()
                        .text_color(colors.text_muted)
                        .child(composition.clone()),
                );
            }
        }
        let has_details = if self.frozen.is_some() {
            self.status.is_some()
        } else {
            self.show_keys
                || touch_modifiers.number_of_modifiers() > 0
                || !self.text_input.read(cx).composition.is_empty()
        };
        div()
            .id("remote-phone-controls")
            .flex()
            .flex_col()
            .flex_shrink_0()
            .max_h((viewport_height / 2.).max(px(48.)))
            .px_2()
            .when(viewport_height > px(300.), |root| root.py_2())
            .bg(colors.editor_background)
            .when(has_details, |root| {
                root.child(
                    div()
                        .id("remote-phone-controls-scroll")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(panel),
                )
            })
            .child(
                div()
                    .id("remote-phone-toolbar")
                    .debug_selector(|| "remote-phone-toolbar".into())
                    .min_h(px(48.))
                    .flex_shrink_0()
                    .child(toolbar),
            )
            .into_any_element()
    }

    fn position(&self, p: Point<Pixels>) -> Option<(u32, u32)> {
        // Before the first frame, the placeholder dimensions are not coordinates.
        self.image.as_ref()?;
        let bounds = self.bounds.get();
        if !bounds.contains(&p) || bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
            return None;
        }
        Some((
            (((p.x - bounds.origin.x) / bounds.size.width) * self.size.0 as f32) as u32,
            (((p.y - bounds.origin.y) / bounds.size.height) * self.size.1 as f32) as u32,
        ))
        .map(|(x, y)| (x.min(self.size.0 as u32 - 1), y.min(self.size.1 as u32 - 1)))
    }
}
impl Render for WaylandView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let phone = phone_layout(window.viewport_size());
        if !phone && self.text_input.read(cx).enabled {
            self.text_input
                .update(cx, |input, cx| input.set_enabled(false, cx));
        }
        let text_input = phone.then(|| (self.focus.clone(), self.text_input.clone()));
        let image = self.frozen.clone().or_else(|| self.image.clone());
        let first_paint = self.first_paint.clone();
        let painted = self.painted.clone();
        let started = self.viewer.started_at;
        let desktop_id = self.viewer.desktop_id;
        let strokes = self.strokes.clone();
        let size = self.size;
        let bounds = self.bounds.clone();
        let canvas = canvas(
            move |available, _, _| {
                let ratio = (f32::from(available.size.width) / size.0 as f32)
                    .min(f32::from(available.size.height) / size.1 as f32);
                let size = gpui::size(px(size.0 as f32 * ratio), px(size.1 as f32 * ratio));
                let fitted = Bounds::new(
                    available.origin
                        + point(
                            (available.size.width - size.width) / 2.,
                            (available.size.height - size.height) / 2.,
                        ),
                    size,
                );
                bounds.set(fitted);
                fitted
            },
            move |_, bounds, window, cx| {
                if let Some((focus, input)) = text_input {
                    window.handle_input(&focus, ElementInputHandler::new(bounds, input), cx);
                }
                if let Some(image) = image {
                    #[cfg(target_os = "linux")]
                    window.paint_video(bounds, image.render.clone());
                    let presented = image.image.clone();
                    on_presented(image.image.id, &painted, window, move || {
                        presented.presented()
                    });
                    if first_paint.replace(false) {
                        tracing::info!(
                            desktop_id,
                            elapsed_ms = started.elapsed().as_millis(),
                            "desktop first frame paint queued"
                        );
                        // A paint callback only queues primitives. This next-frame
                        // marker also exposes stalls in rendering/presentation.
                        window.on_next_frame(move |_, _| {
                            tracing::info!(
                                desktop_id,
                                elapsed_ms = started.elapsed().as_millis(),
                                "desktop frame callback after first paint"
                            );
                        });
                    }
                }
                let map = |(x, y): (u32, u32)| {
                    bounds.origin
                        + point(
                            bounds.size.width * (x as f32 / size.0 as f32),
                            bounds.size.height * (y as f32 / size.1 as f32),
                        )
                };
                for stroke in strokes {
                    let Some(&first) = stroke.first() else {
                        continue;
                    };
                    let mut path =
                        PathBuilder::stroke(px(4.) * f32::from(bounds.size.width) / size.0 as f32);
                    path.move_to(map(first));
                    if stroke.len() == 1 {
                        let radius = bounds.size.width * (2. / size.0 as f32);
                        let dot = Bounds::new(
                            map(first) - point(radius, radius),
                            gpui::size(radius * 2., radius * 2.),
                        );
                        window.paint_quad(fill(dot, rgb(0xff4050)));
                    }
                    for p in stroke.into_iter().skip(1) {
                        path.line_to(map(p));
                    }
                    if let Ok(path) = path.build() {
                        window.paint_path(path, rgb(0xff4050));
                    }
                }
            },
        )
        .size_full();
        let mut surface = div()
            .id("remote-wayland")
            .track_focus(&self.focus)
            .flex_1()
            .min_h_0()
            .relative()
            .bg(rgb(0x161616))
            .on_touch(cx.listener(Self::touch))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if let Some(position) = this.position(event.position) {
                    if this.frozen.is_some() {
                        if this.drawing {
                            if let Some(stroke) = this.strokes.last_mut() {
                                if stroke.last() != Some(&position) {
                                    stroke.push(position);
                                    cx.notify();
                                }
                            }
                        }
                    } else {
                        this.viewer.motion.send_replace(Some(position));
                    }
                }
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                if this.frozen.is_some() {
                    return;
                }
                let (horizontal, vertical) = match event.delta {
                    ScrollDelta::Pixels(p) => (-f32::from(p.x) as f64, -f32::from(p.y) as f64),
                    ScrollDelta::Lines(p) => (-p.x as f64 * 15., -p.y as f64 * 15.),
                };
                this.send(
                    Input::Scroll {
                        horizontal,
                        vertical,
                    },
                    cx,
                );
                cx.stop_propagation();
            }))
            .on_physical_key(cx.listener(|this, event: &PhysicalKeyEvent, _, cx| {
                if this.frozen.is_some() {
                    return;
                }
                let PhysicalKey::LinuxEvdev(code) = event.key;
                this.send(
                    Input::Physical {
                        code,
                        pressed: event.pressed,
                    },
                    cx,
                );
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if this.frozen.is_some() {
                    let key = &event.keystroke;
                    match key.key.as_str() {
                        "escape" => this.annotate(cx),
                        "u" if !key.modifiers.control && !key.modifiers.alt => {
                            this.strokes.pop();
                            cx.notify();
                        }
                        "y" if !key.modifiers.control && !key.modifiers.alt => {
                            this.export(false, cx)
                        }
                        "enter" => this.export(true, cx),
                        "g" if key.modifiers.control => this.annotate(cx),
                        _ => {}
                    }
                    cx.stop_propagation();
                    return;
                }
                if cfg!(target_os = "linux") {
                    cx.stop_propagation();
                    return;
                }
                let key = &event.keystroke;
                if !key.modifiers.control && !key.modifiers.platform && !key.modifiers.alt {
                    if let Some(text) = &key.key_char {
                        this.send(Input::Text(text.clone()), cx);
                        cx.stop_propagation();
                        return;
                    }
                }
                this.send(remote_key(&key.key, key.modifiers), cx);
                cx.stop_propagation();
            }))
            .child(canvas);
        for (button, code) in [
            (MouseButton::Left, 0x110),
            (MouseButton::Right, 0x111),
            (MouseButton::Middle, 0x112),
        ] {
            surface = surface
                .on_mouse_down(
                    button,
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        window.focus(&this.focus, cx);
                        if let Some((x, y)) = this.position(event.position) {
                            if this.frozen.is_some() {
                                if button == MouseButton::Left {
                                    this.strokes.push(vec![(x, y)]);
                                    this.drawing = true;
                                    cx.notify();
                                }
                                return;
                            }
                            this.viewer.motion.send_replace(None);
                            this.send(Input::Move { x, y }, cx);
                            this.send(
                                Input::Button {
                                    button: code,
                                    pressed: true,
                                },
                                cx,
                            );
                        }
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_up(
                    button,
                    cx.listener(move |this, _: &MouseUpEvent, _, cx| {
                        this.drawing = false;
                        if this.frozen.is_none() {
                            this.send(
                                Input::Button {
                                    button: code,
                                    pressed: false,
                                },
                                cx,
                            );
                        }
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_up_out(
                    button,
                    cx.listener(move |this, _, _, cx| {
                        this.drawing = false;
                        if this.frozen.is_none() {
                            this.send(
                                Input::Button {
                                    button: code,
                                    pressed: false,
                                },
                                cx,
                            );
                        }
                    }),
                );
        }
        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .relative()
            .child(surface);
        if phone {
            root = root.child(self.render_phone_controls(window.viewport_size().height, cx));
        } else if self.frozen.is_some() {
            let action =
                |id: &'static str, label: &'static str| div().id(id).cursor_pointer().child(label);
            let mut mode_line = div()
                .flex()
                .items_center()
                .gap_3()
                .px_2()
                .py(px(3.))
                .text_size(px(12.))
                .bg(cx.theme().colors().editor_background)
                .text_color(cx.theme().colors().text_muted)
                .child(
                    div()
                        .text_color(cx.theme().colors().text_accent)
                        .child("DRAW"),
                )
                .child(
                    action("undo", "u undo").on_click(cx.listener(|this, _, _, cx| {
                        this.strokes.pop();
                        cx.notify();
                    })),
                )
                .child(
                    action("copy", "y copy")
                        .on_click(cx.listener(|this, _, _, cx| this.export(false, cx))),
                );
            if self.target.is_some() {
                mode_line = mode_line.child(
                    action("attach", "↵ attach")
                        .on_click(cx.listener(|this, _, _, cx| this.export(true, cx))),
                );
            }
            mode_line = mode_line.child(
                action("resume", "esc live")
                    .on_click(cx.listener(|this, _, _, cx| this.annotate(cx))),
            );
            if let Some(status) = &self.status {
                mode_line = mode_line.child(div().child(status.clone()));
            }
            root = root.child(mode_line);
        }
        if let Some(error) = &self.error {
            root = root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .p_2()
                    .bg(rgb(0x401818))
                    .text_color(rgb(0xffffff))
                    .child(error.clone()),
            );
        }
        root
    }
}

/// A repeated paint (including annotation on a frozen frame) must not refresh
/// the source's progress clock. Report only after the following GUI frame.
fn on_presented(
    id: rho_desktop_proto::FrameId,
    painted: &Cell<Option<rho_desktop_proto::FrameId>>,
    window: &Window,
    report: impl FnOnce() + 'static,
) {
    if painted.get().is_none_or(|previous| id > previous) {
        painted.set(Some(id));
        // Renderer progress, not a hardware scanout fence.
        window.on_next_frame(move |_, _| report());
    }
}

/// Burn the same output-pixel strokes into the frozen frame, not a later frame.
fn draw_line(pixels: &mut [u8], size: (usize, usize), from: (u32, u32), to: (u32, u32)) {
    let (mut x, mut y) = (from.0 as i32, from.1 as i32);
    let (end_x, end_y) = (to.0 as i32, to.1 as i32);
    let dx = (end_x - x).abs();
    let dy = -(end_y - y).abs();
    let sx = if x < end_x { 1 } else { -1 };
    let sy = if y < end_y { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        for oy in -2..=2 {
            for ox in -2..=2 {
                if ox * ox + oy * oy > 4 {
                    continue;
                }
                let (px, py) = (x + ox, y + oy);
                if px >= 0 && py >= 0 && (px as usize) < size.0 && (py as usize) < size.1 {
                    let i = (py as usize * size.0 + px as usize) * 4;
                    pixels[i..i + 4].copy_from_slice(&[0x50, 0x40, 0xff, 255]);
                }
            }
        }
        if x == end_x && y == end_y {
            break;
        }
        let twice = 2 * error;
        if twice >= dy {
            error += dy;
            x += sx;
        }
        if twice <= dx {
            error += dx;
            y += sy;
        }
    }
}
#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::{AppContext as _, EmptyView, TestAppContext};

    use super::{draw_line, on_presented};

    #[gpui::test]
    fn native_ime_sends_only_committed_text_and_respects_toggle(cx: &mut TestAppContext) {
        use gpui::{
            AppContext as _, ElementInputHandler, EntityInputHandler as _, PlatformInputHandler, px,
        };
        use rho_desktop_proto::Input;
        let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
        let input = cx.new(|_| super::RemoteTextInput::new(sender));
        let window = cx.add_window(|_, _| EmptyView);
        let mut handler = window
            .update(cx, |_, window, cx| {
                input.update(cx, |input, cx| {
                    assert!(!input.accepts_text_input(window, cx))
                });
                PlatformInputHandler::new(
                    window.to_async(cx),
                    Box::new(ElementInputHandler::new(
                        gpui::Bounds::new(
                            gpui::point(px(0.), px(0.)),
                            gpui::size(px(360.), px(480.)),
                        ),
                        input.clone(),
                    )),
                )
            })
            .unwrap();
        handler.replace_text_in_range(None, "disabled");
        assert!(receiver.try_recv().is_err());
        window
            .update(cx, |_, window, cx| {
                input.update(cx, |input, cx| {
                    input.set_enabled(true, cx);
                    assert!(input.accepts_text_input(window, cx));
                });
            })
            .unwrap();
        handler.replace_and_mark_text_in_range(None, "🦀に", Some(2..3));
        assert_eq!(handler.marked_text_range(), Some(0..3));
        let mut actual = None;
        assert_eq!(handler.text_for_range(1..2, &mut actual), Some("🦀".into()));
        assert_eq!(
            actual,
            Some(0..2),
            "IME boundaries must not split surrogate pairs"
        );
        assert!(
            receiver.try_recv().is_err(),
            "preedit is not a remote document edit"
        );
        handler.replace_text_in_range(None, "🦀日本語");
        assert!(matches!(receiver.try_recv().unwrap(), Input::Text(text) if text == "🦀日本語"));
        assert!(
            receiver.try_recv().is_err(),
            "one native commit must not also send key events"
        );
        assert_eq!(handler.marked_text_range(), None);
        window
            .update(cx, |_, _, cx| {
                input.update(cx, |input, _| input.modifiers.control = true);
            })
            .unwrap();
        handler.replace_text_in_range(None, "ax");
        assert!(matches!(receiver.try_recv().unwrap(), Input::Key(key) if key == "ctrl+a"));
        assert!(matches!(receiver.try_recv().unwrap(), Input::Key(key) if key == "ctrl+x"));
        assert!(receiver.try_recv().is_err());
        handler.replace_and_mark_text_in_range(None, "cancel", None);
        window
            .update(cx, |_, window, cx| {
                input.update(cx, |input, cx| {
                    input.set_enabled(false, cx);
                    assert!(!input.accepts_text_input(window, cx));
                    assert_eq!(input.modifiers, gpui::Modifiers::default());
                });
            })
            .unwrap();
        assert_eq!(handler.marked_text_range(), None);
        handler.replace_text_in_range(None, "still disabled");
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn remote_phone_layout_keeps_rotation_and_touch_chords() {
        use gpui::{Modifiers, px, size};
        use rho_desktop_proto::Input;
        for (width, height, expected) in [
            (360., 720., true),
            (720., 360., true),
            (600., 900., true),
            (601., 900., false),
            (1000., 600., true),
            (1001., 600., false),
            (1000., 601., false),
            (1280., 832., false),
        ] {
            assert_eq!(
                super::phone_layout(size(px(width), px(height))),
                expected,
                "{width}×{height}"
            );
        }
        assert!(
            matches!(super::remote_key("escape", Modifiers::default()), Input::Key(key) if key == "escape")
        );
        for key in [
            "home", "end", "pageup", "pagedown", "insert", "f1", "f2", "f3", "f4", "f5", "f6",
            "f7", "f8", "f9", "f10", "f11", "f12",
        ] {
            assert!(
                super::TOUCH_KEYS.iter().any(|(offered, _)| *offered == key),
                "{key} must be reachable from touch"
            );
        }
        assert!(matches!(
            super::remote_key("left", Modifiers { control: true, shift: true, ..Default::default() }),
            Input::Key(key) if key == "ctrl+shift+left"
        ));
        assert!(matches!(
            super::remote_key("tab", Modifiers { alt: true, platform: true, ..Default::default() }),
            Input::Key(key) if key == "alt+super+tab"
        ));
    }

    #[gpui::test]
    fn raw_touch_draws_and_drags_once_while_normal_pan_scrolls(cx: &mut TestAppContext) {
        use gpui::{InputEvent as _, TouchEvent, TouchId, TouchPhase, point, px, size};
        use rho_desktop_proto::Input;
        cx.update(crate::tests::init_test_app);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let (viewer, mut receiver, images) = rho_desktop_client::viewer::Viewer::test().unwrap();
        let window = cx.add_window(|window, cx| super::WaylandView::new(viewer, window, cx));
        images.send_modify(|_| {});
        cx.run_until_parked();
        cx.simulate_window_resize(*window, size(px(360.), px(720.)));
        cx.update_window(*window, |_, window, cx| {
            window.simulate_next_frame(cx);
        })
        .unwrap();
        let bounds = window
            .update(cx, |view, _, _| {
                assert!(view.image.is_some());
                view.drag_mode = true;
                view.bounds.get()
            })
            .unwrap();
        let start = bounds.origin + point(bounds.size.width / 4., bounds.size.height / 4.);
        let moved = bounds.origin + point(bounds.size.width * 0.75, bounds.size.height / 2.);
        let final_point =
            bounds.origin + point(bounds.size.width * 0.625, bounds.size.height * 0.75);
        let outside = point(bounds.right() + px(30.), bounds.bottom() + px(30.));
        let dispatch = |cx: &mut TestAppContext, id, phase, position, milliseconds| {
            cx.update_window(*window, |_, window, cx| {
                window.dispatch_event(
                    TouchEvent {
                        id: TouchId(id),
                        phase,
                        position,
                        force: None,
                        timestamp: std::time::Duration::from_millis(milliseconds),
                        serial: None,
                    }
                    .to_platform_input(),
                    cx,
                );
            })
            .unwrap();
        };
        let drain = |receiver: &mut tokio::sync::mpsc::Receiver<Input>| {
            std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>()
        };

        dispatch(cx, 1, TouchPhase::Started, start, 0);
        dispatch(cx, 1, TouchPhase::Moved, moved, 40);
        dispatch(cx, 1, TouchPhase::Ended, outside, 80);
        assert_eq!(
            drain(&mut receiver),
            vec![
                Input::Move { x: 8, y: 6 },
                Input::Button {
                    button: 0x110,
                    pressed: true
                },
                Input::Move { x: 24, y: 12 },
                Input::Button {
                    button: 0x110,
                    pressed: false
                },
            ],
            "drag must release outside bounds and must not also synthesize a click/scroll"
        );

        window.update(cx, |view, _, cx| view.annotate(cx)).unwrap();
        assert_eq!(drain(&mut receiver), vec![Input::ReleaseAll]);
        dispatch(cx, 2, TouchPhase::Started, start, 100);
        dispatch(cx, 2, TouchPhase::Moved, moved, 140);
        dispatch(cx, 2, TouchPhase::Ended, final_point, 180);
        window
            .update(cx, |view, _, _| {
                assert_eq!(view.strokes, vec![vec![(8, 6), (24, 12), (20, 18)]]);
                assert!(view.touch_contact.is_none());
            })
            .unwrap();
        assert!(
            drain(&mut receiver).is_empty(),
            "drawing cannot also send remote input"
        );
        dispatch(cx, 3, TouchPhase::Started, start, 200);
        dispatch(cx, 3, TouchPhase::Moved, moved, 240);
        dispatch(cx, 3, TouchPhase::Cancelled, outside, 280);
        window
            .update(cx, |view, _, _| assert_eq!(view.strokes.len(), 1))
            .unwrap();

        window.update(cx, |view, _, cx| view.annotate(cx)).unwrap();
        dispatch(cx, 4, TouchPhase::Started, start, 300);
        dispatch(cx, 5, TouchPhase::Started, moved, 320);
        dispatch(cx, 5, TouchPhase::Ended, moved, 340);
        window
            .update(cx, |view, _, _| {
                assert_eq!(view.touch_contact, Some(TouchId(4)))
            })
            .unwrap();
        dispatch(cx, 4, TouchPhase::Cancelled, outside, 360);
        assert_eq!(
            drain(&mut receiver),
            vec![
                Input::Move { x: 8, y: 6 },
                Input::Button {
                    button: 0x110,
                    pressed: true
                },
                Input::Button {
                    button: 0x110,
                    pressed: false
                },
            ],
            "other contacts must neither steal nor release the active drag"
        );
        window
            .update(cx, |view, _, _| view.drag_mode = false)
            .unwrap();
        dispatch(cx, 6, TouchPhase::Started, start, 400);
        dispatch(cx, 6, TouchPhase::Moved, moved, 440);
        dispatch(cx, 6, TouchPhase::Ended, moved, 480);
        let scrolling = drain(&mut receiver);
        assert!(!scrolling.is_empty());
        assert!(
            scrolling
                .iter()
                .all(|input| matches!(input, Input::Scroll { .. })),
            "ordinary pan must remain remote scroll, not pointer drag: {scrolling:?}"
        );
        // Expanded controls must keep their collapse/keyboard row visible
        // while the key details scroll, including the 240px IME reservation.
        for (width, height) in [(360., 480.), (720., 120.)] {
            window
                .update(cx, |view, _, _| view.show_keys = true)
                .unwrap();
            cx.simulate_window_resize(*window, size(px(width), px(height)));
            cx.update_window(*window, |_, window, cx| {
                window.simulate_next_frame(cx);
            })
            .unwrap();
            let mut visual = gpui::VisualTestContext::from_window(*window, cx);
            let toolbar = visual
                .debug_bounds("remote-phone-toolbar")
                .expect("toolbar rendered");
            assert!(
                toolbar.size.height >= px(48.),
                "touch row cannot shrink: {toolbar:?}"
            );
            assert!(
                toolbar.top() >= px(0.) && toolbar.bottom() <= px(height),
                "expanded keys cannot hide the collapse row at {width}×{height}: {toolbar:?}"
            );
        }
    }

    #[gpui::test]
    fn presentation_feedback_waits_for_frame_and_ignores_repaints(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| EmptyView);
        let painted = Cell::new(None);
        let reported = Rc::new(Cell::new(0));
        window
            .update(cx, |_, window, _| {
                for (epoch, timestamp_us) in [(2, 50), (2, 50), (1, 90), (2, 60)] {
                    let reported = reported.clone();
                    on_presented(
                        rho_desktop_proto::FrameId {
                            epoch,
                            timestamp_us,
                        },
                        &painted,
                        window,
                        move || reported.set(reported.get() + 1),
                    );
                }
                assert_eq!(reported.get(), 0, "paint is not presentation");
            })
            .unwrap();
        window
            .update(cx, |_, window, cx| {
                assert_eq!(window.simulate_next_frame(cx), 2);
            })
            .unwrap();
        assert_eq!(reported.get(), 2, "one callback per advancing frame");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn planar_video_gpu_rendering() -> anyhow::Result<()> {
        gpui_wgpu::video_tests::planar_video_renders_color_stride_and_clipping()
    }
    #[test]
    fn stroke_keeps_channels_and_clips_at_image_edges() {
        let mut pixels = vec![7; 13 * 9 * 4];
        draw_line(&mut pixels, (13, 9), (0, 1), (11, 7));
        assert_eq!(&pixels[4 * 13..4 * 13 + 4], &[0x50, 0x40, 0xff, 255]);
        assert_eq!(
            &pixels[(7 * 13 + 11) * 4..(7 * 13 + 12) * 4],
            &[0x50, 0x40, 0xff, 255]
        );
        assert_eq!(&pixels[(8 * 13) * 4..(8 * 13 + 1) * 4], &[7; 4]);
    }
}

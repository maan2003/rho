//! Reusable in-canvas touch keyboard for GPUI web clients.

#![cfg(target_family = "wasm")]

use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    App, Bounds, Context, Div, Hsla, Keystroke, MouseButton, MouseDownEvent, Pixels, Point, Render,
    Stateful, Task, WeakEntity, Window, div, point, px,
};
use serde::{Deserialize, Serialize};

const HEIGHT: f32 = 310.;
const STRIP_HEIGHT: f32 = 34.;
const ROW_HEIGHT: f32 = 41.;
const GAP: f32 = 3.;
const STORAGE_KEY: &str = "rho-touch-keyboard-telemetry-v1";
const TELEMETRY_CAPACITY: usize = 2048;

#[derive(Clone)]
pub struct ContextChip {
    pub label: String,
    pub on_select: Rc<dyn Fn(&mut Window, &mut App)>,
}

pub trait KeyboardPlugin {
    fn context_chips(&self) -> Vec<ContextChip>;
    fn style(&self, cx: &App) -> KeyboardStyle;
}

#[derive(Clone, Copy)]
pub struct KeyboardStyle {
    pub background: Hsla,
    pub key_background: Hsla,
    pub key_pressed: Hsla,
    pub border: Hsla,
    pub key_border: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_disabled: Hsla,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Shift {
    #[default]
    Off,
    Once,
    Locked,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TelemetryEntry {
    v: u8,
    key: String,
    dx: f32,
    dy: f32,
    dt_ms: u32,
    backspace_after_key: bool,
}

pub struct TouchKeyboard {
    plugin: Box<dyn KeyboardPlugin>,
    shift: Shift,
    last_shift_ms: f64,
    last_tap_ms: f64,
    last_key: Option<String>,
    telemetry: Vec<TelemetryEntry>,
    repeat_generation: u64,
    _repeat: Option<Task<()>>,
}

#[derive(Clone, Copy)]
enum Key {
    Text(&'static str),
    Shift,
    Backspace,
    Enter,
    Space,
    Dismiss,
    Reserved,
}

impl TouchKeyboard {
    pub fn new(plugin: impl KeyboardPlugin + 'static) -> Self {
        Self {
            plugin: Box::new(plugin),
            shift: Shift::Off,
            last_shift_ms: 0.,
            last_tap_ms: 0.,
            last_key: None,
            telemetry: load_telemetry(),
            repeat_generation: 0,
            _repeat: None,
        }
    }

    pub fn height() -> Pixels {
        px(HEIGHT)
    }

    pub fn region(viewport: gpui::Size<Pixels>) -> Bounds<Pixels> {
        Bounds::new(
            point(px(0.), viewport.height - Self::height()),
            gpui::size(viewport.width, Self::height()),
        )
    }

    fn commit(
        &mut self,
        key: Key,
        center: Point<Pixels>,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<TouchKeyboard>,
    ) {
        let now = js_sys::Date::now();
        match key {
            Key::Shift => {
                if now - self.last_shift_ms < 350. {
                    self.shift = Shift::Locked;
                } else {
                    self.shift = match self.shift {
                        Shift::Off => Shift::Once,
                        Shift::Once | Shift::Locked => Shift::Off,
                    };
                }
                self.last_shift_ms = now;
                self.record(
                    "shift".into(),
                    f32::from(event.position.x - center.x),
                    f32::from(event.position.y - center.y),
                    now,
                    false,
                );
                self.last_key = Some("shift".into());
                cx.notify();
                return;
            }
            Key::Dismiss => {
                self.record(
                    "dismiss".into(),
                    f32::from(event.position.x - center.x),
                    f32::from(event.position.y - center.y),
                    now,
                    false,
                );
                self.last_key = Some("dismiss".into());
                window.blur();
                cx.notify();
                return;
            }
            Key::Reserved => {
                self.record(
                    "reserved".into(),
                    f32::from(event.position.x - center.x),
                    f32::from(event.position.y - center.y),
                    now,
                    false,
                );
                self.last_key = Some("reserved".into());
                return;
            }
            _ => {}
        }

        let (stroke, telemetry_key) = match key {
            Key::Text(text) => {
                let text = if self.shift != Shift::Off {
                    text.to_uppercase()
                } else {
                    text.into()
                };
                (text.clone(), text)
            }
            Key::Backspace => ("backspace".into(), "backspace".into()),
            Key::Enter => ("enter".into(), "enter".into()),
            Key::Space => ("space".into(), "space".into()),
            _ => unreachable!(),
        };
        if let Ok(keystroke) = Keystroke::parse(&stroke) {
            window.dispatch_keystroke(keystroke, cx);
        }
        self.record(
            telemetry_key.clone(),
            f32::from(event.position.x - center.x),
            f32::from(event.position.y - center.y),
            now,
            matches!(key, Key::Backspace) && self.last_key.as_deref() != Some("backspace"),
        );
        self.last_key = Some(telemetry_key);
        if matches!(key, Key::Text(_)) && self.shift == Shift::Once {
            self.shift = Shift::Off;
            cx.notify();
        }
    }

    fn record(&mut self, key: String, dx: f32, dy: f32, now: f64, backspace_after_key: bool) {
        self.telemetry.push(TelemetryEntry {
            v: 1,
            key,
            dx,
            dy,
            dt_ms: if self.last_tap_ms == 0. {
                0
            } else {
                (now - self.last_tap_ms).clamp(0., u32::MAX as f64) as u32
            },
            backspace_after_key,
        });
        self.last_tap_ms = now;
        if self.telemetry.len() > TELEMETRY_CAPACITY {
            self.telemetry
                .drain(..self.telemetry.len() - TELEMETRY_CAPACITY);
        }
        if let Some(storage) = storage()
            && let Ok(json) = serde_json::to_string(&self.telemetry)
        {
            let _ = storage.set_item(STORAGE_KEY, &json);
        }
    }

    fn stop_repeat(&mut self) {
        self.repeat_generation = self.repeat_generation.wrapping_add(1);
        self._repeat.take();
    }

    fn start_repeat(
        &mut self,
        owner: WeakEntity<TouchKeyboard>,
        center: Point<Pixels>,
        event: MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<TouchKeyboard>,
    ) {
        self.stop_repeat();
        let generation = self.repeat_generation;
        let executor = cx.background_executor().clone();
        self._repeat = Some(cx.spawn_in(window, async move |_, cx| {
            executor.timer(Duration::from_millis(420)).await;
            loop {
                let keep_going = owner
                    .update_in(cx, |keyboard, window, cx| {
                        if keyboard.repeat_generation != generation {
                            return false;
                        }
                        keyboard.commit(Key::Backspace, center, &event, window, cx);
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
                executor.timer(Duration::from_millis(65)).await;
            }
        }));
    }

    fn render_keyboard(
        &self,
        window: &mut Window,
        cx: &mut Context<TouchKeyboard>,
    ) -> Stateful<Div> {
        let viewport = window.viewport_size();
        let width = f32::from(viewport.width);
        let top = f32::from(viewport.height) - HEIGHT;
        let colors = self.plugin.style(cx);
        let mut root = div()
            .id("touch-keyboard")
            .h(Self::height())
            .w_full()
            .flex()
            .flex_col()
            .gap(px(GAP))
            .p(px(3.))
            .border_t_1()
            .border_color(colors.border)
            .bg(colors.background);

        root = root.child(
            div().h(px(STRIP_HEIGHT)).flex().items_center().children(
                self.plugin
                    .context_chips()
                    .into_iter()
                    .enumerate()
                    .map(|(index, chip)| {
                        div()
                            .id(("context-chip", index))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(colors.key_background)
                            .text_color(colors.text_muted)
                            .child(chip.label)
                            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                                (chip.on_select)(window, cx);
                            })
                    }),
            ),
        );

        let rows: [Vec<(Key, f32)>; 6] = [
            ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"]
                .into_iter()
                .map(|key| (Key::Text(key), 1.))
                .collect(),
            ["q", "w", "e", "r", "t", "y", "u", "i", "o", "p"]
                .into_iter()
                .map(|key| (Key::Text(key), 1.))
                .collect(),
            ["a", "s", "d", "f", "g", "h", "j", "k", "l"]
                .into_iter()
                .map(|key| (Key::Text(key), 1.))
                .collect(),
            vec![
                (Key::Shift, 1.35),
                (Key::Text("z"), 1.),
                (Key::Text("x"), 1.),
                (Key::Text("c"), 1.),
                (Key::Text("v"), 1.),
                (Key::Text("b"), 1.),
                (Key::Text("n"), 1.),
                (Key::Text("m"), 1.),
                (Key::Backspace, 1.55),
            ],
            vec![
                (Key::Text("."), 1.),
                (Key::Text(","), 1.),
                (Key::Text("?"), 1.),
                (Key::Reserved, 1.),
                (Key::Enter, 2.),
            ],
            vec![(Key::Dismiss, 1.35), (Key::Space, 5.)],
        ];

        for (row_index, row) in rows.into_iter().enumerate() {
            let total_weight: f32 = row.iter().map(|(_, weight)| *weight).sum();
            let available = width - 6. - GAP * (row.len().saturating_sub(1) as f32);
            let unit = available / total_weight;
            let mut x = 3.;
            let mut row_div = div().h(px(ROW_HEIGHT)).w_full().flex().gap(px(GAP));
            for (key, weight) in row {
                let key_width = unit * weight;
                let center = point(
                    px(x + key_width / 2.),
                    px(top
                        + STRIP_HEIGHT
                        + GAP
                        + row_index as f32 * (ROW_HEIGHT + GAP)
                        + ROW_HEIGHT / 2.),
                );
                x += key_width + GAP;
                row_div = row_div.child(self.render_key(
                    key,
                    key_width,
                    center,
                    cx.entity().downgrade(),
                    colors,
                ));
            }
            root = root.child(row_div);
        }
        root
    }

    fn render_key(
        &self,
        key: Key,
        width: f32,
        center: Point<Pixels>,
        owner: WeakEntity<TouchKeyboard>,
        colors: KeyboardStyle,
    ) -> Stateful<Div> {
        let label = match key {
            Key::Text(text) if self.shift != Shift::Off => text.to_uppercase(),
            Key::Text(text) => text.into(),
            Key::Shift if self.shift == Shift::Locked => "⇧ lock".into(),
            Key::Shift => "⇧".into(),
            Key::Backspace => "⌫".into(),
            Key::Enter => "return".into(),
            Key::Space => "space".into(),
            Key::Dismiss => "hide".into(),
            Key::Reserved => "·".into(),
        };
        let repeat_owner = owner.clone();
        let repeat_owner_on_down = repeat_owner.clone();
        let repeat_owner_out = repeat_owner.clone();
        div()
            .id(("touch-key", f32::from(center.x).to_bits() as usize))
            .w(px(width))
            .h(px(ROW_HEIGHT))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .bg(colors.key_background)
            .border_1()
            .border_color(colors.key_border)
            .text_color(if matches!(key, Key::Reserved) {
                colors.text_disabled
            } else {
                colors.text
            })
            .active(|style| style.bg(colors.key_pressed))
            .child(label)
            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                cx.stop_propagation();
                let _ = owner.update(cx, |keyboard, cx| {
                    keyboard.commit(key, center, event, window, cx);
                    if matches!(key, Key::Backspace) {
                        keyboard.start_repeat(
                            repeat_owner_on_down.clone(),
                            center,
                            event.clone(),
                            window,
                            cx,
                        );
                    }
                });
            })
            .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                let _ = repeat_owner.update(cx, |keyboard, _| keyboard.stop_repeat());
            })
            .on_mouse_up_out(MouseButton::Left, move |_, _, cx| {
                let _ = repeat_owner_out.update(cx, |keyboard, _| keyboard.stop_repeat());
            })
    }
}

impl Render for TouchKeyboard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_keyboard(window, cx)
    }
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

fn load_telemetry() -> Vec<TelemetryEntry> {
    storage()
        .and_then(|storage| storage.get_item(STORAGE_KEY).ok().flatten())
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

pub fn dump_telemetry() {
    let value = serde_json::to_string(&load_telemetry()).unwrap_or_else(|_| "[]".into());
    web_sys::console::log_1(&value.into());
}

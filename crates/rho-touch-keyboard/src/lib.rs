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
const ROW_HEIGHT: f32 = 46.;
const H_GAP: f32 = 5.;
const V_GAP: f32 = 8.;
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
    /// Screen origin of the strip (the keyboard's content box), captured at
    /// prepaint so hit testing does not depend on where the owner places us.
    origin: Point<Pixels>,
    pressed: Option<(usize, usize)>,
}

/// Shift pairs punctuation with its iOS sibling rather than uppercasing.
fn shifted_text(text: &str) -> String {
    match text {
        "," => "!".into(),
        "." => "?".into(),
        _ => text.to_uppercase(),
    }
}

/// Fires a short haptic pulse where the platform allows it. The hook lives in
/// index.html because iOS has no vibration API and needs a DOM workaround.
fn haptic() {
    use wasm_bindgen::JsCast as _;
    let Some(window) = web_sys::window() else {
        return;
    };
    let window: &wasm_bindgen::JsValue = window.as_ref();
    if let Ok(hook) = js_sys::Reflect::get(window, &"__rhoHaptic".into())
        && let Some(hook) = hook.dyn_ref::<js_sys::Function>()
    {
        let _ = hook.call0(&wasm_bindgen::JsValue::NULL);
    }
}

#[derive(Clone, Copy)]
enum Key {
    Text(&'static str),
    Shift,
    Backspace,
    Enter,
    Space,
}

/// The visual key rectangles: (key, x, width) per row, on the iOS grid. Ten
/// equal-width keys set the unit; the home row is offset half a pitch, z–m is
/// centered with shift/backspace filling the edges, and the bottom row keeps
/// space/return at their iOS positions so phone muscle memory transfers.
fn layout_rows(width: f32) -> [Vec<(Key, f32, f32)>; 5] {
    let unit = (width - 11. * H_GAP) / 10.;
    let pitch = unit + H_GAP;
    let row_of_ten = |keys: [&'static str; 10]| -> Vec<(Key, f32, f32)> {
        keys.into_iter()
            .enumerate()
            .map(|(index, key)| (Key::Text(key), H_GAP + index as f32 * pitch, unit))
            .collect()
    };

    let home_row = ["a", "s", "d", "f", "g", "h", "j", "k", "l"]
        .into_iter()
        .enumerate()
        .map(|(index, key)| {
            (
                Key::Text(key),
                H_GAP + pitch / 2. + index as f32 * pitch,
                unit,
            )
        })
        .collect();

    let letters_margin = (width - 7. * unit - 6. * H_GAP) / 2.;
    let edge_width = letters_margin - 2. * H_GAP;
    let mut shift_row = vec![(Key::Shift, H_GAP, edge_width)];
    shift_row.extend(
        ["z", "x", "c", "v", "b", "n", "m"]
            .into_iter()
            .enumerate()
            .map(|(index, key)| (Key::Text(key), letters_margin + index as f32 * pitch, unit)),
    );
    shift_row.push((Key::Backspace, width - H_GAP - edge_width, edge_width));

    let side_width = 1.25 * unit;
    let return_width = 2.5 * unit;
    let space_width = width - 5. * H_GAP - 2. * side_width - return_width;
    let mut x = H_GAP;
    let mut bottom_row = Vec::new();
    for (key, key_width) in [
        (Key::Text(","), side_width),
        (Key::Space, space_width),
        (Key::Text("."), side_width),
        (Key::Enter, return_width),
    ] {
        bottom_row.push((key, x, key_width));
        x += key_width + H_GAP;
    }

    [
        row_of_ten(["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"]),
        row_of_ten(["q", "w", "e", "r", "t", "y", "u", "i", "o", "p"]),
        home_row,
        shift_row,
        bottom_row,
    ]
}

/// Maps a tap to a key using contiguous touch cells: the visual gaps are
/// live, split halfway between the facing key edges, edge keys extend to the
/// keyboard edges, and the bottom row soaks up the bottom padding. Only the
/// context strip is excluded — taps there belong to the chips. Returns the
/// key with its row/index and visual center (relative to the keyboard).
fn key_at(width: f32, x: f32, y: f32) -> Option<(Key, usize, usize, Point<Pixels>)> {
    if y < STRIP_HEIGHT {
        return None;
    }
    let pitch = ROW_HEIGHT + V_GAP;
    let band = y - STRIP_HEIGHT;
    let row_index = (((band - V_GAP / 2.) / pitch).floor().max(0.) as usize).min(4);
    let rows = layout_rows(width);
    let row = &rows[row_index];
    let mut chosen = row.len() - 1;
    for (index, window) in row.windows(2).enumerate() {
        let (_, left_x, left_width) = window[0];
        let (_, right_x, _) = window[1];
        let boundary = (left_x + left_width + right_x) / 2.;
        if x < boundary {
            chosen = index;
            break;
        }
    }
    let (key, key_x, key_width) = row[chosen];
    let center = point(
        px(key_x + key_width / 2.),
        px(STRIP_HEIGHT + V_GAP + row_index as f32 * pitch + ROW_HEIGHT / 2.),
    );
    Some((key, row_index, chosen, center))
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
            origin: point(px(0.), px(0.)),
            pressed: None,
        }
    }

    pub fn height() -> Pixels {
        px(HEIGHT)
    }

    /// The screen area the keyboard occupies when shown, `bottom_inset`
    /// above the viewport bottom (the owner's action bar sits below).
    pub fn region(viewport: gpui::Size<Pixels>, bottom_inset: Pixels) -> Bounds<Pixels> {
        Bounds::new(
            point(px(0.), viewport.height - bottom_inset - Self::height()),
            gpui::size(viewport.width, Self::height() + bottom_inset),
        )
    }

    /// Updates keyboard state for a key press and returns the keystroke to
    /// dispatch. Callers must dispatch it after this entity update ends:
    /// `dispatch_keystroke` runs arbitrary app code (input handlers,
    /// subscriptions, draws) that may re-enter this entity.
    fn commit(
        &mut self,
        key: Key,
        center: Point<Pixels>,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<TouchKeyboard>,
    ) -> Option<Keystroke> {
        let now = js_sys::Date::now();
        haptic();
        if let Key::Shift = key {
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
            return None;
        }

        let (stroke, telemetry_key) = match key {
            Key::Text(text) => {
                let text = if self.shift != Shift::Off {
                    shifted_text(text)
                } else {
                    text.into()
                };
                (text.clone(), text)
            }
            Key::Backspace => ("backspace".into(), "backspace".into()),
            Key::Enter => ("enter".into(), "enter".into()),
            Key::Space => ("space".into(), "space".into()),
            Key::Shift => unreachable!(),
        };
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
        Keystroke::parse(&stroke).ok()
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
                let step = owner
                    .update_in(cx, |keyboard, window, cx| {
                        if keyboard.repeat_generation != generation {
                            return None;
                        }
                        Some(keyboard.commit(Key::Backspace, center, &event, window, cx))
                    })
                    .ok()
                    .flatten();
                let Some(keystroke) = step else { break };
                if let Some(keystroke) = keystroke {
                    let _ = cx.update(|window, cx| {
                        window.dispatch_keystroke(keystroke, cx);
                    });
                }
                executor.timer(Duration::from_millis(65)).await;
            }
        }));
    }

    /// The single mouse-down dispatcher over the whole keyboard surface.
    /// Per-key hitboxes would leave the inter-key gaps dead (roughly a
    /// quarter of the grid at phone widths); resolving the key from the
    /// contiguous touch cells makes every pixel below the strip land.
    fn handle_mouse_down(
        keyboard: &WeakEntity<TouchKeyboard>,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut App,
    ) {
        let repeat_owner = keyboard.clone();
        let (handled, keystroke) = keyboard
            .update(cx, |this, cx| {
                let width = f32::from(window.viewport_size().width);
                let x = f32::from(event.position.x - this.origin.x);
                let y = f32::from(event.position.y - this.origin.y);
                let Some((key, row_index, key_index, center)) = key_at(width, x, y) else {
                    return (false, None);
                };
                let center = point(this.origin.x + center.x, this.origin.y + center.y);
                this.pressed = Some((row_index, key_index));
                cx.notify();
                let keystroke = this.commit(key, center, event, window, cx);
                if matches!(key, Key::Backspace) {
                    this.start_repeat(repeat_owner, center, event.clone(), window, cx);
                }
                (true, keystroke)
            })
            .unwrap_or((false, None));
        if handled {
            cx.stop_propagation();
        }
        if let Some(keystroke) = keystroke {
            window.dispatch_keystroke(keystroke, cx);
        }
    }

    fn release(&mut self, cx: &mut Context<TouchKeyboard>) {
        self.stop_repeat();
        if self.pressed.take().is_some() {
            cx.notify();
        }
    }

    fn render_keyboard(
        &self,
        window: &mut Window,
        cx: &mut Context<TouchKeyboard>,
    ) -> Stateful<Div> {
        let width = f32::from(window.viewport_size().width);
        let colors = self.plugin.style(cx);
        let owner = cx.entity().downgrade();
        let origin_owner = owner.clone();
        let up_owner = owner.clone();
        let up_out_owner = owner.clone();
        let mut root = div()
            .on_children_prepainted(move |bounds, _, cx| {
                let _ = origin_owner.update(cx, |this, _| {
                    if let Some(strip) = bounds.first() {
                        this.origin = strip.origin;
                    }
                });
            })
            .id("touch-keyboard")
            .h(Self::height())
            .w_full()
            .flex()
            .flex_col()
            .gap(px(V_GAP))
            .border_t_1()
            .border_color(colors.border)
            .bg(colors.background)
            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                Self::handle_mouse_down(&owner, event, window, cx);
            })
            .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                let _ = up_owner.update(cx, |keyboard, cx| keyboard.release(cx));
            })
            .on_mouse_up_out(MouseButton::Left, move |_, _, cx| {
                let _ = up_out_owner.update(cx, |keyboard, cx| keyboard.release(cx));
            });

        root = root.child(
            div()
                .h(px(STRIP_HEIGHT))
                .px(px(H_GAP))
                .flex()
                .items_center()
                .children(
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

        for (row_index, row) in layout_rows(width).into_iter().enumerate() {
            let mut row_div = div().relative().h(px(ROW_HEIGHT)).w_full();
            for (key_index, (key, key_x, key_width)) in row.into_iter().enumerate() {
                row_div = row_div.child(self.render_key(
                    key,
                    (row_index, key_index),
                    key_x,
                    key_width,
                    colors,
                ));
            }
            root = root.child(row_div);
        }
        root
    }

    /// A purely visual key face: hit testing happens on the keyboard
    /// surface, so the painted rectangle can keep its gaps.
    fn render_key(
        &self,
        key: Key,
        position: (usize, usize),
        x: f32,
        width: f32,
        colors: KeyboardStyle,
    ) -> Div {
        let label = match key {
            Key::Text(text) if self.shift != Shift::Off => shifted_text(text),
            Key::Text(text) => text.into(),
            Key::Shift if self.shift == Shift::Locked => "⇧ lock".into(),
            Key::Shift => "⇧".into(),
            Key::Backspace => "⌫".into(),
            Key::Enter => "return".into(),
            Key::Space => "space".into(),
        };
        div()
            .absolute()
            .left(px(x))
            .top(px(0.))
            .w(px(width))
            .h(px(ROW_HEIGHT))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .bg(if self.pressed == Some(position) {
                colors.key_pressed
            } else {
                colors.key_background
            })
            .border_1()
            .border_color(colors.key_border)
            .text_color(colors.text)
            .child(label)
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

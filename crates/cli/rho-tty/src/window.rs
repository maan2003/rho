//! The one window: the terminal grid, sized in pixels at one cell per
//! `CELL_W` × `CELL_H`.

use std::cell::{Cell as StdCell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    Bounds, Capslock, DispatchEventResult, DisplayId, GpuSpecs, Modifiers, MouseButton, Pixels,
    PlatformAtlas, PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow, Point,
    PromptButton, PromptLevel, RequestFrameOptions, Scene, Size, WindowAppearance,
    WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowVisibility, point, px, size,
};

use crate::atlas::CellAtlas;
use crate::grid::{Grid, Screen};
use crate::{CELL_H, CELL_W};

#[derive(Debug)]
pub(crate) struct TtyDisplay {
    pub(crate) cells: StdCell<(u16, u16)>,
}

pub(crate) fn pixel_size((cols, rows): (u16, u16)) -> Size<Pixels> {
    size(px(cols as f32 * CELL_W), px(rows as f32 * CELL_H))
}

impl PlatformDisplay for TtyDisplay {
    /// A terminal has no refresh rate gpui can pace by.
    fn refresh_interval(&self) -> Option<std::time::Duration> {
        None
    }

    fn id(&self) -> DisplayId {
        DisplayId::new(0)
    }

    fn uuid(&self) -> anyhow::Result<uuid::Uuid> {
        Ok(uuid::Uuid::nil())
    }

    fn bounds(&self) -> Bounds<Pixels> {
        Bounds {
            origin: Point::default(),
            size: pixel_size(self.cells.get()),
        }
    }
}

type ResizeCallback = Box<dyn FnMut(Size<Pixels>, f32)>;

#[derive(Default)]
struct Callbacks {
    request_frame: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    input: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    active: Option<Box<dyn FnMut(bool)>>,
    hover: Option<Box<dyn FnMut(bool)>>,
    resize: Option<ResizeCallback>,
    close: Option<Box<dyn FnOnce()>>,
}

pub(crate) struct WindowState {
    pub(crate) input_handler: Option<PlatformInputHandler>,
    pub(crate) mouse: Point<Pixels>,
    pub(crate) modifiers: Modifiers,
    pub(crate) pressed: Option<MouseButton>,
    title: String,
}

pub(crate) struct WindowInner {
    pub(crate) display: Rc<TtyDisplay>,
    pub(crate) state: RefCell<WindowState>,
    callbacks: RefCell<Callbacks>,
    atlas: Arc<CellAtlas>,
    screen: RefCell<Screen>,
    /// The frame gpui drew, held until its caret is known.
    drawn: RefCell<Option<Grid>>,
    pub(crate) frame_wanted: StdCell<bool>,
    force_render: StdCell<bool>,
}

impl WindowInner {
    pub(crate) fn new(display: Rc<TtyDisplay>, screen: Screen) -> Rc<Self> {
        Rc::new(Self {
            display,
            state: RefCell::new(WindowState {
                input_handler: None,
                mouse: Point::default(),
                modifiers: Modifiers::default(),
                pressed: None,
                title: String::new(),
            }),
            callbacks: RefCell::default(),
            atlas: Arc::default(),
            screen: RefCell::new(screen),
            drawn: RefCell::default(),
            frame_wanted: StdCell::new(true),
            force_render: StdCell::new(true),
        })
    }

    pub(crate) fn cells(&self) -> (u16, u16) {
        self.display.cells.get()
    }

    /// Hands input to gpui; a key it does not bind is typed, as the
    /// Wayland window does.
    pub(crate) fn handle_input(&self, input: PlatformInput) {
        let callback = self.callbacks.borrow_mut().input.take();
        if let Some(mut callback) = callback {
            let result = callback(input.clone());
            crate::debug(format_args!(
                "input {input:?} -> propagate {}",
                result.propagate
            ));
            self.callbacks.borrow_mut().input = Some(callback);
            if !result.propagate {
                self.frame_wanted.set(true);
                return;
            }
        }
        if let PlatformInput::KeyDown(event) = input
            && event.keystroke.modifiers.is_subset_of(&Modifiers::shift())
            && let Some(key_char) = &event.keystroke.key_char
        {
            let handler = self.state.borrow_mut().input_handler.take();
            if let Some(mut handler) = handler {
                handler.replace_text_in_range(None, key_char);
                self.state.borrow_mut().input_handler = Some(handler);
            }
        }
        self.frame_wanted.set(true);
    }

    pub(crate) fn resized(&self, cells: (u16, u16)) {
        self.display.cells.set(cells);
        self.screen.borrow_mut().invalidate();
        let callback = self.callbacks.borrow_mut().resize.take();
        if let Some(mut callback) = callback {
            callback(pixel_size(cells), 1.0);
            self.callbacks.borrow_mut().resize = Some(callback);
        }
        self.force_render.set(true);
        self.frame_wanted.set(true);
    }

    pub(crate) fn set_active(&self, active: bool) {
        for slot in [0, 1] {
            let callback = if slot == 0 {
                self.callbacks.borrow_mut().active.take()
            } else {
                self.callbacks.borrow_mut().hover.take()
            };
            if let Some(mut callback) = callback {
                callback(active);
                let mut callbacks = self.callbacks.borrow_mut();
                if slot == 0 {
                    callbacks.active = Some(callback);
                } else {
                    callbacks.hover = Some(callback);
                }
            }
        }
    }

    /// Runs gpui's frame if one is wanted.
    pub(crate) fn frame(&self) {
        if !self.frame_wanted.replace(false) {
            return;
        }
        let callback = self.callbacks.borrow_mut().request_frame.take();
        if let Some(mut callback) = callback {
            callback(RequestFrameOptions {
                require_presentation: false,
                force_render: self.force_render.replace(false),
                host_vsync: None,
                signal_at: None,
                signal_source: gpui::FrameRequestSource::LocalSchedule,
            });
            self.callbacks.borrow_mut().request_frame = Some(callback);
        }
        let Some(mut grid) = self.drawn.take() else {
            return;
        };
        let caret = self.caret();
        grid.set_caret(
            caret.and_then(|b| grid.cell_at(f32::from(b.origin.x), f32::from(b.origin.y))),
        );
        if let Err(error) = self.screen.borrow_mut().present(grid) {
            eprintln!("rho-tty: writing the frame failed: {error}");
        }
    }

    /// The focused input's caret, where text typed now would go: none
    /// when nothing takes text, as in vim's normal mode.
    fn caret(&self) -> Option<Bounds<Pixels>> {
        let mut handler = self.state.borrow_mut().input_handler.take()?;
        let bounds = handler.selected_text_range(false).and_then(|selection| {
            let at = if selection.reversed {
                selection.range.start
            } else {
                selection.range.end
            };
            handler.bounds_for_range(at..at)
        });
        let mut state = self.state.borrow_mut();
        if state.input_handler.is_none() {
            state.input_handler = Some(handler);
        }
        bounds
    }

    pub(crate) fn close(&self) {
        if let Some(callback) = self.callbacks.borrow_mut().close.take() {
            callback();
        }
    }
}

pub(crate) struct TtyWindow(pub(crate) Rc<WindowInner>);

impl raw_window_handle::HasWindowHandle for TtyWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        Err(raw_window_handle::HandleError::NotSupported)
    }
}

impl raw_window_handle::HasDisplayHandle for TtyWindow {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Err(raw_window_handle::HandleError::NotSupported)
    }
}

impl PlatformWindow for TtyWindow {
    /// The window is the terminal, which never moves to another display.
    fn on_display_changed(&self, _callback: Box<dyn FnMut()>) {}

    fn bounds(&self) -> Bounds<Pixels> {
        self.0.display.bounds()
    }

    fn is_maximized(&self) -> bool {
        true
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Fullscreen(self.bounds())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.bounds().size
    }

    fn resize(&mut self, _size: Size<Pixels>) {}

    fn scale_factor(&self) -> f32 {
        1.0
    }

    fn appearance(&self) -> WindowAppearance {
        WindowAppearance::Dark
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.0.display.clone())
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.0.state.borrow().mouse
    }

    fn modifiers(&self) -> Modifiers {
        self.0.state.borrow().modifiers
    }

    fn capslock(&self) -> Capslock {
        Capslock::default()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.0.state.borrow_mut().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.0.state.borrow_mut().input_handler.take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<futures::channel::oneshot::Receiver<usize>> {
        None
    }

    fn activate(&self) {}

    fn is_active(&self) -> bool {
        true
    }

    fn visibility(&self) -> WindowVisibility {
        WindowVisibility::Visible
    }

    fn is_hovered(&self) -> bool {
        true
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }

    fn set_title(&mut self, title: &str) {
        self.0.state.borrow_mut().title = title.to_owned();
    }

    fn get_title(&self) -> String {
        self.0.state.borrow().title.clone()
    }

    fn set_background_appearance(&self, _background: WindowBackgroundAppearance) {}

    fn minimize(&self) {}

    fn zoom(&self) {}

    fn toggle_fullscreen(&self) {}

    fn is_fullscreen(&self) -> bool {
        true
    }

    fn frame_waker(&self) -> Option<Rc<dyn Fn()>> {
        let inner = Rc::downgrade(&self.0);
        Some(Rc::new(move || {
            if let Some(inner) = inner.upgrade() {
                inner.frame_wanted.set(true);
            }
        }))
    }

    fn schedule_frame(&self) {
        self.0.frame_wanted.set(true);
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.0.callbacks.borrow_mut().request_frame = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.0.callbacks.borrow_mut().input = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.callbacks.borrow_mut().active = Some(callback);
    }

    fn on_visibility_change(&self, _callback: Box<dyn FnMut(WindowVisibility)>) {}

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.callbacks.borrow_mut().hover = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.0.callbacks.borrow_mut().resize = Some(callback);
    }

    fn on_moved(&self, _callback: Box<dyn FnMut()>) {}

    fn on_should_close(&self, _callback: Box<dyn FnMut() -> bool>) {}

    fn on_hit_test_window_control(&self, _callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.0.callbacks.borrow_mut().close = Some(callback);
    }

    fn on_appearance_changed(&self, _callback: Box<dyn FnMut()>) {}

    fn draw(&self, scene: &Scene) {
        let (cols, rows) = self.0.cells();
        *self.0.drawn.borrow_mut() = Some(Grid::paint(scene, &self.0.atlas, cols, rows));
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.0.atlas.clone()
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}

    /// Layout lands on cells, so text and the boxes around it line up.
    fn layout_grid(&self) -> Option<Size<Pixels>> {
        Some(size(px(CELL_W), px(CELL_H)))
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        None
    }
}

/// The pixel at the centre of a cell.
pub(crate) fn cell_center(col: u16, row: u16) -> Point<Pixels> {
    point(
        px((col as f32 + 0.5) * CELL_W),
        px((row as f32 + 0.5) * CELL_H),
    )
}

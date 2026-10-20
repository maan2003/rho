//! A gpui platform drawn into a terminal.
//!
//! The window is the terminal grid at [`CELL_W`] × [`CELL_H`] pixels a
//! cell. gpui lays out and paints exactly as it does on a GPU; the text
//! system makes every glyph one cell wide, and the window paints the Scene
//! it is handed into cells instead of pixels. Nothing above gpui knows it
//! is in a terminal, so rho's buffers run here unchanged.
//!
//! Text lines up with cells only at the editor's size: a 16 px font with a
//! line height of one. Text at other sizes still lands in the nearest cell.

mod atlas;
mod dispatcher;
mod grid;
mod headless;
mod input;
mod platform;
mod text;
mod window;

pub use headless::Headless;
pub use platform::TtyPlatform;

/// One cell's width in gpui pixels.
pub const CELL_W: f32 = 8.0;
/// One cell's height in gpui pixels: one line of 16 px text.
pub const CELL_H: f32 = 16.0;
/// The font size whose characters are exactly one cell wide.
pub const FONT_SIZE: f32 = 16.0;

/// A gpui application that runs in this terminal.
pub fn application() -> gpui::Application {
    gpui::Application::with_platform(std::rc::Rc::new(TtyPlatform::new()))
}

/// A gpui application on no terminal at all, `cols` × `rows` cells, and
/// the handle a test types into it and reads it back by.
pub fn headless(cols: u16, rows: u16) -> (gpui::Application, Headless) {
    let (platform, headless) = TtyPlatform::headless(cols, rows);
    (
        gpui::Application::with_platform(std::rc::Rc::new(platform)),
        headless,
    )
}

/// Appends a line to `$RHO_TTY_LOG`, if set: the terminal itself is the
/// screen, so there is nowhere else to say what happened.
pub(crate) fn debug(args: std::fmt::Arguments) {
    use std::io::Write as _;
    static LOG: std::sync::LazyLock<Option<std::sync::Mutex<std::fs::File>>> =
        std::sync::LazyLock::new(|| {
            let path = std::env::var_os("RHO_TTY_LOG")?;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
                .map(std::sync::Mutex::new)
        });
    if let Some(log) = LOG.as_ref() {
        let _ = writeln!(log.lock().unwrap(), "{args}");
    }
}

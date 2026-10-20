//! The gpui platform: one window on the terminal, and a main loop that
//! wakes for gpui's runnables and the terminal's input alike.

use std::cell::{Cell, RefCell};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use base64::Engine as _;
use crossterm::cursor::{SetCursorStyle, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use futures::channel::oneshot;
use gpui::{
    Action, ActivityGuard, AnyWindowHandle, BackgroundExecutor, ClipboardItem, CursorStyle,
    DummyKeyboardMapper, ForegroundExecutor, Keymap, Menu, MenuItem, PathPromptOptions, Platform,
    PlatformDisplay, PlatformKeyboardLayout, PlatformKeyboardMapper, PlatformTextSystem,
    PlatformWindow, Result, Task, ThermalState, WindowAppearance, WindowParams,
};

use crate::dispatcher::{MainMsg, TtyDispatcher};
use crate::grid::Screen;
use crate::headless::{Headless, Shared};
use crate::input::{self, Clicks};
use crate::text::CellTextSystem;
use crate::window::{TtyDisplay, TtyWindow, WindowInner};

pub struct TtyPlatform {
    dispatcher: Arc<TtyDispatcher>,
    /// No terminal: frames go to memory and input comes from a test.
    headless: Option<Rc<Shared>>,
    background: BackgroundExecutor,
    foreground: ForegroundExecutor,
    text_system: Arc<CellTextSystem>,
    sender: Sender<MainMsg>,
    receiver: RefCell<Option<Receiver<MainMsg>>>,
    display: Rc<TtyDisplay>,
    window: RefCell<Option<(AnyWindowHandle, Rc<WindowInner>)>>,
    quitting: Cell<bool>,
    on_quit: RefCell<Vec<Box<dyn FnMut() -> bool>>>,
    clipboard: RefCell<Option<ClipboardItem>>,
    primary: RefCell<Option<ClipboardItem>>,
}

impl TtyPlatform {
    pub fn new() -> Self {
        Self::build(terminal::size().unwrap_or((80, 24)), None)
    }

    /// A platform with no terminal, `cols` × `rows` cells, and the handle
    /// a test drives it by.
    pub fn headless(cols: u16, rows: u16) -> (Self, Headless) {
        let shared = Rc::new(Shared::default());
        let platform = Self::build((cols, rows), Some(shared.clone()));
        let headless = Headless {
            sender: platform.sender.clone(),
            shared,
        };
        (platform, headless)
    }

    fn build(cells: (u16, u16), headless: Option<Rc<Shared>>) -> Self {
        let (sender, receiver) = mpsc::channel();
        let dispatcher = Arc::new(TtyDispatcher::new(sender.clone()));
        Self {
            dispatcher: dispatcher.clone(),
            headless,
            background: BackgroundExecutor::new(dispatcher.clone()),
            foreground: ForegroundExecutor::new(dispatcher),
            text_system: Arc::new(CellTextSystem),
            sender,
            receiver: RefCell::new(Some(receiver)),
            display: Rc::new(TtyDisplay {
                cells: Cell::new(cells),
            }),
            window: RefCell::default(),
            quitting: Cell::new(false),
            on_quit: RefCell::default(),
            clipboard: RefCell::default(),
            primary: RefCell::default(),
        }
    }

    fn window(&self) -> Option<Rc<WindowInner>> {
        self.window
            .borrow()
            .as_ref()
            .map(|(_, inner)| inner.clone())
    }

    /// Reads the terminal on a thread of its own, into the main channel.
    fn read_input(&self) {
        let input = self.sender.clone();
        std::thread::Builder::new()
            .name("tty-input".into())
            .spawn(move || {
                loop {
                    let message = match crossterm::event::read() {
                        Ok(event) => MainMsg::Input(event),
                        Err(_) => MainMsg::InputClosed,
                    };
                    let closed = matches!(message, MainMsg::InputClosed);
                    if input.send(message).is_err() || closed {
                        return;
                    }
                }
            })
            .expect("spawn input thread");
    }

    fn run_loop(&self, receiver: Receiver<MainMsg>, on_finish_launching: Box<dyn FnOnce()>) {
        on_finish_launching();
        // A terminal has its user's attention until it says otherwise.
        if let Some(window) = self.window() {
            window.set_active(true);
        }
        let mut clicks = Clicks::default();
        while !self.quitting.get() {
            let wanted = self.window().is_some_and(|w| w.frame_wanted.get());
            let first = if wanted {
                receiver.try_recv().map_err(|e| match e {
                    mpsc::TryRecvError::Empty => RecvTimeoutError::Timeout,
                    mpsc::TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
                })
            } else {
                receiver.recv_timeout(Duration::from_millis(500))
            };
            let mut next = first;
            let started = Instant::now();
            let mut drained = false;
            loop {
                match next {
                    Ok(MainMsg::Run(runnable)) => {
                        runnable.run();
                    }
                    Ok(MainMsg::Input(event)) => {
                        if let Some(window) = self.window() {
                            input::dispatch(&window, &mut clicks, event);
                        }
                    }
                    Ok(MainMsg::InputClosed) => self.quit(),
                    Ok(MainMsg::Wake) => {}
                    Err(RecvTimeoutError::Timeout) => {
                        drained = true;
                        break;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
                if self.quitting.get() || started.elapsed() > BATCH {
                    break;
                }
                next = receiver.try_recv().map_err(|_| RecvTimeoutError::Timeout);
            }
            let window = self.window();
            if let Some(window) = &window {
                window.frame();
            }
            if let Some(shared) = &self.headless
                && drained
                && !window.is_some_and(|w| w.frame_wanted.get())
                && !self.dispatcher.is_busy()
            {
                shared.idle();
            }
        }
        if let Some(window) = self.window() {
            window.close();
        }
    }
}

impl Default for TtyPlatform {
    fn default() -> Self {
        Self::new()
    }
}

fn enter_terminal() -> std::io::Result<()> {
    terminal::enable_raw_mode()?;
    execute!(
        std::io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        EnableFocusChange
    )
}

pub(crate) fn leave_terminal() {
    let _ = execute!(
        std::io::stdout(),
        DisableFocusChange,
        DisableBracketedPaste,
        DisableMouseCapture,
        SetCursorStyle::DefaultUserShape,
        Show,
        LeaveAlternateScreen
    );
    let _ = terminal::disable_raw_mode();
}

/// How long one turn of the loop may run messages before it draws.
const BATCH: Duration = Duration::from_millis(8);

impl Platform for TtyPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.background.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn 'static + FnOnce()>) {
        let receiver = self
            .receiver
            .borrow_mut()
            .take()
            .expect("the platform runs once");
        if self.headless.is_none() {
            if let Err(error) = enter_terminal() {
                eprintln!("rho-tty: cannot set up the terminal: {error}");
                return;
            }
            let hook = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                leave_terminal();
                hook(info);
            }));
            self.read_input();
        }
        self.run_loop(receiver, on_finish_launching);
        if self.headless.is_none() {
            leave_terminal();
        }
    }

    fn quit(&self) {
        let mut callbacks = self.on_quit.borrow_mut();
        for callback in callbacks.iter_mut() {
            callback();
        }
        self.quitting.set(true);
    }

    fn restart(&self, _binary_path: Option<PathBuf>, _arguments: Vec<std::ffi::OsString>) {}

    fn activate(&self, _ignoring_other_apps: bool) {}

    fn hide(&self) {}

    fn hide_other_apps(&self) {}

    fn unhide_other_apps(&self) {}

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        vec![self.display.clone()]
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.display.clone())
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        self.window.borrow().as_ref().map(|(handle, _)| *handle)
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        _options: WindowParams,
    ) -> anyhow::Result<Box<dyn PlatformWindow>> {
        anyhow::ensure!(
            self.window.borrow().is_none(),
            "a terminal holds one window"
        );
        let screen = match &self.headless {
            Some(shared) => Screen::in_memory(shared.grid.clone()),
            None => Screen::default(),
        };
        let inner = WindowInner::new(self.display.clone(), screen);
        *self.window.borrow_mut() = Some((handle, inner.clone()));
        Ok(Box::new(TtyWindow(inner)))
    }

    fn window_appearance(&self) -> WindowAppearance {
        WindowAppearance::Dark
    }

    fn open_url(&self, _url: &str) {}

    fn on_open_urls(&self, _callback: Box<dyn FnMut(Vec<String>)>) {}

    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn prompt_for_paths(
        &self,
        _options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        oneshot::channel().1
    }

    fn prompt_for_new_path(
        &self,
        _directory: &Path,
        _suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        oneshot::channel().1
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }

    fn reveal_path(&self, _path: &Path) {}

    fn open_with_system(&self, _path: &Path) {}

    fn on_quit(&self, callback: Box<dyn FnMut() -> bool>) {
        self.on_quit.borrow_mut().push(callback);
    }

    fn on_reopen(&self, _callback: Box<dyn FnMut()>) {}

    fn on_system_sleep(&self, _callback: Box<dyn FnMut()>) {}

    fn on_system_wake(&self, _callback: Box<dyn FnMut()>) {}

    fn set_menus(&self, _menus: Vec<Menu>, _keymap: &Keymap) {}

    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {}

    fn on_app_menu_action(&self, _callback: Box<dyn FnMut(&dyn Action)>) {}

    fn on_will_open_app_menu(&self, _callback: Box<dyn FnMut()>) {}

    fn on_validate_app_menu_command(&self, _callback: Box<dyn FnMut(&dyn Action) -> bool>) {}

    fn thermal_state(&self) -> ThermalState {
        ThermalState::Nominal
    }

    fn on_thermal_state_change(&self, _callback: Box<dyn FnMut()>) {}

    fn prevent_idle_sleep(&self, _reason: &str) -> Task<Result<ActivityGuard>> {
        Task::ready(Ok(ActivityGuard::noop()))
    }

    fn app_path(&self) -> Result<PathBuf> {
        Ok(std::env::current_exe()?)
    }

    fn path_for_auxiliary_executable(&self, name: &str) -> Result<PathBuf> {
        anyhow::bail!("no auxiliary executable {name:?} in a terminal")
    }

    fn set_cursor_style(&self, _style: CursorStyle) {}

    fn hide_cursor_until_mouse_moves(&self) {}

    fn is_cursor_visible(&self) -> bool {
        true
    }

    fn should_auto_hide_scrollbars(&self) -> bool {
        true
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        self.clipboard.borrow().clone()
    }

    /// Kept for pasting here, and sent to the terminal as OSC 52 so it
    /// reaches the system clipboard too.
    fn write_to_clipboard(&self, item: ClipboardItem) {
        if let Some(text) = item.text() {
            let encoded = base64::engine::general_purpose::STANDARD.encode(text);
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "\x1b]52;c;{encoded}\x07");
            let _ = out.flush();
        }
        *self.clipboard.borrow_mut() = Some(item);
    }

    fn read_from_primary(&self) -> Option<ClipboardItem> {
        self.primary.borrow().clone()
    }

    fn write_to_primary(&self, item: ClipboardItem) {
        *self.primary.borrow_mut() = Some(item);
    }

    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(Ok(None))
    }

    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(TerminalLayout)
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }

    fn on_keyboard_layout_change(&self, _callback: Box<dyn FnMut()>) {}
}

struct TerminalLayout;

impl PlatformKeyboardLayout for TerminalLayout {
    fn id(&self) -> &str {
        "terminal"
    }

    fn name(&self) -> &str {
        "terminal"
    }
}

use editor::{Backspace, MoveLeft, MoveRight, MoveToBeginningOfLine, MoveToEndOfLine, Newline, Redo, Undo};
use gpui::{App, AppContext as _, Bounds, KeyBinding, WindowBounds, WindowOptions, px, size};
use rho_gui::rho_assets::RhoAssets;
use rho_gui::workspace::Workspace;

fn main() {
    console_error_panic_hook::set_once();
    gpui_platform::web_init();
    let app = gpui_platform::application()
        .with_assets(RhoAssets)
        .run_embedded(|cx: &mut App| {
        RhoAssets.load_fonts(cx).expect("load embedded fonts");
        // Mirror the native client's settings: rho defaults (Rho Font buffer
        // font) plus the same theme the native binary writes into fresh user
        // settings, with the full rho theme assets loaded.
        let mut store = settings::SettingsStore::new(cx, rho_gui::rho_assets::RHO_DEFAULT_SETTINGS);
        store
            .set_user_settings(r#"{"theme": "Rho OKSolar P3"}"#, cx)
            .result()
            .expect("load web user settings");
        cx.set_global(store);
        theme_settings::init(theme::LoadThemes::All(Box::new(RhoAssets)), cx);
        editor::init(cx);
        cx.bind_keys([
            KeyBinding::new("left", MoveLeft, Some("Editor")),
            KeyBinding::new("right", MoveRight, Some("Editor")),
            KeyBinding::new("up", zed_actions::editor::MoveUp, Some("Editor")),
            KeyBinding::new("down", zed_actions::editor::MoveDown, Some("Editor")),
            KeyBinding::new("home", MoveToBeginningOfLine::default(), Some("Editor")),
            KeyBinding::new("end", MoveToEndOfLine::default(), Some("Editor")),
            KeyBinding::new("backspace", Backspace, Some("Editor")),
            KeyBinding::new("enter", Newline, Some("Editor")),
            KeyBinding::new("ctrl-z", Undo, Some("Editor")),
            KeyBinding::new("ctrl-shift-z", Redo, Some("Editor")),
        ]);
        let bounds = Bounds::centered(None, size(px(1180.), px(720.)), cx);
        cx.open_window(
            WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() },
            |window, cx| cx.new(|cx| Workspace::new(window, cx)),
        ).expect("open browser window");
        cx.activate(true);
    });
    std::mem::forget(app);
}

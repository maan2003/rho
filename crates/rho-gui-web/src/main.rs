use gpui::{App, AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};
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
            let mut store =
                settings::SettingsStore::new(cx, rho_gui::rho_assets::RHO_DEFAULT_SETTINGS);
            store
                .set_user_settings(
                    // Taller lines than the desktop default: rows double as touch
                    // targets in the browser client.
                    r#"{"theme": "Rho OKSolar P3", "buffer_line_height": {"custom": 1.8}}"#,
                    cx,
                )
                .result()
                .expect("load web user settings");
            store.override_global(vim_mode_setting::VimModeSetting(true));
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::All(Box::new(RhoAssets)), cx);
            editor::init(cx);
            rho_gui::init_vim_mode(cx).expect("initialize Rho Vim mode");
            let bounds = Bounds::centered(None, size(px(1180.), px(720.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| Workspace::new(window, cx)),
            )
            .expect("open browser window");
            cx.activate(true);
        });
    std::mem::forget(app);
}

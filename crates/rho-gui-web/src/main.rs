use editor::{Backspace, MoveLeft, MoveRight, MoveToBeginningOfLine, MoveToEndOfLine, Newline, Redo, Undo};
use gpui::{App, AppContext as _, Bounds, KeyBinding, WindowBounds, WindowOptions, px, size};
use rho_gui::{
    AgentDone, AgentHide, DashboardNewAgent, DashboardReply, DashboardToggleSubagents, RailOpen,
    SubmitPrompt,
};
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
            .set_user_settings(
                // Taller lines than the desktop default: rows double as touch
                // targets in the browser client.
                r#"{"theme": "Rho OKSolar P3", "buffer_line_height": {"custom": 1.8}}"#,
                cx,
            )
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
            // The transcript's prompt is chat-style: Enter sends, and the
            // deeper context outranks the plain `Editor` Newline binding.
            KeyBinding::new("enter", SubmitPrompt, Some("RhoTranscript > Editor")),
            KeyBinding::new("shift-enter", Newline, Some("RhoTranscript > Editor")),
            // Dashboard triage, as on the desktop: the listing is read-only,
            // so plain letters act on the row under the cursor. The native
            // client qualifies these with `vim_mode == normal`; there is no
            // vim here, so the bare dashboard context is the whole of it —
            // without them the browser client cannot triage a row at all.
            KeyBinding::new("enter", RailOpen, Some("RhoDashboard > Editor")),
            KeyBinding::new("r", DashboardReply, Some("RhoDashboard > Editor")),
            KeyBinding::new("d", AgentDone, Some("RhoDashboard > Editor")),
            KeyBinding::new("shift-d", AgentHide, Some("RhoDashboard > Editor")),
            KeyBinding::new("n", DashboardNewAgent, Some("RhoDashboard > Editor")),
            KeyBinding::new(
                "tab",
                DashboardToggleSubagents,
                Some("RhoDashboard > Editor"),
            ),
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

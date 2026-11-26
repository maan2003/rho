//! Chrome settings for rho's editors: bare buffers in the frame. Chat
//! editors (agent view, draft compose) also drop editing affordances;
//! file editors keep them but shed the same chrome.

use editor::Editor;
use gpui::{App, Context, TextStyleRefinement, Window};

/// Initializes the modal editor engine and the exact keymap stack every
/// rho editor reads keys with. rho is Helix-first: Helix is forced on so
/// no settings file can silently drop the user back into plain vim. A
/// screen's own keys are bound after this, so they come before the
/// bundled ones where both match.
pub fn init_vim_mode(cx: &mut App) -> anyhow::Result<()> {
    let settings = cx.global_mut::<settings::SettingsStore>();
    settings.override_global(vim_mode_setting::VimModeSetting(false));
    settings.override_global(vim_mode_setting::HelixModeSetting(true));
    vim::init(cx);
    let default_key_bindings =
        settings::KeymapFile::load_asset_allow_partial_failure(settings::DEFAULT_KEYMAP_PATH, cx)?;
    cx.bind_keys(default_key_bindings);
    let vim_key_bindings =
        settings::KeymapFile::load_asset_allow_partial_failure(settings::VIM_KEYMAP_PATH, cx)?;
    anyhow::ensure!(
        cx.build_action("vim::Left", None).is_ok(),
        "Vim actions are missing from the application registry"
    );
    anyhow::ensure!(
        vim_key_bindings
            .iter()
            .any(|binding| binding.action().name() == "vim::Left"),
        "the Vim keymap did not load its core motion bindings"
    );
    cx.bind_keys(vim_key_bindings);
    Ok(())
}

#[allow(unused_variables)]
pub fn configure(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    editor.set_show_gutter(false, cx);
    editor.set_show_compact_gutter(true, cx);
    editor.set_show_line_numbers(false, cx);
    editor.set_show_git_diff_gutter(false, cx);
    editor.set_show_code_actions(false, cx);
    editor.set_show_runnables(false, cx);
    editor.set_show_breakpoints(false, cx);
    editor.set_show_bookmarks(false, cx);
    editor.set_show_scrollbars(false, cx);
    editor.set_offset_content(false, cx);
    editor.set_mouse_click_selection_enabled(false, cx);
    editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
    editor.set_show_wrap_guides(false, cx);
    editor.set_show_indent_guides(false, cx);
    editor.set_autoindent(false);
    editor.set_show_edit_predictions(Some(false), window, cx);
    editor.set_use_selection_highlight(false);
    editor.disable_expand_excerpt_buttons(cx);
}

/// Chrome and typography for the compact dashboard preview. Its text is
/// deliberately 85% the size of a full editor without affecting viewport
/// editors.
pub fn configure_preview(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    configure(editor, window, cx);
    let font_size = editor.style(cx).text.font_size.to_pixels(window.rem_size()) * 0.85;
    editor.set_text_style_refinement(TextStyleRefinement {
        font_size: Some(font_size.into()),
        ..Default::default()
    });
}

/// Chrome for file buffers: a real code editor dressed as a rho buffer.
/// Unlike the chat editors it keeps editing behavior (autoindent, mouse
/// selection, no soft wrap), but sheds every gutter column and overlay —
/// no line numbers by choice, no scrollbars, no guides.
#[allow(unused_variables)]
pub fn configure_file(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    editor.set_show_gutter(false, cx);
    editor.set_show_line_numbers(false, cx);
    editor.set_show_git_diff_gutter(false, cx);
    editor.set_show_code_actions(false, cx);
    editor.set_show_runnables(false, cx);
    editor.set_show_breakpoints(false, cx);
    editor.set_show_bookmarks(false, cx);
    editor.set_show_scrollbars(false, cx);
    editor.set_offset_content(false, cx);
    editor.set_show_wrap_guides(false, cx);
    editor.set_show_indent_guides(false, cx);
    editor.set_show_edit_predictions(Some(false), window, cx);
    editor.disable_expand_excerpt_buttons(cx);
}

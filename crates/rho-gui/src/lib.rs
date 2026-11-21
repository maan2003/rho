//! rho-gui: the one window over the frame and the systems.
//!
//! The frame (`rho-frame`) owns everything every screen shares; each
//! system (`rho-chat`, …) owns its own surfaces, rows and cards. This crate
//! lists the systems, opens the window and holds what belongs to the
//! desktop and nothing else: fonts and themes ([`rho_assets`]), the chime
//! ([`chime`]) and the D-Bus name ([`application`]).

pub mod application;
pub mod chime;
pub mod rho_assets;
pub mod telemetry;

use gpui::App;

/// Initializes the modal editor engine and the exact keymap stack. rho is
/// Helix-first: Helix is forced on so no settings file can silently drop
/// the user back into plain vim.
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

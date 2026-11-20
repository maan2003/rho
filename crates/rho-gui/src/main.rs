//! rho-gui: the one window, attached to running agent hosts.

use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use gpui::{App, AppContext as _, WindowOptions};
use rho_agent_hosts::HostSpec;
use rho_frame::{AnySystem, Frame, FrameLink};
use rho_gui::rho_assets::RhoAssets;
use settings::{RegisterSetting, Settings, SettingsContent, SettingsStore};
use tracing_subscriber::EnvFilter;

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[derive(Parser)]
#[command(
    name = "rho-gui",
    about = "Attach a native GUI to one or more running Rho agent hosts"
)]
struct Args {
    /// Attach an agent host as `<name>=local[:<runtime-dir>]` or
    /// `<name>=iroh:<endpoint-id>@<ssh-dest>`. Repeatable; the name labels
    /// the host's threads and agents once more than one is attached.
    /// Without it, the hosts attached when rho-gui last ran are attached
    /// again; with it, these replace them. Hosts attached or detached
    /// while running are remembered the same way.
    #[arg(long, value_name = "NAME=TARGET")]
    attach: Vec<String>,

    /// Rho executable on SSH hosts.
    #[arg(long, value_name = "PATH", default_value = "rho")]
    remote_rho: String,

    /// A pairing link (`rho://…`, which `rho` prints on the host) to name,
    /// attach and pair with. With a rho-gui already running, it goes to that
    /// one.
    #[arg(value_name = "LINK")]
    link: Option<String>,

    /// Write a Dial9 CPU profile with GPUI's frame and editor spans on exit,
    /// and the frames and editor stages as JSON beside it.
    #[arg(long, value_name = "FILE")]
    cpu_profile: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Read the client-only action journal.
    Journal {
        #[command(subcommand)]
        command: JournalCommand,
    },
}

#[derive(Subcommand)]
enum JournalCommand {
    /// Print faithful JSONL records after the GUI has exited.
    Dump {
        #[arg(long)]
        kind: Option<String>,
    },
}

#[derive(RegisterSetting)]
struct TextRenderingModeSetting(settings::TextRenderingMode);

impl Settings for TextRenderingModeSetting {
    fn from_settings(content: &SettingsContent) -> Self {
        Self(content.workspace.text_rendering_mode.unwrap())
    }
}

fn apply_text_rendering_mode(cx: &mut App) {
    let mode = match TextRenderingModeSetting::get_global(cx).0 {
        settings::TextRenderingMode::PlatformDefault => gpui::TextRenderingMode::PlatformDefault,
        settings::TextRenderingMode::Subpixel => gpui::TextRenderingMode::Subpixel,
        settings::TextRenderingMode::Grayscale => gpui::TextRenderingMode::Grayscale,
    };
    cx.set_text_rendering_mode(mode);
}

fn main() {
    // Chrome starts this binary as the browser extension's native host.
    if rho_browser::native_host::is_invocation(&std::env::args().collect::<Vec<_>>()) {
        if let Err(error) = rho_browser::native_host::run() {
            eprintln!("rho browser native host: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    init_tracing();
    if let Err(error) = run() {
        eprintln!("rho-gui: {error:#}");
        std::process::exit(1);
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if let Err(error) = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
    {
        eprintln!("rho-gui: failed to initialize tracing: {error}");
    }
}

fn default_client_state_dir() -> Result<PathBuf> {
    let base = dirs::state_dir().ok_or_else(|| anyhow::anyhow!("state directory not available"))?;
    Ok(base.join("rho"))
}

fn run() -> Result<()> {
    let args = Args::parse();
    let client_state_dir = default_client_state_dir()?;
    if let Some(Command::Journal {
        command: JournalCommand::Dump { kind },
    }) = args.command.as_ref()
    {
        let stdout = std::io::stdout();
        return rho_journal::dump(&client_state_dir, kind.as_deref(), stdout.lock());
    }
    // The name before the database: a second rho-gui hands its link (or, with
    // none, asks to be brought forward) to the one that runs, and one that is
    // starting already gets what is asked of it meanwhile.
    let application = match rho_gui::application::claim(args.link.clone()) {
        Ok(rho_gui::application::Claim::Ours(service)) => Some(service),
        Ok(rho_gui::application::Claim::HandedOff) => return Ok(()),
        Err(error) => {
            tracing::warn!(%error, "no {} on the session bus; links from elsewhere cannot reach this rho-gui", rho_gui::application::NAME);
            None
        }
    };
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .map_err(|_| anyhow::anyhow!("failed to install the AWS-LC rustls crypto provider"))?;
    }
    // The client keeps one database, the way the agent host does, and it is
    // opened here, before anything else: the hosts to attach are in it, and
    // everything that keeps a cache is handed it now. A second rho stops at
    // the door rather than running on with none of its own state.
    let db = rho_db::client::open_shared(&client_state_dir)
        .context("open the client database; exit the other rho first")?;
    if let Err(error) = rho_journal::init(db.clone(), rho_frame::dealer_policy_snapshot()) {
        tracing::warn!(%error, "the action journal is unavailable; this session records nothing");
    }
    rho_agent_hosts::fido::init(db.clone());
    if let Err(error) = rho_agents_client::cache::init(db.clone()) {
        tracing::warn!(%error, "the agent mirror is unavailable; this session starts from the agent host");
    }
    rho_agents_client::cache::set_state_dir(client_state_dir.clone());
    let specs = host_specs(&args, &db)?;
    let store = db.clone();
    let link = args.link.clone();
    let mut profile = args
        .cpu_profile
        .clone()
        .map(rho_gui::telemetry::Profile::start)
        .transpose()?;
    rho_gui::telemetry::enable(profile.is_none());

    gpui_platform::application()
        .with_assets(RhoAssets)
        .run(move |cx: &mut App| {
            // Windows draw on this thread.
            let draw_tid = rho_profiling::current_tid();
            if let Some(profile) = &profile {
                profile.keep_collecting(cx);
            }
            const USER_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
            cx.on_user_idle(USER_IDLE_TIMEOUT, move |event| match event {
                gpui::UserIdleEvent::Idle => {
                    rho_journal::record(rho_journal::Event::UserIdle {
                        timeout_s: USER_IDLE_TIMEOUT.as_secs(),
                    });
                }
                gpui::UserIdleEvent::Resumed => {
                    rho_journal::record(rho_journal::Event::UserResumed);
                }
            });
            cx.on_app_quit(move |_| {
                if let Some(profile) = profile.take()
                    && let Err(error) = profile.finish(draw_tid)
                {
                    eprintln!("rho-gui: failed to write the CPU profile: {error:#}");
                }
                rho_gui::telemetry::shutdown();
                // Before the tokio runtime goes with the app: a host
                // supervisor still waiting out a reconnect delay would be
                // polled into the runtime's shutdown and panic there.
                rho_agent_hosts::connection::close();
                rho_journal::flush();
                std::future::ready(())
            })
            .detach();

            if let Err(error) = init_app(cx) {
                eprintln!("rho-gui: {error:#}");
                cx.quit();
                return;
            }
            quit_on_termination_signal(cx);
            cx.activate(true);

            // Wayland compositors resolve the launcher icon by matching this
            // app_id against rho-gui.desktop.
            let window_options = WindowOptions {
                app_id: Some("rho-gui".to_owned()),
                ..Default::default()
            };
            let window = match cx.open_window(window_options, move |window, cx| {
                let handle = window
                    .window_handle()
                    .downcast::<Frame>()
                    .expect("the frame is the window's root");
                let frame = FrameLink::new(handle);
                let chat =
                    cx.new(|cx| rho_chat::ui::ChatSystem::new(db.clone(), frame.clone(), cx));
                let agents = cx
                    .new(|cx| rho_agents_view::AgentsSystem::new(chat.clone(), frame.clone(), cx));
                // The chat ranks a thread by the agents still in it and by
                // the stops they are in, and names them with what they do;
                // the agents system knows all of it, and a thread on screen
                // reads again when it moves.
                let participants = agents.downgrade();
                chat.update(cx, |chat, cx| {
                    chat.set_participants(Rc::new(move |endpoint, agent, cx| {
                        participants.upgrade().map_or_else(
                            || {
                                Some(rho_chat::ui::Participant {
                                    label: agent.encoded(),
                                    doing: None,
                                })
                            },
                            |agents| agents.read(cx).participant(endpoint, agent),
                        )
                    }));
                    cx.observe(&agents, |_, _, cx| cx.notify()).detach();
                });
                let browser = cx.new(|_| {
                    rho_browser::ui::PageSystem::new(client_state_dir.clone(), frame.clone())
                });
                let notes = cx.new(|cx| rho_notes::NoteSystem::new(frame.clone(), cx));
                let slack =
                    cx.new(|cx| rho_slack::ui::SlackSystem::new(db.clone(), frame.clone(), cx));
                let telemetry = cx.new(|_| rho_gui::telemetry::TelemetrySystem::new(frame));
                let systems = vec![
                    AnySystem::new(chat),
                    AnySystem::new(agents),
                    AnySystem::new(browser),
                    AnySystem::new(notes),
                    AnySystem::new(slack),
                    AnySystem::new(telemetry),
                ];
                let chime = rho_gui::chime::Chime;
                cx.new(|cx| {
                    Frame::new(
                        db.clone(),
                        systems,
                        specs.clone(),
                        Rc::new(move || chime.play()),
                        window,
                        cx,
                    )
                })
            }) {
                Ok(window) => window,
                Err(error) => {
                    eprintln!("rho-gui: failed to open window: {error:#}");
                    cx.quit();
                    return;
                }
            };
            match application {
                Some(service) => rho_gui::application::serve(service, window, cx),
                None => {
                    if let Some(link) = link {
                        let _ = window.update(cx, |frame, window, cx| {
                            frame.prompt_link_name(link, window, cx);
                        });
                    }
                }
            }
        });
    // The window is gone and everything it ran has written what it keeps.
    // Settled, the file opens at once on the next start instead of being
    // repaired as after a crash. Its write lock is never let go, so
    // nothing still running on another thread commits after it.
    let closed =
        futures::executor::block_on(store.settle()).context("close the client database")?;
    std::mem::forget(closed);
    Ok(())
}

fn quit_on_termination_signal(cx: &mut App) {
    let signal = gpui_tokio::Tokio::spawn(cx, async {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let signal = tokio::select! {
            _ = terminate.recv() => "SIGTERM",
            _ = interrupt.recv() => "SIGINT",
        };
        Ok::<_, std::io::Error>(signal)
    });
    cx.spawn(async move |cx| match signal.await {
        Ok(Ok(signal)) => {
            tracing::info!(signal, "termination signal received; quitting");
            cx.update(|cx| cx.quit());
        }
        Ok(Err(error)) => {
            tracing::error!(%error, "failed to register termination signal handler")
        }
        Err(error) => tracing::error!(%error, "termination signal handler task failed"),
    })
    .detach();
}

fn host_specs(args: &Args, db: &rho_db::RhoDb) -> Result<Vec<HostSpec>> {
    if args.attach.is_empty() {
        return Ok(rho_agent_hosts::saved::load(db));
    }
    let mut specs = Vec::new();
    for host in &args.attach {
        let spec = HostSpec::parse(host, &args.remote_rho)
            .map_err(|error| anyhow::anyhow!("--attach {host}: {error}"))?;
        anyhow::ensure!(
            !specs.iter().any(|other: &HostSpec| other.name == spec.name),
            "host {:?} is attached twice; names label threads and must be distinct",
            spec.name
        );
        specs.push(spec);
    }
    Ok(specs)
}

fn init_app(cx: &mut App) -> Result<()> {
    gpui_tokio::init(cx);
    RhoAssets.load_fonts(cx)?;
    let settings_path = settings_path()?;
    let user_settings = load_or_create_settings(&settings_path)?;
    let mut store = SettingsStore::new(cx, &rho_gui::rho_assets::default_settings());
    store
        .set_user_settings(&user_settings, cx)
        .result()
        .with_context(|| format!("failed to load settings from {}", settings_path.display()))?;
    cx.set_global(store);
    apply_text_rendering_mode(cx);
    cx.observe_global::<SettingsStore>(apply_text_rendering_mode)
        .detach();
    theme_settings::init(theme::LoadThemes::All(Box::new(RhoAssets)), cx);
    release_channel::init(semver::Version::new(0, 1, 0), cx);
    editor::init(cx);
    search::init(cx);
    rho_gui::init_vim_mode(cx).context("failed to initialize Rho Vim mode")?;
    Ok(())
}

const DEFAULT_SETTINGS: &str = r#"// Rho GUI user settings. Values here override bundled defaults.
{
  "theme": "Rho OKSolar P3"
}
"#;

fn settings_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir().context("config directory not available")?;
    Ok(config_dir.join("rho-gui").join("settings.json"))
}

fn load_or_create_settings(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(settings) => Ok(settings),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create settings directory {}", parent.display())
                })?;
            }
            fs::write(path, DEFAULT_SETTINGS).with_context(|| {
                format!("failed to write default settings to {}", path.display())
            })?;
            Ok(DEFAULT_SETTINGS.to_owned())
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to read settings from {}", path.display()))
        }
    }
}

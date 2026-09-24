use clap::Parser as _;

#[derive(clap::Parser)]
#[command(name = "rho-agent-host", about = "Run the rho GUI daemon")]
struct Args {
    #[command(flatten)]
    daemon: rho_agent_host::DaemonArgs,
}

fn main() {
    let args = Args::parse();
    init_tracing();
    rho_agent_host::configure_embedded_environment();
    let mut daemon_args = args.daemon;
    let result = (|| {
        let profiler = rho_agent_host::DaemonProfiler::start(&mut daemon_args)?;
        let runtime = tokio::runtime::Runtime::new()?;
        let result = runtime.block_on(rho_agent_host::run(daemon_args));
        drop(runtime);
        profiler.finish(result)
    })();
    if let Err(error) = result {
        eprintln!("rho-agent-host: {error:#}");
        std::process::exit(1);
    }
}

/// The daemon's own output, so what it says about itself is kept.
///
/// It said nothing until now: nothing in this binary installed a
/// subscriber, so every `tracing::info!` in the daemon — including the
/// one-shot conversions' reports of what they did to the user's store —
/// was written to a subscriber that did not exist and was lost. Under
/// systemd stderr is what journald keeps, so that is where this writes,
/// and the level rule is rho-gui's: `RUST_LOG` when it is set, info when
/// it is not.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if let Err(error) = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
    {
        eprintln!("rho-agent-host: failed to initialize tracing: {error}");
    }
}

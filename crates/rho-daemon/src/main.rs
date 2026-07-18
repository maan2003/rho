use clap::Parser as _;

#[derive(clap::Parser)]
#[command(name = "rho-daemon", about = "Run the rho GUI daemon")]
struct Args {
    #[command(flatten)]
    daemon: rho_daemon::DaemonArgs,
}

fn main() {
    let args = Args::parse();
    // SAFETY: top of main — no threads exist yet and nothing has captured
    // pre-namespace state.
    unsafe { rho_daemon::init_daemon_namespace() }.expect("set up daemon namespace");
    let mut daemon_args = args.daemon;
    let result = (|| {
        let profiler = rho_daemon::DaemonProfiler::start(&mut daemon_args)?;
        let runtime = tokio::runtime::Runtime::new()?;
        let result = runtime.block_on(rho_daemon::run(daemon_args));
        drop(runtime);
        profiler.finish(result)
    })();
    if let Err(error) = result {
        eprintln!("rho-daemon: {error:#}");
        std::process::exit(1);
    }
}

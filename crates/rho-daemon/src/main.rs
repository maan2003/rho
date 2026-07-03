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
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("rho-daemon: failed to start async runtime: {error:#}");
            std::process::exit(1);
        }
    };
    if let Err(error) = runtime.block_on(rho_daemon::run(args.daemon)) {
        eprintln!("rho-daemon: {error:#}");
        std::process::exit(1);
    }
}

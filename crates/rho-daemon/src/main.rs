use clap::Parser as _;

#[derive(clap::Parser)]
#[command(name = "rho-daemon", about = "Run the rho GUI daemon")]
struct Args {
    #[command(flatten)]
    daemon: rho_daemon::DaemonArgs,
}

fn main() {
    let args = Args::parse();
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

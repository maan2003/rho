fn main() {
    if let Err(error) = rho_agent::worker_main() {
        eprintln!("rho-agent-worker: {error:#}");
        std::process::exit(1);
    }
}

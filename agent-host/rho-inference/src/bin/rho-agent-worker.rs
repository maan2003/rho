fn main() {
    if std::env::args().nth(1).as_deref() == Some("--notebook-guardian") {
        if let Err(error) = rho_agent::worker::native::process::notebook_guardian_main() {
            eprintln!("rho-notebook-guardian: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("--notebook-worker") {
        if let Err(error) = rho_agent::worker::native::process::notebook_worker_main() {
            eprintln!("rho-notebook-worker: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(error) = rho_inference::worker_main() {
        eprintln!("rho-agent-worker: {error:#}");
        std::process::exit(1);
    }
}

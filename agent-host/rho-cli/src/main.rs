fn main() -> std::process::ExitCode {
    match rho_cli::main() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rho: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

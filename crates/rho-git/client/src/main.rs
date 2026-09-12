//! `git`, as agents see it: the mirror store's client in front of the real
//! git (`rho_git_client::wrapper`).

fn main() {
    let args = std::env::args_os().skip(1).collect();
    match rho_git_client::wrapper::run(args) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("rho-git: {error:#}");
            std::process::exit(128);
        }
    }
}

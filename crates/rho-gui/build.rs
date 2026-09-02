fn main() {
    if let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", "HEAD"])
        .output()
        && output.status.success()
        && let Ok(head) = String::from_utf8(output.stdout)
    {
        println!("cargo:rerun-if-changed={}", head.trim());
    }
    if let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        && output.status.success()
        && let Ok(commit) = String::from_utf8(output.stdout)
    {
        println!("cargo:rustc-env=RHO_BUILD_GIT_COMMIT={}", commit.trim());
    }

    for name in ["PROFILE", "OPT_LEVEL", "TARGET"] {
        println!(
            "cargo:rustc-env=RHO_BUILD_{name}={}",
            std::env::var(name).unwrap_or_else(|_| "unknown".to_owned())
        );
    }
}

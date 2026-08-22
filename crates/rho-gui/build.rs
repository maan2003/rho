fn main() {
    for name in ["PROFILE", "OPT_LEVEL", "TARGET"] {
        println!(
            "cargo:rustc-env=RHO_BUILD_{name}={}",
            std::env::var(name).unwrap_or_else(|_| "unknown".to_owned())
        );
    }
}

//! Harness-free execution tests: parent owns scratch directories; child
//! establishes its workset before Tokio, just like the production companion.
pub fn run<F: std::future::Future<Output = ()>>(bashrc: &str, test: impl FnOnce() -> F) {
    let args = std::env::args_os().collect::<Vec<_>>();
    if args.get(1).is_some_and(|arg| arg == "--workset-test") {
        let bytes = std::fs::read(&args[2]).unwrap();
        let layout: rho_fs_view::WorksetLayout =
            senax_encoder::decode(&mut bytes.as_slice()).unwrap();
        unsafe {
            layout.build().unwrap();
            layout.enter().unwrap()
        };
        tokio::runtime::Runtime::new().unwrap().block_on(test());
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let path = |name| camino::Utf8PathBuf::from_path_buf(temp.path().join(name)).unwrap();
    for directory in ["work", "store", "cache", "state", "home"] {
        std::fs::create_dir(path(directory)).unwrap();
    }
    std::fs::write(path("home").join(".bashrc"), bashrc).unwrap();
    let layout = rho_fs_view::WorksetLayout {
        workset: "test".into(),
        source: path("work"),
        store_root: path("store"),
        store_socket: None,
        cache: path("cache"),
        state: path("state"),
        paths: Default::default(),
    };
    std::fs::write(path("layout"), senax_encoder::encode(&layout).unwrap()).unwrap();
    // Exposed worksets inherit HOME, and with it the test's shell startup.
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--workset-test")
        .arg(path("layout"))
        .env("HOME", path("home"))
        .status()
        .unwrap();
    assert!(status.success(), "workset execution test failed");
}

use std::path::Path;

use tokio::process::Command;

fn main() {
    // These namespace-first binaries cannot use libtest, but nextest still
    // needs a libtest-compatible listing to run and time each binary.
    if std::env::args().any(|arg| arg == "--list") {
        if !std::env::args().any(|arg| arg == "--ignored") {
            println!("e2e: test");
        }
        return;
    }
    if !std::process::Command::new("unshare")
        .args(["-U", "true"])
        .status()
        .is_ok_and(|status| status.success())
    {
        eprintln!("skipping Claude namespace test: kernel forbids user namespaces");
        return;
    }
    // The test process represents a workset before its executor starts.
    rho_fs_view::layout::unshare_identity_user_namespace().unwrap();
    rho_fs_view::layout::unshare_mount_namespace().unwrap();
    let args = std::env::args_os().collect::<Vec<_>>();
    if args.get(1).is_some_and(|arg| arg == "--sources") {
        let paths = rho_claude::accounts::ClaudePaths::at(
            camino::Utf8PathBuf::from_path_buf(std::path::PathBuf::from(&args[2])).unwrap(),
        );
        let root = Path::new(&args[3]);
        let mounted = rho_claude::namespace::install_sources(
            &paths,
            root,
            Path::new("/state"),
            paths.config_home().to_owned(),
        )
        .unwrap();
        std::fs::write(
            root.join(mounted.projects().strip_prefix("/").unwrap())
                .join("transcript"),
            "shared",
        )
        .unwrap();
        return;
    }
    // Parent owns scratch paths; the mount child exits before their cleanup.
    let source = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let paths = rho_claude::accounts::ClaudePaths::at(
        camino::Utf8PathBuf::from_path_buf(source.path().join("fresh")).unwrap(),
    );
    assert!(
        std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--sources")
            .arg(paths.config_home())
            .arg(root.path())
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        std::fs::read_to_string(paths.projects().join("transcript")).unwrap(),
        "shared"
    );
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(run());
}

async fn run() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let target = root.join("config");
    let projects = root.join("projects");
    std::fs::create_dir(&target).unwrap();
    std::fs::create_dir(&projects).unwrap();
    std::fs::write(target.join("CLAUDE.md"), "terminal").unwrap();
    let mut children = Vec::new();
    for label in ["one", "two"] {
        let account = root.join(label);
        std::fs::create_dir_all(account.join("projects")).unwrap();
        std::fs::write(account.join("CLAUDE.md"), "original").unwrap();
        std::fs::write(account.join("settings.json"), "{}").unwrap();
        let prompt = root.join(format!("{label}.md"));
        let settings = root.join(format!("{label}.json"));
        std::fs::write(&prompt, label).unwrap();
        std::fs::write(&settings, label).unwrap();
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "cat \"$1/CLAUDE.md\" \"$1/settings.json\"; echo shared > \"$1/projects/written\"",
            "sh",
            target.to_str().unwrap(),
        ]);
        command.stdout(std::process::Stdio::piped());
        rho_claude::namespace::prepare(
            &mut command,
            &target,
            &account,
            &projects,
            &prompt,
            Some(&settings),
        )
        .unwrap();
        children.push((label, command.spawn().unwrap()));
        assert_eq!(
            std::fs::read_to_string(target.join("CLAUDE.md")).unwrap(),
            "terminal"
        );
    }
    for (label, child) in children {
        let output = child.wait_with_output().await.unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{label}{label}")
        );
    }
    assert_eq!(
        std::fs::read_to_string(projects.join("written")).unwrap(),
        "shared\n"
    );
    assert_eq!(
        std::fs::read_to_string(target.join("CLAUDE.md")).unwrap(),
        "terminal"
    );

    let mut command = Command::new("sh");
    let marker = root.join("must-not-exec");
    command.args(["-c", "touch \"$1\"", "sh", marker.to_str().unwrap()]);
    rho_claude::namespace::prepare(
        &mut command,
        Path::new("/proc/cpuinfo"),
        &root.join("one"),
        &projects,
        &root.join("one.md"),
        None,
    )
    .unwrap();
    assert!(command.spawn().is_err());
    assert!(!marker.exists());
    println!("Claude private namespaces passed");
}

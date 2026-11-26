//! End-to-end test for `rho wayland record` against a real headless compositor.
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result, ensure};
use rho_desktop_media::webm::verify_webm_decodable;

struct SessionGuard {
    rho: PathBuf,
    runtime: PathBuf,
    state_dir: PathBuf,
    session: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = Command::new(&self.rho)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .args([
                "wayland",
                "--session",
                &self.session,
                "--state-dir",
                self.state_dir.to_str().unwrap(),
                "record",
                "stop",
            ])
            .output();
        let _ = Command::new(&self.rho)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .args([
                "wayland",
                "--session",
                &self.session,
                "--state-dir",
                self.state_dir.to_str().unwrap(),
                "stop",
            ])
            .output();
    }
}

#[test]
fn records_background_and_duration_webm_from_headless_wayland() -> Result<()> {
    let rho = PathBuf::from(env!("CARGO_BIN_EXE_rho"));
    let temp = tempfile::tempdir()?;
    let runtime = temp.path().join("rt");
    std::fs::create_dir(&runtime)?;
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))?;
    let state_dir = temp.path().join("state");
    let session = "record-e2e".to_owned();

    let _guard = SessionGuard {
        rho: rho.clone(),
        runtime: runtime.clone(),
        state_dir: state_dir.clone(),
        session: session.clone(),
    };

    let start = Command::new(&rho)
        .env("XDG_RUNTIME_DIR", &runtime)
        .args([
            "wayland",
            "--session",
            &session,
            "--state-dir",
            state_dir.to_str().unwrap(),
            "start",
            "--width",
            "320",
            "--height",
            "240",
            "--scale",
            "1",
        ])
        .output()
        .context("start headless wayland session")?;
    ensure!(
        start.status.success(),
        "wayland start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    // 1. Background record start / move / click / record stop.
    let bg_out = temp.path().join("flow.webm");
    let rec_start = Command::new(&rho)
        .env("XDG_RUNTIME_DIR", &runtime)
        .args([
            "wayland",
            "--session",
            &session,
            "--state-dir",
            state_dir.to_str().unwrap(),
            "record",
            "start",
            "--output",
            bg_out.to_str().unwrap(),
            "--fps",
            "10",
        ])
        .output()?;
    ensure!(
        rec_start.status.success(),
        "record start failed: {}",
        String::from_utf8_lossy(&rec_start.stderr)
    );

    let mv = Command::new(&rho)
        .env("XDG_RUNTIME_DIR", &runtime)
        .args([
            "wayland",
            "--session",
            &session,
            "--state-dir",
            state_dir.to_str().unwrap(),
            "move",
            "120",
            "100",
        ])
        .output()?;
    ensure!(mv.status.success(), "wayland move failed");
    std::thread::sleep(std::time::Duration::from_millis(450));

    let rec_stop = Command::new(&rho)
        .env("XDG_RUNTIME_DIR", &runtime)
        .args([
            "wayland",
            "--session",
            &session,
            "--state-dir",
            state_dir.to_str().unwrap(),
            "record",
            "stop",
        ])
        .output()?;
    ensure!(
        rec_stop.status.success(),
        "record stop failed: {}",
        String::from_utf8_lossy(&rec_stop.stderr)
    );

    let bg_bytes = std::fs::read(&bg_out)?;
    let (bg_info, bg_decoded) = verify_webm_decodable(&bg_bytes)?;
    assert_eq!(bg_info.doc_type, "webm");
    assert_eq!(bg_info.codec_id, "V_VP9");
    assert_eq!((bg_info.pixel_width, bg_info.pixel_height), (320, 240));
    assert!(bg_info.duration_ms > 0.0);
    assert!(
        bg_info.blocks.len() >= 3 && bg_decoded == bg_info.blocks.len(),
        "expected >=3 decoded frames, got {bg_decoded}/{}",
        bg_info.blocks.len()
    );
    assert!(bg_info.blocks[0].1, "first frame must be a keyframe");

    // 2. Foreground fixed-duration capture.
    let fg_out = temp.path().join("duration.webm");
    let fg = Command::new(&rho)
        .env("XDG_RUNTIME_DIR", &runtime)
        .args([
            "wayland",
            "--session",
            &session,
            "--state-dir",
            state_dir.to_str().unwrap(),
            "record",
            "--output",
            fg_out.to_str().unwrap(),
            "--duration",
            "0.6",
            "--fps",
            "10",
        ])
        .output()?;
    ensure!(
        fg.status.success(),
        "foreground record failed: {}",
        String::from_utf8_lossy(&fg.stderr)
    );

    let fg_bytes = std::fs::read(&fg_out)?;
    let (fg_info, fg_decoded) = verify_webm_decodable(&fg_bytes)?;
    assert_eq!(fg_info.doc_type, "webm");
    assert_eq!(fg_info.codec_id, "V_VP9");
    assert_eq!((fg_info.pixel_width, fg_info.pixel_height), (320, 240));
    assert!(
        fg_info.blocks.len() >= 4 && fg_decoded == fg_info.blocks.len(),
        "expected >=4 decoded frames, got {fg_decoded}/{}",
        fg_info.blocks.len()
    );
    Ok(())
}

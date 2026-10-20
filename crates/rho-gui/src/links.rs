//! Pairing links (`rho://…`) opened from outside: the desktop hands one to
//! a new `rho-gui`, and only one rho-gui may run, so a second one gives its
//! link to the first through a socket in the state directory and exits.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use futures::StreamExt as _;
use gpui::App;

fn socket(state_dir: &Path) -> PathBuf {
    state_dir.join("rho-gui-links.sock")
}

/// Gives `link` to the rho-gui already running, if one is. False when none
/// listens, and this process is the rho-gui to open it.
pub fn hand_off(state_dir: &Path, link: &str) -> bool {
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket(state_dir)) else {
        return false;
    };
    writeln!(stream, "{}", link.trim()).is_ok()
}

/// Listens for links from later rho-gui processes and gives each to
/// `open`. Only the one rho-gui, which holds the client database, calls
/// it, so a socket file left behind is a dead one's.
pub fn listen(state_dir: &Path, open: impl Fn(String, &mut App) + 'static, cx: &mut App) {
    let path = socket(state_dir);
    let _ = std::fs::remove_file(&path);
    let (sender, mut links) = futures::channel::mpsc::unbounded::<String>();
    gpui_tokio::Tokio::spawn(cx, async move {
        use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _};
        let listener = match tokio::net::UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "links from other processes cannot reach this rho-gui");
                return;
            }
        };
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        while let Ok((stream, _)) = listener.accept().await {
            let mut line = String::new();
            // One short line; a link is a few hundred bytes.
            let mut reader = tokio::io::BufReader::new(stream).take(4096);
            if reader.read_line(&mut line).await.is_ok() && !line.trim().is_empty() {
                let _ = sender.unbounded_send(line.trim().to_owned());
            }
        }
    })
    .detach();
    cx.spawn(async move |cx| {
        while let Some(link) = links.next().await {
            cx.update(|cx| open(link, cx));
        }
    })
    .detach();
}

//! The Unix sockets at the workset's ends of GCCP byte streams.
//!
//! A byte stream ([docs/gccp.md](../../../../docs/gccp.md)) joins a socket
//! of one clanker's to a socket of another's. Both sockets are the
//! workset's: it is the workset process that connects to a service an agent
//! opened, and that listens where an agent wants a service of another
//! host's to appear, so a socket is reached as the agents reach it, in the
//! workset's view of the files. Each connected socket then goes to the
//! host as a descriptor ([`crate::ipc::transport::Sender::send_with_fd`]),
//! and the host carries the bytes between it and the other harness: no
//! byte of a stream passes through this process.

use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use futures::future::BoxFuture;
use tokio::net::{UnixListener, UnixStream};

pub(crate) struct Streams {
    /// The workset's root. A service is a socket under it.
    root: PathBuf,
    /// The forwards, by the path each listens at.
    forwards: Mutex<HashMap<PathBuf, tokio::task::JoinHandle<()>>>,
}

/// Gives the host a connection that a forward accepted, to join to a
/// stream. The host closes one that it gets no stream for, which is all
/// its client learns.
pub(crate) type Open = Arc<dyn Fn(OwnedFd) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

/// A connected socket as the descriptor the host gets.
fn descriptor(socket: UnixStream) -> io::Result<OwnedFd> {
    Ok(socket.into_std()?.into())
}

impl Streams {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            forwards: Mutex::default(),
        }
    }

    /// `path` with its links followed, if that is in the workset. The file
    /// itself need not be there yet; its directory must.
    ///
    /// A service is something of the agent's own. This keeps a socket of
    /// the machine's, such as the host's own, from being opened to another
    /// host by its path alone, through a link too. It is a check on a
    /// mistake and no more: agents are not sandboxed, and one that means to
    /// can carry bytes anywhere.
    pub(crate) fn inside(&self, path: &Path) -> anyhow::Result<PathBuf> {
        let shown = path.display();
        anyhow::ensure!(path.is_absolute(), "{shown} is not an absolute path");
        let resolved = match path.canonicalize() {
            Ok(resolved) => resolved,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let (Some(directory), Some(name)) = (path.parent(), path.file_name()) else {
                    anyhow::bail!("{shown} names no file");
                };
                directory
                    .canonicalize()
                    .with_context(|| format!("the directory of {shown}"))?
                    .join(name)
            }
            Err(error) => return Err(error).with_context(|| shown.to_string()),
        };
        let root = self
            .root
            .canonicalize()
            .with_context(|| self.root.display().to_string())?;
        anyhow::ensure!(
            resolved.starts_with(&root),
            "{shown} is not in the workset ({})",
            root.display()
        );
        Ok(resolved)
    }

    /// Connects to the service socket at `path`, for the host.
    pub(crate) async fn service(&self, path: &str) -> anyhow::Result<OwnedFd> {
        let resolved = self.inside(Path::new(path))?;
        let socket = UnixStream::connect(&resolved)
            .await
            .with_context(|| format!("connecting to {path}"))?;
        Ok(descriptor(socket)?)
    }

    /// Listens at `path`, and gives each connection to `open`.
    /// A forward that listened there already ends, and so does a socket
    /// file that nothing listens at: the forward of a workset process that
    /// is gone leaves one.
    pub(crate) fn forward(&self, path: &Path, open: Open) -> anyhow::Result<()> {
        let shown = path.display();
        anyhow::ensure!(path.is_absolute(), "{shown} is not an absolute path");
        let mut forwards = self.forwards.lock().expect("poison");
        if let Some(replaced) = forwards.remove(path) {
            replaced.abort();
            let _ = std::fs::remove_file(path);
        } else if let Err(error) = std::os::unix::net::UnixStream::connect(path)
            && error.kind() == io::ErrorKind::ConnectionRefused
        {
            let _ = std::fs::remove_file(path);
        }
        let listener = UnixListener::bind(path).with_context(|| format!("listening at {shown}"))?;
        let at = path.to_owned();
        let accepting = tokio::spawn(async move {
            loop {
                let accepted = listener.accept().await;
                let socket = match accepted.and_then(|(socket, _)| descriptor(socket)) {
                    Ok(socket) => socket,
                    Err(error) => {
                        eprintln!("rho-agent: forward at {}: {error}", at.display());
                        // Out of file descriptors, most likely: not at once
                        // again.
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        continue;
                    }
                };
                let (open, at) = (open.clone(), at.clone());
                tokio::spawn(async move {
                    if let Err(error) = open(socket).await {
                        eprintln!("rho-agent: forward at {}: {error:#}", at.display());
                    }
                });
            }
        });
        forwards.insert(path.to_owned(), accepting);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A service is a socket in the workset, however its path gets there:
    /// a path that leaves the workset, straight or through a link, is
    /// refused, and a link that stays in it is followed.
    #[test]
    fn a_service_is_a_socket_in_the_workset() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workset");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("host.sock"), b"").unwrap();
        std::fs::write(root.join("sub/own.sock"), b"").unwrap();
        std::os::unix::fs::symlink(outside.join("host.sock"), root.join("link.sock")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("away")).unwrap();
        std::os::unix::fs::symlink(root.join("sub"), root.join("near")).unwrap();
        let streams = Streams::new(&root);
        let root = root.canonicalize().unwrap();

        for own in [
            "sub/own.sock",
            "sub/later.sock",
            "near/own.sock",
            "sub/../sub/own.sock",
        ] {
            let resolved = streams.inside(&root.join(own)).unwrap();
            assert!(
                resolved.starts_with(root.join("sub")),
                "{own}: {resolved:?}"
            );
        }
        for other in [
            outside.join("host.sock"),
            root.join("link.sock"),
            root.join("away/host.sock"),
            root.join("away/later.sock"),
            root.join("../outside/host.sock"),
            root.join("missing/later.sock"),
            PathBuf::from("sub/own.sock"),
        ] {
            assert!(streams.inside(&other).is_err(), "{other:?}");
        }
    }

    /// A forward gives each connection it accepts to the host, as the
    /// socket itself. A connection the host drops closes. The path is taken
    /// over from a forward that is replaced and from one that died, and not
    /// from someone who listens.
    #[tokio::test]
    async fn a_forward_gives_each_connection_to_the_host() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("there.sock");
        let streams = Arc::new(Streams::new(dir.path()));
        let (opened, mut opens) = tokio::sync::mpsc::unbounded_channel();
        let open = |label: &'static str| -> Open {
            let opened = opened.clone();
            Arc::new(move |socket| {
                let _ = opened.send((label, socket));
                Box::pin(async { Ok(()) })
            })
        };

        // Somebody else listens: the path is theirs.
        let theirs = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(streams.forward(&path, open("first")).is_err());
        // They died and left the file.
        drop(theirs);
        assert!(path.exists());
        streams.forward(&path, open("first")).unwrap();

        // What the host gets is the other end of the client's connection.
        let mut client = UnixStream::connect(&path).await.unwrap();
        let (label, socket) = opens.recv().await.unwrap();
        assert_eq!(label, "first");
        let socket = std::os::unix::net::UnixStream::from(socket);
        let mut hosts = UnixStream::from_std(socket).unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut ping = [0; 4];
        hosts.read_exact(&mut ping).await.unwrap();
        assert_eq!(&ping, b"ping");
        // The host is all that holds it: dropped there, it closes here.
        drop(hosts);
        assert_eq!(client.read(&mut ping).await.unwrap(), 0);

        // Replaced: connections go to the new forward only.
        streams.forward(&path, open("second")).unwrap();
        let _client = UnixStream::connect(&path).await.unwrap();
        assert_eq!(opens.recv().await.unwrap().0, "second");
    }

    /// The socket of a service is connected for the host, and only if its
    /// path is in the workset.
    #[tokio::test]
    async fn a_service_is_connected_for_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workset");
        std::fs::create_dir_all(&root).unwrap();
        let inside = UnixListener::bind(root.join("web.sock")).unwrap();
        let _outside = UnixListener::bind(dir.path().join("host.sock")).unwrap();
        let streams = Streams::new(&root);
        let path = |path: std::path::PathBuf| path.to_str().unwrap().to_owned();

        streams.service(&path(root.join("web.sock"))).await.unwrap();
        inside.accept().await.unwrap();
        assert!(
            streams
                .service(&path(dir.path().join("host.sock")))
                .await
                .is_err()
        );
        assert!(
            streams
                .service(&path(root.join("later.sock")))
                .await
                .is_err()
        );
    }
}

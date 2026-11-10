//! The Unix sockets at the workset's ends of GCCP byte streams.
//!
//! A byte stream ([docs/gccp.md](../../../../docs/gccp.md)) joins a socket
//! of one clanker's to a socket of another's. Both sockets are the
//! workset's: it is the workset process that connects to a service an agent
//! opened, and that listens where an agent wants a service of another
//! host's to appear. The host only carries the bytes
//! ([`crate::ipc::stream`]), so a socket is reached as the agents reach it,
//! in the workset's view of the files.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use futures::future::BoxFuture;
use tokio::net::{UnixListener, UnixStream};

pub(crate) struct Streams {
    /// The workset's root. A service is a socket under it.
    root: PathBuf,
    /// The forwards, by the path each listens at.
    forwards: Mutex<HashMap<PathBuf, tokio::task::JoinHandle<()>>>,
    /// The connections the forwards accepted, each until the host has a
    /// stream for it.
    accepted: Mutex<HashMap<u64, UnixStream>>,
    next: AtomicU64,
}

/// Asks the host for a stream for the accepted connection of that id.
pub(crate) type Open = Arc<dyn Fn(u64) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

impl Streams {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            forwards: Mutex::default(),
            accepted: Mutex::default(),
            next: AtomicU64::new(1),
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

    /// Connects to the service socket at `path`.
    pub(crate) async fn service(&self, path: &str) -> anyhow::Result<UnixStream> {
        let resolved = self.inside(Path::new(path))?;
        UnixStream::connect(&resolved)
            .await
            .with_context(|| format!("connecting to {path}"))
    }

    /// Listens at `path`, and has `open` get a stream for each connection.
    /// A forward that listened there already ends, and so does a socket
    /// file that nothing listens at: the forward of a workset process that
    /// is gone leaves one.
    pub(crate) fn forward(self: &Arc<Self>, path: &Path, open: Open) -> anyhow::Result<()> {
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
        let streams = self.clone();
        let at = path.to_owned();
        let accepting = tokio::spawn(async move {
            loop {
                let socket = match listener.accept().await {
                    Ok((socket, _)) => socket,
                    Err(error) => {
                        eprintln!("rho-agent: forward at {}: {error}", at.display());
                        // Out of file descriptors, most likely: not at once
                        // again.
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        continue;
                    }
                };
                let conn = streams.next.fetch_add(1, Ordering::Relaxed);
                streams
                    .accepted
                    .lock()
                    .expect("poison")
                    .insert(conn, socket);
                let (streams, open, at) = (streams.clone(), open.clone(), at.clone());
                tokio::spawn(async move {
                    let opened = open(conn).await;
                    // Taken if the host attached a stream. Otherwise it
                    // closes here, which is all its client learns.
                    streams.accepted.lock().expect("poison").remove(&conn);
                    if let Err(error) = opened {
                        eprintln!("rho-agent: forward at {}: {error:#}", at.display());
                    }
                });
            }
        });
        forwards.insert(path.to_owned(), accepting);
        Ok(())
    }

    /// The connection a forward accepted under `conn`, once.
    pub(crate) fn take(&self, conn: u64) -> anyhow::Result<UnixStream> {
        self.accepted
            .lock()
            .expect("poison")
            .remove(&conn)
            .context("the connection is gone")
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

    /// A forward hands each connection it accepts to the host under an id
    /// of its own, which takes it once. A connection the host got no stream
    /// for closes. The path is taken over from a forward that is replaced
    /// and from one that died, and not from someone who listens.
    #[tokio::test]
    async fn a_forward_gives_each_connection_to_the_host_once() {
        use tokio::io::AsyncReadExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("there.sock");
        let streams = Arc::new(Streams::new(dir.path()));
        let (opened, mut opens) = tokio::sync::mpsc::unbounded_channel();
        let open = |label: &'static str, attach: bool| -> Open {
            let opened = opened.clone();
            Arc::new(move |conn| {
                let _ = opened.send((label, conn));
                Box::pin(async move {
                    anyhow::ensure!(attach, "refused");
                    Ok(())
                })
            })
        };

        // Somebody else listens: the path is theirs.
        let theirs = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(streams.forward(&path, open("first", true)).is_err());
        // They died and left the file.
        drop(theirs);
        assert!(path.exists());
        streams.forward(&path, open("first", false)).unwrap();

        // No stream: the client's connection closes.
        let mut client = UnixStream::connect(&path).await.unwrap();
        let (label, conn) = opens.recv().await.unwrap();
        assert_eq!(label, "first");
        assert_eq!(client.read(&mut [0; 1]).await.unwrap(), 0);
        assert!(streams.take(conn).is_err());

        // Replaced: connections go to the new forward only.
        streams.forward(&path, open("second", true)).unwrap();
        let _client = UnixStream::connect(&path).await.unwrap();
        let (label, second) = opens.recv().await.unwrap();
        assert_eq!(label, "second");
        assert_ne!(second, conn);
    }
}

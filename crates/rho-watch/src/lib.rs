//! The workset process's filesystem watches over repositories: one kernel
//! inotify instance shared by everything that caches something derived from
//! files, e.g. dev shells and the environments activated from them.
//!
//! A [`Subscription`] covers a set of paths and becomes stale when any of
//! them may have changed. Checking it drains the kernel's queue first, so a
//! change made before the check is always seen; a thread drains the queue
//! meanwhile so that it never overflows while nobody checks. Staleness is
//! conservative: an overflowing queue, or a watched directory going away,
//! makes every affected subscription stale.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rustix::fs::inotify::{self, ReadFlags, WatchFlags};

/// Paths one subscription may cover, symlink targets included.
pub const MAX_PATHS: usize = 4096;

/// Symlinks followed from one path.
const MAX_LINKS: usize = 64;

/// A process's watches. Cloning shares them.
#[derive(Clone)]
pub struct Watcher(Arc<Inner>);

struct Inner {
    fd: OwnedFd,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    next: u64,
    /// For each kernel watch, what each subscription wants from it.
    watches: HashMap<i32, HashMap<u64, Interest>>,
    subscriptions: HashMap<u64, Subscribed>,
}

struct Subscribed {
    stale: Arc<AtomicBool>,
    watches: HashSet<i32>,
}

/// What a subscription wants from one kernel watch: any event, or events
/// on some entries of a watched directory. Events on the watched inode
/// itself (deleted, moved, unmounted) always count.
#[derive(Default)]
struct Interest {
    all: bool,
    names: HashSet<OsString>,
}

impl Watcher {
    /// New watches, drained by a thread for the life of the process.
    pub fn new() -> io::Result<Self> {
        let fd = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)?;
        let inner = Arc::new(Inner {
            fd,
            state: Mutex::default(),
        });
        let drained = inner.clone();
        std::thread::Builder::new()
            .name("rho-watch".into())
            .spawn(move || drain_forever(&drained))?;
        Ok(Self(inner))
    }

    /// The process's watcher.
    pub fn global() -> io::Result<Self> {
        static GLOBAL: OnceLock<Watcher> = OnceLock::new();
        if let Some(watcher) = GLOBAL.get() {
            return Ok(watcher.clone());
        }
        let watcher = Self::new()?;
        Ok(GLOBAL.get_or_init(|| watcher).clone())
    }

    /// Watch `contents` for any change, and `names` for being created,
    /// replaced or removed, or having their attributes changed, but not for
    /// what a directory among them contains. Each path's ancestors are
    /// watched for replacement, and symlinks along it are followed.
    pub fn watch<'a>(
        &self,
        contents: impl IntoIterator<Item = &'a Path>,
        names: impl IntoIterator<Item = &'a Path>,
    ) -> io::Result<Subscription> {
        let mut state = self.0.state.lock().unwrap();
        // Queued events predate this subscription.
        self.0.drain(&mut state)?;
        let id = state.next;
        state.next += 1;
        let stale = Arc::new(AtomicBool::new(false));
        state.subscriptions.insert(
            id,
            Subscribed {
                stale: stale.clone(),
                watches: HashSet::new(),
            },
        );
        let subscription = Subscription {
            watcher: self.clone(),
            id,
            stale,
        };
        let mut adding = Adding {
            fd: &self.0.fd,
            state: &mut state,
            id,
            seen: HashSet::new(),
        };
        for path in contents {
            adding.path(path, true, 0)?;
        }
        for path in names {
            adding.path(path, false, 0)?;
        }
        drop(state);
        Ok(subscription)
    }

    /// Account for every change made so far.
    pub fn sync(&self) -> io::Result<()> {
        self.0.drain(&mut self.0.state.lock().unwrap())
    }
}

/// Paths some cached result was derived from.
pub struct Subscription {
    watcher: Watcher,
    id: u64,
    stale: Arc<AtomicBool>,
}

impl Subscription {
    /// Whether any watched path may have changed since the subscription
    /// was made. Once stale, always stale.
    pub fn changed(&self) -> io::Result<bool> {
        if !self.stale.load(Ordering::Acquire) {
            self.watcher.sync()?;
        }
        Ok(self.stale.load(Ordering::Acquire))
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription").field("id", &self.id).finish_non_exhaustive()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut state = self.watcher.0.state.lock().unwrap();
        let Some(subscribed) = state.subscriptions.remove(&self.id) else { return };
        for wd in subscribed.watches {
            let Some(interests) = state.watches.get_mut(&wd) else { continue };
            interests.remove(&self.id);
            if interests.is_empty() {
                state.watches.remove(&wd);
                // Fails if the kernel already dropped the watch.
                let _ = inotify::remove_watch(&self.watcher.0.fd, wd);
            }
        }
    }
}

struct Adding<'a> {
    fd: &'a OwnedFd,
    state: &'a mut State,
    id: u64,
    seen: HashSet<(PathBuf, bool)>,
}

impl Adding<'_> {
    fn path(&mut self, path: &Path, contents: bool, links: usize) -> io::Result<()> {
        if links > MAX_LINKS {
            return Err(io::Error::other("watched symlink chain is too deep"));
        }
        if !self.seen.insert((path.to_owned(), contents)) {
            return Ok(());
        }
        if self.seen.len() > MAX_PATHS {
            return Err(io::Error::other("too many watched paths"));
        }
        let mut parent = PathBuf::from("/");
        for component in path.components().skip(1) {
            let name = component.as_os_str();
            match self.add(&parent) {
                Ok(interest) => {
                    interest.names.insert(name.to_owned());
                }
                // A missing ancestor's creation shows in the last one watched.
                Err(error) if error == rustix::io::Errno::NOENT || error == rustix::io::Errno::NOTDIR => {
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            }
            parent.push(name);
            if let Ok(target) = std::fs::read_link(&parent) {
                let target = if target.is_absolute() {
                    target
                } else {
                    parent.parent().unwrap().join(target)
                };
                self.path(&target, contents, links + 1)?;
            }
        }
        // For names, the parent's watch reports replacement and attribute
        // changes. A file's own watch also sees writes through hard links
        // in other directories.
        if contents && (path.is_dir() || path.is_file()) {
            match self.add(path) {
                Ok(interest) => interest.all = true,
                Err(error) if error == rustix::io::Errno::NOENT || error == rustix::io::Errno::NOTDIR => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn add(&mut self, path: &Path) -> rustix::io::Result<&mut Interest> {
        let wd = inotify::add_watch(
            self.fd,
            path,
            WatchFlags::MODIFY
                | WatchFlags::ATTRIB
                | WatchFlags::CLOSE_WRITE
                | WatchFlags::CREATE
                | WatchFlags::DELETE
                | WatchFlags::MOVED_FROM
                | WatchFlags::MOVED_TO
                | WatchFlags::DELETE_SELF
                | WatchFlags::MOVE_SELF,
        )?;
        self.state
            .subscriptions
            .get_mut(&self.id)
            .unwrap()
            .watches
            .insert(wd);
        Ok(self.state.watches.entry(wd).or_default().entry(self.id).or_default())
    }
}

impl Inner {
    fn drain(&self, state: &mut State) -> io::Result<()> {
        let mut buffer = [MaybeUninit::uninit(); 4096];
        let mut reader = inotify::Reader::new(&self.fd, &mut buffer);
        loop {
            let event = match reader.next() {
                Ok(event) => event,
                Err(error) if error == rustix::io::Errno::AGAIN => return Ok(()),
                Err(error) => {
                    state.all_stale();
                    return Err(error.into());
                }
            };
            let flags = event.events();
            if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
                state.all_stale();
                continue;
            }
            let wd = event.wd();
            let itself = flags.intersects(
                ReadFlags::IGNORED | ReadFlags::UNMOUNT | ReadFlags::DELETE_SELF | ReadFlags::MOVE_SELF,
            );
            let Some(interests) = state.watches.get(&wd) else { continue };
            let name = event.file_name().map(|name| OsStr::from_bytes(name.to_bytes()));
            for (id, interest) in interests {
                if itself || interest.all || name.is_some_and(|name| interest.names.contains(name)) {
                    state.subscriptions[id].stale.store(true, Ordering::Release);
                }
            }
            if flags.contains(ReadFlags::IGNORED) {
                // The kernel dropped the watch.
                state.watches.remove(&wd);
            }
        }
    }
}

impl State {
    fn all_stale(&self) {
        for subscribed in self.subscriptions.values() {
            subscribed.stale.store(true, Ordering::Release);
        }
    }
}

fn drain_forever(inner: &Inner) {
    loop {
        let mut fds = [rustix::event::PollFd::new(&inner.fd, rustix::event::PollFlags::IN)];
        match rustix::event::poll(&mut fds, None) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => return,
        }
        let _ = inner.drain(&mut inner.state.lock().unwrap());
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn watch(watcher: &Watcher, contents: &[PathBuf], names: &[PathBuf]) -> Subscription {
        watcher
            .watch(contents.iter().map(PathBuf::as_path), names.iter().map(PathBuf::as_path))
            .unwrap()
    }

    #[test]
    fn symlink_targets_retargeting_and_ancestor_replacement() {
        let watcher = Watcher::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("dir/value"), "one").unwrap();
        symlink("dir/value", root.path().join("link")).unwrap();
        let paths = [root.path().join("link")];
        let subscription = watch(&watcher, &paths, &[]);
        assert!(!subscription.changed().unwrap());
        std::fs::write(root.path().join("dir/value"), "two").unwrap();
        assert!(subscription.changed().unwrap());
        let subscription = watch(&watcher, &paths, &[]);
        std::fs::rename(root.path().join("dir"), root.path().join("old")).unwrap();
        assert!(subscription.changed().unwrap());
        let subscription = watch(&watcher, &paths, &[]);
        std::fs::remove_file(root.path().join("link")).unwrap();
        symlink("old/value", root.path().join("link")).unwrap();
        assert!(subscription.changed().unwrap());
    }

    #[test]
    fn writes_through_hard_links_invalidate() {
        let watcher = Watcher::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("other")).unwrap();
        let input = root.path().join("input");
        let alias = root.path().join("other/alias");
        std::fs::write(&input, "one").unwrap();
        std::fs::hard_link(&input, &alias).unwrap();
        let subscription = watch(&watcher, &[input], &[]);
        assert!(!subscription.changed().unwrap());
        std::fs::write(alias, "two").unwrap();
        assert!(subscription.changed().unwrap());
    }

    #[test]
    fn names_see_creation_and_removal_but_not_contents() {
        let watcher = Watcher::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("dir");
        std::fs::create_dir(&dir).unwrap();
        let subscription = watch(&watcher, &[], &[dir.clone(), root.path().join("missing")]);
        std::fs::write(dir.join("inside"), "x").unwrap();
        std::fs::write(root.path().join("unrelated"), "x").unwrap();
        assert!(!subscription.changed().unwrap());
        std::fs::write(root.path().join("missing"), "x").unwrap();
        assert!(subscription.changed().unwrap());
    }

    #[test]
    fn subscriptions_share_watches_independently() {
        let watcher = Watcher::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        let (one, two) = (root.path().join("one"), root.path().join("two"));
        std::fs::write(&one, "").unwrap();
        std::fs::write(&two, "").unwrap();
        let first = watch(&watcher, &[one.clone()], &[]);
        let second = watch(&watcher, &[two.clone()], &[]);
        let both = watch(&watcher, &[one.clone(), two.clone()], &[]);
        std::fs::write(&two, "x").unwrap();
        assert!(!first.changed().unwrap());
        assert!(second.changed().unwrap() && both.changed().unwrap());
        // Dropping a subscription keeps the watches others share.
        drop(both);
        std::fs::write(&one, "x").unwrap();
        assert!(first.changed().unwrap());
        drop((first, second));
        assert!(watcher.0.state.lock().unwrap().watches.is_empty());
    }
}

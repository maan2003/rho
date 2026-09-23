//! Input tracking types for Nix evaluation caching.
//!
//! Unlike upstream devenv, inputs are what the evaluator observed when it read
//! the flake's local sources, reported by Nix at read time (see
//! [`devenv_core::ObservedKind`]). Paths are relative to the flake's source
//! root (its git repository, or the flake directory for `path:` flakes), so
//! every checkout and worktree can validate the same cache entry against its
//! own files. A git flake only exposes files in the git index; [`Checkout`]
//! applies the same visibility rule when observing the working tree.

use devenv_cache_core::{CacheError, CacheResult, compute_string_hash};
use devenv_core::ObservedKind;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// How a local source tree was fetched, which decides file visibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FlakeScheme {
    /// `git+file`: only files in the git index are visible.
    Git,
    /// `path`: every file is visible.
    Path,
}

impl FlakeScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            FlakeScheme::Git => "git",
            FlakeScheme::Path => "path",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "git" => Some(FlakeScheme::Git),
            "path" => Some(FlakeScheme::Path),
            _ => None,
        }
    }
}

/// An input dependency tracked during evaluation.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Input {
    Path(PathInput),
    /// Source-info metadata of the flake itself (`rev`, `dirtyRev`,
    /// `lastModified`, ...), all of which follow from its git revision state.
    FlakeRev(RevInputDesc),
}

impl Input {
    /// Hash of every input's identity and state, which tells candidates of
    /// one cache key apart.
    pub fn compute_input_hash(inputs: &[Self]) -> String {
        let mut lines: Vec<String> = inputs
            .iter()
            .map(|input| match input {
                Input::Path(p) => format!(
                    "{} {} {:?} {}",
                    p.kind.as_str(),
                    p.scheme.as_str(),
                    p.value,
                    p.path.display()
                ),
                Input::FlakeRev(r) => format!("rev {}", r.content_hash.as_deref().unwrap_or("-")),
            })
            .collect();
        lines.sort();
        compute_string_hash(&lines.join("\n"))
    }

    /// The same input with its state captured now.
    pub fn recapture(&self, checkout: &Checkout, hashes: &mut dyn FileHashes) -> io::Result<Self> {
        Ok(match self {
            Input::Path(p) => Input::Path(PathInput {
                value: checkout.observe(p.scheme, &p.path, p.kind, hashes)?,
                ..p.clone()
            }),
            Input::FlakeRev(_) => Input::FlakeRev(RevInputDesc::new(checkout)?),
        })
    }

    /// What identifies this input apart from its state.
    pub fn identity(&self) -> InputIdentity {
        match self {
            Input::Path(p) => InputIdentity::Path(p.kind, p.scheme, p.path.clone()),
            Input::FlakeRev(_) => InputIdentity::FlakeRev,
        }
    }

    /// Whether the input's state differs now.
    pub fn changed(&self, checkout: &Checkout, hashes: &mut dyn FileHashes) -> io::Result<bool> {
        Ok(self.recapture(checkout, hashes)? != *self)
    }
}

/// See [`Input::identity`].
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum InputIdentity {
    Path(ObservedKind, FlakeScheme, PathBuf),
    FlakeRev,
}

/// One observation of a path in the flake's source tree.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct PathInput {
    pub kind: ObservedKind,
    /// Visibility rule of the input the path was read through.
    pub scheme: FlakeScheme,
    /// Relative to the source root; empty for the root itself.
    pub path: PathBuf,
    /// What was observed, in Nix's format; see [`Checkout::observe`].
    pub value: String,
}

/// What [`Checkout::observe`] reports for a file, directory or symlink Nix
/// could not have read; never a value Nix itself reports for them.
pub const UNREADABLE: &str = "\0unreadable";

/// The flake's revision state: `HEAD` and whether the tree is dirty.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct RevInputDesc {
    /// `None` if the flake is not a git repository.
    pub content_hash: Option<String>,
}

impl RevInputDesc {
    pub fn new(checkout: &Checkout) -> io::Result<Self> {
        if checkout.scheme != FlakeScheme::Git {
            return Ok(Self { content_hash: None });
        }
        let git = |args: &[&str]| -> io::Result<String> {
            let out = Command::new("git").arg("-C").arg(&checkout.root).args(args).output()?;
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        };
        let head = git(&["rev-parse", "--verify", "-q", "HEAD"])?;
        let dirty = !git(&["status", "--porcelain", "--untracked-files=no"])?.is_empty();
        Ok(Self {
            content_hash: Some(compute_string_hash(&format!("{head} dirty={dirty}"))),
        })
    }
}

/// Content hashes of regular files, possibly answered from a cache.
pub trait FileHashes {
    /// What [`devenv_cache_core::compute_file_hash`] returns for `path`:
    /// base16 BLAKE3 of its contents, as Nix reports them.
    fn content_hash(&mut self, path: &Path) -> CacheResult<String>;
}

/// Hashes every file from scratch.
pub struct Uncached;

impl FileHashes for Uncached {
    fn content_hash(&mut self, path: &Path) -> CacheResult<String> {
        devenv_cache_core::compute_file_hash(path)
    }
}

/// One checkout of the flake's source tree, as the evaluator would see it.
pub struct Checkout {
    pub root: PathBuf,
    pub scheme: FlakeScheme,
    /// For git checkouts, the paths in the git index.
    tracked: Option<BTreeSet<Vec<u8>>>,
}

impl Checkout {
    /// Load the visibility rule for the source tree at `root`: for git
    /// checkouts, the paths in its index.
    pub fn new(root: &Path, scheme: FlakeScheme) -> io::Result<Self> {
        let tracked = match scheme {
            FlakeScheme::Path => None,
            FlakeScheme::Git => Some(match read_index(root)? {
                Some(tracked) => tracked,
                None => ls_files(root)?,
            }),
        };
        Ok(Self {
            root: root.to_path_buf(),
            scheme,
            tracked,
        })
    }

    /// Whether the root-relative `rel`, read through an input with `scheme`,
    /// is visible: for git, a tracked file or a directory containing one.
    pub fn is_visible(&self, scheme: FlakeScheme, rel: &Path) -> bool {
        let (FlakeScheme::Git, Some(tracked)) = (scheme, &self.tracked) else {
            return true;
        };
        let rel = rel.as_os_str().as_bytes();
        if rel.is_empty() || tracked.contains(rel) {
            return true;
        }
        let mut prefix = rel.to_vec();
        prefix.push(b'/');
        tracked
            .range(prefix.clone()..)
            .next()
            .is_some_and(|p| p.starts_with(&prefix))
    }

    /// What Nix would observe reading `rel` now, in the format of the
    /// `observed-*` eval effects.
    pub fn observe(
        &self,
        scheme: FlakeScheme,
        rel: &Path,
        kind: ObservedKind,
        hashes: &mut dyn FileHashes,
    ) -> io::Result<String> {
        let missing = || match kind {
            ObservedKind::Stat => "missing".to_owned(),
            _ => UNREADABLE.to_owned(),
        };
        if !self.is_visible(scheme, rel) {
            return Ok(missing());
        }
        let physical = self.root.join(rel);
        let result = match kind {
            ObservedKind::Stat => std::fs::symlink_metadata(&physical).map(|meta| {
                let file_type = meta.file_type();
                if file_type.is_file() {
                    if meta.permissions().mode() & 0o100 != 0 { "executable" } else { "regular" }
                } else if file_type.is_dir() {
                    "directory"
                } else if file_type.is_symlink() {
                    "symlink"
                } else {
                    "other"
                }
                .to_owned()
            }),
            // Nix reads through symlinks, and cannot read a directory.
            ObservedKind::File => match std::fs::metadata(&physical) {
                Ok(meta) if !meta.is_file() => return Ok(missing()),
                Ok(_) => hashes.content_hash(&physical).map_err(cache_error_to_io),
                Err(e) => Err(e),
            },
            ObservedKind::Dir => std::fs::read_dir(&physical).and_then(|entries| {
                let mut names = Vec::new();
                for entry in entries {
                    let name = entry?.file_name();
                    if self.is_visible(scheme, &rel.join(&name)) {
                        names.push(name);
                    }
                }
                Ok(listing_hash(names))
            }),
            ObservedKind::Link => {
                std::fs::read_link(&physical).map(|target| target.to_string_lossy().into_owned())
            }
        };
        match result {
            Ok(value) => Ok(value),
            Err(e) if is_absent(&e) => Ok(missing()),
            Err(e) => Err(e),
        }
    }
}

/// The paths in the git index of the checkout at `root`, read directly, or
/// `None` if only git can tell: a split or sparse index, or a format this
/// reader does not know.
fn read_index(root: &Path) -> io::Result<Option<BTreeSet<Vec<u8>>>> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        // Worktrees and submodules: a `gitdir:` file.
        let Ok(file) = std::fs::read(&dot_git) else { return Ok(None) };
        let Some(dir) = file.strip_prefix(b"gitdir: ") else { return Ok(None) };
        root.join(std::ffi::OsStr::from_bytes(dir.trim_ascii_end()))
    };
    let index = match std::fs::read(git_dir.join("index")) {
        Ok(index) => index,
        // No index yet: nothing is tracked.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Some(BTreeSet::new())),
        Err(e) => return Err(e),
    };
    Ok(parse_index(&index))
}

/// The entry names of a version 2, 3 or 4 git index with SHA-1 object ids.
fn parse_index(index: &[u8]) -> Option<BTreeSet<Vec<u8>>> {
    const HASH: usize = 20;
    let u32_at = |at: usize| Some(u32::from_be_bytes(index.get(at..at + 4)?.try_into().ok()?));
    if index.get(..4)? != b"DIRC" {
        return None;
    }
    let version = u32_at(4)?;
    if !(2..=4).contains(&version) {
        return None;
    }
    let count = u32_at(8)?;
    let mut at = 12;
    let mut name = Vec::new();
    let mut tracked = BTreeSet::new();
    for _ in 0..count {
        let start = at;
        // ctime, mtime, dev, ino, mode, uid, gid, size, object id, flags
        let mode = u32_at(at + 24)?;
        at += 40 + HASH;
        let flags = u16::from_be_bytes(index.get(at..at + 2)?.try_into().ok()?);
        at += 2;
        if flags & 0x4000 != 0 {
            if version < 3 {
                return None;
            }
            at += 2;
        }
        if version == 4 {
            // Strip a varint's worth of the previous name, then append.
            let mut byte = *index.get(at)?;
            at += 1;
            let mut strip = usize::from(byte & 0x7f);
            while byte & 0x80 != 0 {
                byte = *index.get(at)?;
                at += 1;
                strip = ((strip + 1) << 7) | usize::from(byte & 0x7f);
            }
            name.truncate(name.len().checked_sub(strip)?);
        } else {
            name.clear();
        }
        let len = index.get(at..)?.iter().position(|b| *b == 0)?;
        name.extend_from_slice(&index[at..at + len]);
        at += len + 1;
        if usize::from(flags & 0xfff) != name.len().min(0xfff) {
            return None; // not SHA-1, or corrupt
        }
        if version < 4 {
            // NUL padding to a multiple of 8 bytes.
            at = start + (at - start).div_ceil(8) * 8;
        }
        // Directory entries only exist in sparse indexes.
        if mode & 0o170000 == 0o040000 {
            return None;
        }
        tracked.insert(name.clone());
    }
    // Entries of a split index live in its shared index too.
    while at + 8 + HASH <= index.len() {
        let signature = &index[at..at + 4];
        if signature == b"link" || signature == b"sdir" {
            return None;
        }
        at += 8 + usize::try_from(u32_at(at + 4)?).ok()?;
    }
    Some(tracked)
}

/// The paths in the git index of the checkout at `root`, as git lists them.
fn ls_files(root: &Path) -> io::Result<BTreeSet<Vec<u8>>> {
    let out = Command::new("git").arg("-C").arg(root).args(["ls-files", "-z"]).output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "git ls-files in {} failed: {}",
            root.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out
        .stdout
        .split(|b| *b == 0)
        .filter(|entry| !entry.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

/// Nix's `observed-dir` value for a directory with entries `names`.
pub fn listing_hash(mut names: Vec<OsString>) -> String {
    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let mut hasher = blake3::Hasher::new();
    for name in &names {
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
    }
    hasher.finalize().to_hex().to_string()
}

/// Whether `e` means the path cannot be read as the observed kind, as
/// opposed to a failure to check it.
fn is_absent(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory | io::ErrorKind::InvalidInput
    )
}

fn cache_error_to_io(e: CacheError) -> io::Error {
    match e {
        CacheError::Io(e) => e,
        CacheError::FileNotFound(path) => io::Error::new(io::ErrorKind::NotFound, path.display().to_string()),
        e => io::Error::other(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn observe(checkout: &Checkout, scheme: FlakeScheme, rel: &str, kind: ObservedKind) -> String {
        checkout.observe(scheme, Path::new(rel), kind, &mut Uncached).unwrap()
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git").arg("-C").arg(dir).args(args).status().unwrap();
        assert!(status.success());
    }

    #[test]
    fn test_file_hash_is_blake3_of_contents() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("a"), b"hello").unwrap();
        let checkout = Checkout::new(temp.path(), FlakeScheme::Path).unwrap();
        assert_eq!(
            observe(&checkout, FlakeScheme::Path, "a", ObservedKind::File),
            blake3::hash(b"hello").to_hex().to_string()
        );
        assert_eq!(observe(&checkout, FlakeScheme::Path, "b", ObservedKind::File), UNREADABLE);
    }

    #[test]
    fn test_stat_types() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path();
        std::fs::write(dir.join("file"), b"").unwrap();
        std::fs::write(dir.join("exe"), b"").unwrap();
        std::fs::set_permissions(dir.join("exe"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir(dir.join("dir")).unwrap();
        symlink("file", dir.join("link")).unwrap();
        let checkout = Checkout::new(dir, FlakeScheme::Path).unwrap();
        let stat = |rel| observe(&checkout, FlakeScheme::Path, rel, ObservedKind::Stat);
        assert_eq!(stat("file"), "regular");
        assert_eq!(stat("exe"), "executable");
        assert_eq!(stat("dir"), "directory");
        assert_eq!(stat("link"), "symlink");
        assert_eq!(stat("none"), "missing");
        assert_eq!(stat("file/below"), "missing");
        assert_eq!(observe(&checkout, FlakeScheme::Path, "link", ObservedKind::Link), "file");
    }

    #[test]
    fn test_listing_matches_nix_format() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("b"), b"").unwrap();
        std::fs::write(temp.path().join("a"), b"").unwrap();
        let checkout = Checkout::new(temp.path(), FlakeScheme::Path).unwrap();
        assert_eq!(
            observe(&checkout, FlakeScheme::Path, "", ObservedKind::Dir),
            blake3::hash(b"a\0b\0").to_hex().to_string()
        );
    }

    #[test]
    fn test_git_hides_untracked_files() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path();
        git(dir, &["init", "-q"]);
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/tracked"), b"x").unwrap();
        std::fs::write(dir.join("sub/untracked"), b"y").unwrap();
        std::fs::create_dir(dir.join("empty")).unwrap();
        git(dir, &["add", "sub/tracked"]);
        let checkout = Checkout::new(dir, FlakeScheme::Git).unwrap();

        assert_eq!(
            observe(&checkout, FlakeScheme::Git, "sub", ObservedKind::Dir),
            listing_hash(vec!["tracked".into()])
        );
        assert_eq!(observe(&checkout, FlakeScheme::Git, "sub/untracked", ObservedKind::Stat), "missing");
        assert_eq!(observe(&checkout, FlakeScheme::Git, "empty", ObservedKind::Stat), "missing");
        assert_eq!(observe(&checkout, FlakeScheme::Git, "sub", ObservedKind::Stat), "directory");
        // A `path:` input nested in the repository sees everything.
        assert_eq!(
            observe(&checkout, FlakeScheme::Path, "sub/untracked", ObservedKind::Stat),
            "regular"
        );
    }

    #[test]
    fn test_index_reader_agrees_with_git() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path();
        git(dir, &["init", "-q"]);
        for name in ["a", "dir/b", "dir/sub/c", "dir/sub/cc", "long-name-shares-a-prefix", "x y"] {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, name).unwrap();
        }
        git(dir, &["add", "."]);
        git(dir, &["add", "-N", "--", "."]);
        std::fs::write(dir.join("intent"), b"").unwrap();
        git(dir, &["add", "-N", "intent"]);
        for version in ["2", "3", "4"] {
            git(dir, &["update-index", "--index-version", version]);
            assert_eq!(read_index(dir).unwrap(), Some(ls_files(dir).unwrap()), "index v{version}");
        }
        git(dir, &["update-index", "--split-index"]);
        assert_eq!(read_index(dir).unwrap(), None);
    }

    #[test]
    fn test_changed_detects_edits() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("a"), b"one").unwrap();
        let checkout = Checkout::new(temp.path(), FlakeScheme::Path).unwrap();
        let input = Input::Path(PathInput {
            kind: ObservedKind::File,
            scheme: FlakeScheme::Path,
            path: "a".into(),
            value: observe(&checkout, FlakeScheme::Path, "a", ObservedKind::File),
        });
        assert!(!input.changed(&checkout, &mut Uncached).unwrap());
        std::fs::write(temp.path().join("a"), b"two").unwrap();
        assert!(input.changed(&checkout, &mut Uncached).unwrap());
    }
}

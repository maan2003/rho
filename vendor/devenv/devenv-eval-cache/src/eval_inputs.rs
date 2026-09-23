//! Input tracking types for Nix evaluation caching.
//!
//! This module contains types that describe file and environment variable
//! dependencies tracked during Nix evaluation for caching purposes.
//!
//! Unlike upstream devenv, file inputs inside the evaluated flake are
//! [`Anchor::Flake`]-relative, so that every checkout and worktree of a flake
//! can validate the same cache entry against its own files. A git flake only
//! exposes files in the git index; [`Checkout`] applies the same visibility
//! rule when hashing the working tree.

use devenv_cache_core::file::{is_executable, source_file_hash};
use devenv_cache_core::{CacheResult, compute_directory_content_hash_with, compute_string_hash};
use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// Where a file input's path is anchored.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Anchor {
    /// Relative to the evaluated flake's directory; empty for the flake itself.
    Flake,
    /// An absolute path outside the flake.
    Absolute,
}

impl Anchor {
    pub fn as_str(self) -> &'static str {
        match self {
            Anchor::Flake => "flake",
            Anchor::Absolute => "abs",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "flake" => Some(Anchor::Flake),
            "abs" => Some(Anchor::Absolute),
            _ => None,
        }
    }
}

/// How the flake's own tree was fetched, which decides file visibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
}

/// An input dependency tracked during evaluation.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Input {
    File(FileInputDesc),
    Env(EnvInputDesc),
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
                Input::File(f) => format!(
                    "file {} {} {} {} {}",
                    f.anchor.as_str(),
                    f.recursive,
                    f.is_directory,
                    f.content_hash.as_deref().unwrap_or("-"),
                    f.path.display()
                ),
                Input::Env(e) => {
                    format!("env {} {}", e.content_hash.as_deref().unwrap_or("-"), e.name)
                }
                Input::FlakeRev(r) => format!("rev {}", r.content_hash.as_deref().unwrap_or("-")),
            })
            .collect();
        lines.sort();
        compute_string_hash(&lines.join("\n"))
    }

    /// The same input with its state captured now.
    pub fn recapture(
        &self,
        checkout: &Checkout,
        hashes: &mut dyn FileHashes,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> io::Result<Self> {
        Ok(match self {
            Input::File(file) => Input::File(FileInputDesc::new(
                file.anchor,
                file.path.clone(),
                file.recursive,
                checkout,
                hashes,
            )?),
            Input::Env(e) => Input::Env(EnvInputDesc::new(e.name.clone(), env)),
            Input::FlakeRev(_) => Input::FlakeRev(RevInputDesc::new(checkout)?),
        })
    }

    /// What identifies this input apart from its state.
    pub fn identity(&self) -> InputIdentity {
        match self {
            Input::File(f) => InputIdentity::File(f.anchor, f.path.clone(), f.recursive),
            Input::Env(e) => InputIdentity::Env(e.name.clone()),
            Input::FlakeRev(_) => InputIdentity::FlakeRev,
        }
    }

    /// Whether the input's state differs now.
    pub fn changed(
        &self,
        checkout: &Checkout,
        hashes: &mut dyn FileHashes,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> io::Result<bool> {
        Ok(self.recapture(checkout, hashes, env)? != *self)
    }
}

/// See [`Input::identity`].
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum InputIdentity {
    File(Anchor, PathBuf, bool),
    Env(String),
    FlakeRev,
}

/// Description of a file input dependency.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct FileInputDesc {
    pub anchor: Anchor,
    pub path: PathBuf,
    pub is_directory: bool,
    /// Whether this path is copied into the Nix store.
    ///
    /// For directories, copied paths are hashed recursively over their contents.
    /// For files, copied paths include the owner's executable bit in their hash.
    /// `false` inputs retain the cheaper observation-specific hashing used by
    /// operations such as `readFile` and `readDir`.
    pub recursive: bool,
    /// `None` if the path does not exist (or is not visible in the flake).
    pub content_hash: Option<String>,
}

impl FileInputDesc {
    /// Capture the current state of a path as `checkout` exposes it.
    pub fn new(
        anchor: Anchor,
        path: PathBuf,
        recursive: bool,
        checkout: &Checkout,
        hashes: &mut dyn FileHashes,
    ) -> io::Result<Self> {
        let physical = checkout.physical(anchor, &path);
        let visible = |rel: &str| anchor == Anchor::Absolute || checkout.is_visible(&join(&path, rel));
        let metadata = if visible("") {
            match std::fs::metadata(&physical) {
                Ok(metadata) => Some(metadata),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        let Some(metadata) = metadata else {
            return Ok(Self {
                anchor,
                path,
                is_directory: false,
                recursive,
                content_hash: None,
            });
        };
        let is_directory = metadata.is_dir();
        let content_hash = if is_directory {
            if recursive {
                compute_directory_content_hash_with(&physical, &visible, &mut |file| {
                    hashes.source_hash(file)
                })
                .map_err(io::Error::other)?
            } else {
                // Only the listing is observed by `readDir`.
                let mut names: Vec<String> = std::fs::read_dir(&physical)?
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| visible(name))
                    .collect();
                names.sort();
                compute_string_hash(&names.join("\n"))
            }
        } else if recursive {
            hashes.source_hash(&physical).map_err(io::Error::other)?
        } else {
            hashes.content_hash(&physical).map_err(io::Error::other)?
        };
        Ok(Self {
            anchor,
            path,
            is_directory,
            recursive,
            content_hash: Some(content_hash),
        })
    }
}

fn join(base: &Path, rel: &str) -> String {
    let base = base.to_string_lossy();
    match (base.is_empty(), rel.is_empty()) {
        (true, _) => rel.to_owned(),
        (false, true) => base.into_owned(),
        (false, false) => format!("{base}/{rel}"),
    }
}

/// Description of an environment variable input dependency.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct EnvInputDesc {
    pub name: String,
    pub content_hash: Option<String>,
}

impl EnvInputDesc {
    /// Capture the variable's current value. Presence is preserved separately
    /// from an empty value.
    pub fn new(name: String, env: &dyn Fn(&str) -> Option<String>) -> Self {
        let content_hash = env(&name).map(|v| compute_string_hash(&v));
        Self { name, content_hash }
    }
}

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
            let out = Command::new("git").arg("-C").arg(&checkout.dir).args(args).output()?;
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
    /// What [`devenv_cache_core::compute_file_hash`] returns for `path`.
    fn content_hash(&mut self, path: &Path) -> CacheResult<String>;

    /// What [`devenv_cache_core::compute_source_file_hash`] returns for `path`.
    fn source_hash(&mut self, path: &Path) -> CacheResult<String> {
        let executable = is_executable(&std::fs::metadata(path)?);
        Ok(source_file_hash(&self.content_hash(path)?, executable))
    }
}

/// Hashes every file from scratch.
pub struct Uncached;

impl FileHashes for Uncached {
    fn content_hash(&mut self, path: &Path) -> CacheResult<String> {
        devenv_cache_core::compute_file_hash(path)
    }
}

/// One checkout of the evaluated flake, as the evaluator would see it.
pub struct Checkout {
    pub dir: PathBuf,
    pub scheme: FlakeScheme,
    /// For git flakes, the paths in the git index.
    tracked: Option<BTreeSet<String>>,
}

impl Checkout {
    /// Load the visibility rule for the flake at `dir`: one `git ls-files`
    /// for git flakes.
    pub fn new(dir: &Path, scheme: FlakeScheme) -> io::Result<Self> {
        let tracked = match scheme {
            FlakeScheme::Path => None,
            FlakeScheme::Git => {
                let out = Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(["ls-files", "-z"])
                    .output()?;
                if !out.status.success() {
                    return Err(io::Error::other(format!(
                        "git ls-files in {} failed: {}",
                        dir.display(),
                        String::from_utf8_lossy(&out.stderr).trim()
                    )));
                }
                Some(
                    out.stdout
                        .split(|b| *b == 0)
                        .filter(|entry| !entry.is_empty())
                        .map(|entry| String::from_utf8_lossy(entry).into_owned())
                        .collect(),
                )
            }
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            scheme,
            tracked,
        })
    }

    pub fn physical(&self, anchor: Anchor, path: &Path) -> PathBuf {
        match anchor {
            Anchor::Flake => self.dir.join(path),
            Anchor::Absolute => path.to_path_buf(),
        }
    }

    /// Whether the flake-relative `rel` is a tracked file or a directory
    /// containing one.
    pub fn is_visible(&self, rel: &str) -> bool {
        let Some(tracked) = &self.tracked else {
            return true;
        };
        if rel.is_empty() || tracked.contains(rel) {
            return true;
        }
        let prefix = format!("{rel}/");
        tracked
            .range(prefix.clone()..)
            .next()
            .is_some_and(|p| p.starts_with(&prefix))
    }
}

/// Return the first file input modified at or after `threshold`.
///
/// Its recaptured state may not be what Nix read, so the result must not be
/// cached. Re-stats each file with full precision; a recursive directory is
/// checked entry by entry since nested edits do not touch its own mtime.
pub fn any_input_modified_after(
    inputs: &[Input],
    checkout: &Checkout,
    threshold: SystemTime,
) -> Option<PathBuf> {
    let newer = |path: &Path| {
        std::fs::symlink_metadata(path)
            .and_then(|m| m.modified())
            .is_ok_and(|mtime| mtime >= threshold)
    };
    inputs.iter().find_map(|input| {
        let Input::File(file) = input else {
            return None;
        };
        let physical = checkout.physical(file.anchor, &file.path);
        if newer(&physical) {
            return Some(physical);
        }
        if file.is_directory && file.recursive {
            return walkdir::WalkDir::new(&physical)
                .min_depth(1)
                .into_iter()
                .filter_map(Result::ok)
                .map(|entry| entry.into_path())
                .find(|path| newer(path));
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn capture(dir: &Path, rel: &str, recursive: bool) -> FileInputDesc {
        let checkout = Checkout::new(dir, FlakeScheme::Path).unwrap();
        FileInputDesc::new(Anchor::Flake, rel.into(), recursive, &checkout, &mut Uncached).unwrap()
    }

    fn changed(dir: &Path, desc: &FileInputDesc) -> bool {
        let checkout = Checkout::new(dir, FlakeScheme::Path).unwrap();
        Input::File(desc.clone())
            .changed(&checkout, &mut Uncached, &|_| None)
            .unwrap()
    }

    #[test]
    fn test_unchanged_file() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::write(temp_dir.path().join("test.txt"), b"Hello, World!").unwrap();
        let desc = capture(temp_dir.path(), "test.txt", false);
        assert!(desc.content_hash.is_some());
        assert!(!changed(temp_dir.path(), &desc));
    }

    #[test]
    fn test_metadata_modified_file() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("test.txt");
        std::fs::write(&path, b"Hello, World!").unwrap();
        let desc = capture(temp_dir.path(), "test.txt", false);

        let file = std::fs::File::open(&path).unwrap();
        file.set_modified(SystemTime::now() + std::time::Duration::from_secs(1))
            .unwrap();
        assert!(!changed(temp_dir.path(), &desc));
    }

    #[test]
    fn test_content_modified_file() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("test.txt");
        std::fs::write(&path, b"Hello, World!").unwrap();
        let desc = capture(temp_dir.path(), "test.txt", false);

        std::fs::write(&path, b"Modified content").unwrap();
        assert!(changed(temp_dir.path(), &desc));
    }

    #[test]
    fn test_created_file_is_modified() {
        let temp_dir = TempDir::new().unwrap();
        let missing = capture(temp_dir.path(), "appeared.txt", false);
        assert_eq!(missing.content_hash, None);
        assert!(!changed(temp_dir.path(), &missing));

        std::fs::write(temp_dir.path().join("appeared.txt"), b"now I exist").unwrap();
        assert!(changed(temp_dir.path(), &missing));
    }

    #[test]
    fn test_created_directory_is_modified() {
        let temp_dir = TempDir::new().unwrap();
        let missing = capture(temp_dir.path(), "appeared-dir", false);
        std::fs::create_dir(temp_dir.path().join("appeared-dir")).unwrap();
        assert!(changed(temp_dir.path(), &missing));
    }

    #[test]
    fn test_removed_file() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("test.txt");
        std::fs::write(&path, b"Hello, World!").unwrap();
        let desc = capture(temp_dir.path(), "test.txt", false);

        std::fs::remove_file(&path).unwrap();
        assert!(changed(temp_dir.path(), &desc));
    }

    /// Reproduces https://github.com/cachix/devenv/issues/2886: a copied
    /// source tree must be invalidated by nested content changes.
    #[test]
    fn test_check_state_detects_nested_content_change() {
        let temp_dir = TempDir::new().unwrap();
        let src = temp_dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let main_rs = src.join("main.rs");
        std::fs::write(&main_rs, b"fn main() { println!(\"Hello, world!\"); }").unwrap();

        let desc = capture(temp_dir.path(), "", true);
        assert!(desc.is_directory);

        std::fs::write(&main_rs, b"fn main() { println!(\"Goodbye, world!\"); }").unwrap();
        assert!(changed(temp_dir.path(), &desc));
    }

    #[test]
    fn test_check_state_detects_executable_bit_change_for_copied_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("script.sh");
        std::fs::write(&file_path, b"#!/bin/sh\nexit 0\n").unwrap();

        let mut permissions = std::fs::metadata(&file_path).unwrap().permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&file_path, permissions.clone()).unwrap();
        let desc = capture(temp_dir.path(), "script.sh", true);

        permissions.set_mode(0o744);
        std::fs::set_permissions(&file_path, permissions).unwrap();
        assert!(changed(temp_dir.path(), &desc));
    }

    /// A non-recursive directory (e.g. tracked via `readDir`) only depends on its
    /// listing, so a nested content change must not invalidate it.
    #[test]
    fn test_check_state_ignores_nested_change_for_non_recursive_dir() {
        let temp_dir = TempDir::new().unwrap();
        let src = temp_dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let main_rs = src.join("main.rs");
        std::fs::write(&main_rs, b"fn main() {}").unwrap();

        let desc = capture(temp_dir.path(), "src", false);
        std::fs::write(&main_rs, b"fn main() { loop {} }").unwrap();
        assert!(!changed(temp_dir.path(), &desc));

        std::fs::write(src.join("lib.rs"), b"").unwrap();
        assert!(changed(temp_dir.path(), &desc));
    }

    #[test]
    fn same_tree_in_two_checkouts_hashes_identically() {
        let (a, b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        for dir in [a.path(), b.path()] {
            std::fs::create_dir(dir.join("nix")).unwrap();
            std::fs::write(dir.join("nix/a.nix"), b"{ }").unwrap();
        }
        assert_eq!(capture(a.path(), "nix", true), capture(b.path(), "nix", true));
        assert_eq!(capture(a.path(), "nix/a.nix", false), capture(b.path(), "nix/a.nix", false));
    }

    #[test]
    fn untracked_files_are_invisible_in_git_flakes() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        let git = |args: &[&str]| {
            assert!(Command::new("git").arg("-C").arg(dir).args(args).status().unwrap().success())
        };
        git(&["init", "-q"]);
        std::fs::write(dir.join("flake.nix"), b"{ }").unwrap();
        git(&["add", "flake.nix"]);
        let checkout = Checkout::new(dir, FlakeScheme::Git).unwrap();
        let tree = |checkout: &Checkout| {
            FileInputDesc::new(Anchor::Flake, "".into(), true, checkout, &mut Uncached).unwrap()
        };
        let before = tree(&checkout);

        std::fs::write(dir.join("untracked.nix"), b"{ }").unwrap();
        let checkout = Checkout::new(dir, FlakeScheme::Git).unwrap();
        assert_eq!(before, tree(&checkout));
        let untracked =
            FileInputDesc::new(Anchor::Flake, "untracked.nix".into(), false, &checkout, &mut Uncached)
                .unwrap();
        assert_eq!(untracked.content_hash, None);

        git(&["add", "untracked.nix"]);
        let checkout = Checkout::new(dir, FlakeScheme::Git).unwrap();
        assert_ne!(before, tree(&checkout));
    }

    #[test]
    fn empty_environment_value_is_distinct_from_unset() {
        let missing = EnvInputDesc::new("X".into(), &|_| None);
        let empty = EnvInputDesc::new("X".into(), &|_| Some(String::new()));
        assert_eq!(missing.content_hash, None);
        assert!(empty.content_hash.is_some());
    }

    #[test]
    fn inputs_modified_after_threshold_are_found() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::create_dir(temp_dir.path().join("src")).unwrap();
        std::fs::write(temp_dir.path().join("src/a"), b"a").unwrap();
        let inputs = vec![Input::File(capture(temp_dir.path(), "src", true))];
        let checkout = Checkout::new(temp_dir.path(), FlakeScheme::Path).unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(3600);
        assert_eq!(any_input_modified_after(&inputs, &checkout, later), None);
        let earlier = SystemTime::now() - std::time::Duration::from_secs(3600);
        assert!(any_input_modified_after(&inputs, &checkout, earlier).is_some());
    }
}

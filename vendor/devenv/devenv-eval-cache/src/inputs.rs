//! What a development shell evaluation depended on, in a form that is valid
//! for every checkout of the same flake.
//!
//! Nix reports effects against physical paths and against the virtual store
//! paths it mounts inputs at. [`record_inputs`] maps both onto
//! [`Root::Flake`]-relative paths when they fall inside the evaluated flake,
//! drops paths inside locked inputs (their identity is `flake.lock`, which is
//! part of the cache key), and keeps anything else as an absolute path.
//!
//! A git flake only exposes files in the git index, so input state for
//! [`Root::Flake`] is computed through [`FlakeView`], which applies the same
//! visibility rule to the working tree.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use devenv_core::eval_op::EvalOp;

/// Where an input path is anchored.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Root {
    /// Relative to the evaluated flake's directory.
    Flake,
    /// An absolute path outside the flake.
    Absolute,
    /// A process environment variable; `path` is its name.
    Env,
}

impl Root {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Root::Flake => "flake",
            Root::Absolute => "abs",
            Root::Env => "env",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "flake" => Root::Flake,
            "abs" => Root::Absolute,
            "env" => Root::Env,
            _ => return None,
        })
    }
}

/// How an input was observed, which decides what part of its state matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// File contents and executable bit (evaluated, read or hashed).
    File,
    /// A whole tree copied to the store: every visible file below it.
    Tree,
    /// A directory listing: child names and types.
    Listing,
    /// Only whether the path exists and its type.
    Type,
    /// An environment variable's value.
    Env,
    /// Source-info metadata of the flake itself (`rev`, `lastModified`, ...).
    FlakeRev,
}

impl Kind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Tree => "tree",
            Kind::Listing => "listing",
            Kind::Type => "type",
            Kind::Env => "env",
            Kind::FlakeRev => "flake-rev",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "file" => Kind::File,
            "tree" => Kind::Tree,
            "listing" => Kind::Listing,
            "type" => Kind::Type,
            "env" => Kind::Env,
            "flake-rev" => Kind::FlakeRev,
            _ => return None,
        })
    }
}

/// An input identity: which path, anchored where, observed how.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InputId {
    pub root: Root,
    pub kind: Kind,
    /// Relative (`Root::Flake`, empty for the flake root itself), absolute
    /// (`Root::Absolute`), or a variable name (`Root::Env`).
    pub path: String,
}

/// An input together with the state the evaluation observed.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Input {
    pub id: InputId,
    pub state: String,
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

/// The inputs of one evaluation.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub scheme: FlakeScheme,
    pub inputs: Vec<Input>,
}

#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("the evaluator never mounted the flake at {0}")]
    FlakeNotMounted(PathBuf),
    #[error("{0} changed while it was being evaluated")]
    ChangedDuringEval(String),
    #[error("reading {path}: {source}")]
    Io { path: String, source: io::Error },
}

/// Map evaluation effects to checkout-independent inputs and capture their
/// current state.
///
/// Fails with [`RecordError::ChangedDuringEval`] if an input was modified at
/// or after `started_at`: its current state may not be what Nix read, so the
/// result must not be cached.
pub fn record_inputs(
    ops: &[EvalOp],
    flake_dir: &Path,
    started_at: SystemTime,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Recorded, RecordError> {
    let mounts = Mounts::new(ops, flake_dir)?;
    let mut ids = BTreeSet::new();
    for op in ops {
        let (path, kind) = match op {
            EvalOp::EvaluatedFile { source, .. }
            | EvalOp::ReadFile { source }
            | EvalOp::HashFile { source, .. } => (source, Kind::File),
            EvalOp::CopiedSource { source, .. } | EvalOp::FilteredSource { source, .. } => {
                (source, Kind::Tree)
            }
            EvalOp::ReadDir { source } => (source, Kind::Listing),
            EvalOp::ReadFileType { source } | EvalOp::PathExists { source } => {
                (source, Kind::Type)
            }
            EvalOp::GetEnv { name } => {
                ids.insert(InputId {
                    root: Root::Env,
                    kind: Kind::Env,
                    path: name.clone(),
                });
                continue;
            }
            EvalOp::ForcedInputAttr { store_path, .. } => {
                if mounts.is_flake(store_path) {
                    ids.insert(InputId {
                        root: Root::Flake,
                        kind: Kind::FlakeRev,
                        path: String::new(),
                    });
                }
                continue;
            }
            EvalOp::MountedInput { .. } => continue,
        };
        if let Some((root, path)) = mounts.resolve(path) {
            ids.insert(InputId { root, kind, path });
        }
    }

    let mut view = FlakeView::new(flake_dir, mounts.scheme);
    view.prefetch(ids.iter()).map_err(|source| RecordError::Io {
        path: flake_dir.display().to_string(),
        source,
    })?;
    let mut inputs = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(abs) = view.physical_path(&id) {
            if modified_since(&abs, id.kind, started_at) {
                return Err(RecordError::ChangedDuringEval(abs.display().to_string()));
            }
        }
        let state = view.state(&id, env).map_err(|source| RecordError::Io {
            path: id.path.clone(),
            source,
        })?;
        inputs.push(Input { id, state });
    }
    Ok(Recorded {
        scheme: mounts.scheme,
        inputs,
    })
}

/// Virtual store mounts seen during evaluation, and which one is the flake.
struct Mounts {
    flake_dir: PathBuf,
    scheme: FlakeScheme,
    /// Mount point -> directory it mirrors, relative to the flake (`Some`) or
    /// immutable (`None`).
    mounts: HashMap<PathBuf, Option<PathBuf>>,
}

impl Mounts {
    fn new(ops: &[EvalOp], flake_dir: &Path) -> Result<Self, RecordError> {
        let mut scheme = None;
        let mut mounts = HashMap::new();
        for op in ops {
            let EvalOp::MountedInput { store_path, url } = op else {
                continue;
            };
            let local = local_url(url);
            let mapped = match &local {
                Some((s, dir)) if dir == flake_dir => {
                    scheme.get_or_insert(*s);
                    Some(PathBuf::new())
                }
                Some((_, dir)) => dir.strip_prefix(flake_dir).ok().map(Path::to_path_buf),
                None => None,
            };
            mounts.insert(store_path.clone(), mapped);
        }
        Ok(Self {
            flake_dir: flake_dir.to_path_buf(),
            scheme: scheme.ok_or_else(|| RecordError::FlakeNotMounted(flake_dir.to_path_buf()))?,
            mounts,
        })
    }

    fn is_flake(&self, store_path: &Path) -> bool {
        matches!(self.mounts.get(store_path), Some(Some(rel)) if rel.as_os_str().is_empty())
    }

    /// Anchor `path`, or `None` if it cannot change without `flake.lock`
    /// changing too.
    fn resolve(&self, path: &Path) -> Option<(Root, String)> {
        if let Ok(rest) = path.strip_prefix("/nix/store") {
            let mut components = rest.components();
            let name = components.next()?;
            let mount = Path::new("/nix/store").join(name);
            let base = self.mounts.get(&mount)?.as_ref()?;
            return Some((Root::Flake, rel_string(&base.join(components.as_path()))));
        }
        match path.strip_prefix(&self.flake_dir) {
            Ok(rel) => Some((Root::Flake, rel_string(rel))),
            // Nix's built-in sources (`<nix/fetchurl.nix>`, `«nix-internal»/...`)
            // are part of the evaluator.
            Err(_) if !path.is_absolute() => None,
            Err(_) => Some((Root::Absolute, path.to_string_lossy().into_owned())),
        }
    }
}

/// `git+file:///x?...` or `path:/x?...` -> scheme and directory.
fn local_url(url: &str) -> Option<(FlakeScheme, PathBuf)> {
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("git+file://") {
        (FlakeScheme::Git, rest)
    } else if let Some(rest) = url.strip_prefix("path:") {
        (FlakeScheme::Path, rest)
    } else {
        return None;
    };
    let path = rest.split(['?', '#']).next()?;
    if !path.starts_with('/') {
        return None;
    }
    // Flake URLs percent-encode unusual characters; the common ones used in
    // checkout paths do not need decoding.
    Some((scheme, PathBuf::from(path)))
}

fn rel_string(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn modified_since(path: &Path, kind: Kind, since: SystemTime) -> bool {
    let newer = |p: &Path| {
        fs::symlink_metadata(p)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t >= since)
    };
    match kind {
        Kind::Tree | Kind::Listing => {
            newer(path)
                || walkdir::WalkDir::new(path)
                    .min_depth(1)
                    .into_iter()
                    .flatten()
                    .any(|e| (kind == Kind::Tree || e.depth() == 1) && newer(e.path()))
        }
        Kind::File | Kind::Type => newer(path),
        Kind::Env | Kind::FlakeRev => false,
    }
}

/// The flake's tree as the evaluator sees it from one checkout.
pub struct FlakeView {
    dir: PathBuf,
    scheme: FlakeScheme,
    /// For git flakes, index entries below the prefetched paths.
    tracked: Option<BTreeSet<String>>,
}

impl FlakeView {
    pub fn new(dir: &Path, scheme: FlakeScheme) -> Self {
        Self {
            dir: dir.to_path_buf(),
            scheme,
            tracked: None,
        }
    }

    /// Load git visibility for every flake-relative input in one `git ls-files`.
    pub fn prefetch<'a>(&mut self, ids: impl IntoIterator<Item = &'a InputId>) -> io::Result<()> {
        if self.scheme != FlakeScheme::Git {
            return Ok(());
        }
        let specs: BTreeSet<&str> = ids
            .into_iter()
            .filter(|id| id.root == Root::Flake && id.kind != Kind::FlakeRev)
            .map(|id| if id.path.is_empty() { "." } else { id.path.as_str() })
            .collect();
        let mut tracked = BTreeSet::new();
        if !specs.is_empty() {
            let out = Command::new("git")
                .arg("-C")
                .arg(&self.dir)
                .args(["ls-files", "-z", "--"])
                .args(&specs)
                .env("GIT_LITERAL_PATHSPECS", "1")
                .output()?;
            if !out.status.success() {
                return Err(io::Error::other(format!(
                    "git ls-files in {} failed: {}",
                    self.dir.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            for entry in out.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
                tracked.insert(String::from_utf8_lossy(entry).into_owned());
            }
        }
        self.tracked = Some(tracked);
        Ok(())
    }

    fn physical_path(&self, id: &InputId) -> Option<PathBuf> {
        match id.root {
            Root::Flake if id.kind != Kind::FlakeRev => Some(self.dir.join(&id.path)),
            Root::Absolute => Some(PathBuf::from(&id.path)),
            _ => None,
        }
    }

    /// Visible descendants of `rel` (relative to it), or `None` if every file
    /// is visible.
    fn visible_below(&self, rel: &str) -> Option<Vec<String>> {
        let tracked = self.tracked.as_ref()?;
        let prefix = if rel.is_empty() {
            String::new()
        } else {
            format!("{rel}/")
        };
        Some(
            tracked
                .range(prefix.clone()..)
                .take_while(|p| p.starts_with(&prefix))
                .map(|p| p[prefix.len()..].to_owned())
                .collect(),
        )
    }

    fn is_visible(&self, rel: &str) -> bool {
        match &self.tracked {
            None => true,
            Some(tracked) => {
                rel.is_empty()
                    || tracked.contains(rel)
                    || self.visible_below(rel).is_some_and(|v| !v.is_empty())
            }
        }
    }

    /// The current state of `id`, comparable with a recorded state.
    pub fn state(&self, id: &InputId, env: &dyn Fn(&str) -> Option<String>) -> io::Result<String> {
        match id.root {
            Root::Env => Ok(match env(&id.path) {
                Some(v) => format!("set {}", blake3::hash(v.as_bytes()).to_hex()),
                None => "unset".into(),
            }),
            Root::Flake if id.kind == Kind::FlakeRev => git_rev_state(&self.dir),
            Root::Flake => {
                if !self.is_visible(&id.path) {
                    return Ok("absent".into());
                }
                let path = self.dir.join(&id.path);
                let below = if id.kind == Kind::Tree || id.kind == Kind::Listing {
                    self.visible_below(&id.path)
                } else {
                    None
                };
                path_state(&path, id.kind, below)
            }
            Root::Absolute => path_state(Path::new(&id.path), id.kind, None),
        }
    }
}

/// The flake's revision state: `HEAD` and whether the tree is dirty. Every
/// source-info attribute (`rev`, `dirtyRev`, `lastModified`, `revCount`) is a
/// function of it.
fn git_rev_state(dir: &Path) -> io::Result<String> {
    let git = |args: &[&str]| -> io::Result<String> {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let head = git(&["rev-parse", "--verify", "-q", "HEAD"])?;
    let dirty = !git(&["status", "--porcelain", "--untracked-files=no"])?.is_empty();
    Ok(if dirty { format!("{head}-dirty") } else { head })
}

fn path_state(path: &Path, kind: Kind, visible: Option<Vec<String>>) -> io::Result<String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok("absent".into()),
        Err(e) => return Err(e),
    };
    let ty = file_type_name(&meta);
    match kind {
        Kind::Type => Ok(ty.into()),
        Kind::File => {
            if !meta.is_file() {
                return Ok(ty.into());
            }
            Ok(format!("file {}", file_identity(path, &meta)?))
        }
        Kind::Listing => {
            if !meta.is_dir() {
                return Ok(ty.into());
            }
            let mut children = BTreeMap::new();
            match visible {
                Some(visible) => {
                    for rel in visible {
                        let first = rel.split('/').next().unwrap_or(&rel).to_owned();
                        if let Ok(m) = fs::symlink_metadata(path.join(&first)) {
                            children.insert(first, file_type_name(&m));
                        }
                    }
                }
                None => {
                    for entry in fs::read_dir(path)? {
                        let entry = entry?;
                        let m = entry.path().symlink_metadata()?;
                        children.insert(entry.file_name().to_string_lossy().into_owned(), file_type_name(&m));
                    }
                }
            }
            let listing: Vec<String> = children.into_iter().map(|(n, t)| format!("{t} {n}")).collect();
            Ok(format!("listing {}", blake3::hash(listing.join("\n").as_bytes()).to_hex()))
        }
        Kind::Tree => {
            if !meta.is_dir() {
                return path_state(path, if meta.is_file() { Kind::File } else { Kind::Type }, None);
            }
            let files: Vec<String> = match visible {
                Some(visible) => visible,
                None => walkdir::WalkDir::new(path)
                    .min_depth(1)
                    .sort_by_file_name()
                    .into_iter()
                    .filter_map(Result::ok)
                    .filter(|e| !e.file_type().is_dir())
                    .filter_map(|e| e.path().strip_prefix(path).ok().map(rel_string))
                    .collect(),
            };
            let mut hasher = blake3::Hasher::new();
            for rel in files {
                let file = path.join(&rel);
                let entry = match fs::symlink_metadata(&file) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e),
                    Ok(m) if m.is_symlink() => {
                        format!("symlink {rel} {}", fs::read_link(&file)?.display())
                    }
                    Ok(m) if m.is_file() => format!("file {rel} {}", file_identity(&file, &m)?),
                    Ok(m) => format!("{} {rel}", file_type_name(&m)),
                };
                hasher.update(entry.as_bytes());
                hasher.update(b"\n");
            }
            Ok(format!("tree {}", hasher.finalize().to_hex()))
        }
        Kind::Env | Kind::FlakeRev => unreachable!("not a path input"),
    }
}

fn file_type_name(meta: &fs::Metadata) -> &'static str {
    let ty = meta.file_type();
    if ty.is_symlink() {
        "symlink"
    } else if ty.is_dir() {
        "directory"
    } else if ty.is_file() {
        "regular"
    } else {
        "unknown"
    }
}

/// Content hash plus the executable bit, which is all a store copy keeps.
fn file_identity(path: &Path, meta: &fs::Metadata) -> io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut file = fs::File::open(path)?;
    io::copy(&mut file, &mut hasher)?;
    let exec = if meta.permissions().mode() & 0o100 != 0 { "x" } else { "-" };
    Ok(format!("{exec}{}", hasher.finalize().to_hex()))
}

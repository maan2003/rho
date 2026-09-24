//! The mapping from evaluation effects to inputs.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};

use devenv_core::eval_op::{EvalOp, ObservedKind};

use crate::eval_inputs::{FlakeScheme, Input, PathInput};

/// The inputs of one evaluation, as Nix observed them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecordedInputs {
    pub paths: Vec<PathInput>,
    /// Whether the flake's own source-info metadata was forced; its state is
    /// captured separately, see [`crate::eval_inputs::RevInputDesc`].
    pub flake_rev: bool,
}

impl RecordedInputs {
    pub fn into_inputs(self) -> Vec<Input> {
        self.paths.into_iter().map(Input::Path).collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("the evaluator never mounted the flake source at {0}")]
    FlakeNotMounted(PathBuf),
    #[error("the evaluator read local input {0}, which is outside the flake source at {1}")]
    OutsideSource(String, PathBuf),
    #[error("{0} changed while it was being evaluated")]
    ChangedDuringEval(PathBuf),
}

/// Convert the effects of evaluating a flake whose source tree is at `root`
/// into inputs, and report how the tree was fetched.
///
/// Observations through the virtual store path Nix mounted the tree (or a
/// local input inside it) at become `root`-relative. Observations of other
/// mounted inputs never appear: Nix only records reads of local inputs, and
/// everything else is locked by `flake.lock`, which is part of the cache key.
pub fn record_inputs(ops: &[EvalOp], root: &Path) -> Result<(FlakeScheme, RecordedInputs), RecordError> {
    let mounts = Mounts::new(ops, root)?;
    let mut paths: BTreeMap<(ObservedKind, FlakeScheme, PathBuf), String> = BTreeMap::new();
    let mut flake_rev = false;

    for op in ops {
        match op {
            EvalOp::MountedInput { .. } => {}
            EvalOp::ForcedInputAttr { store_path, .. } => flake_rev |= mounts.is_root(store_path),
            EvalOp::Observed { source, kind, value } => {
                let Some((scheme, path)) = mounts.resolve(source)? else {
                    continue;
                };
                let previous = paths.insert((*kind, scheme, path.clone()), value.clone());
                if previous.is_some_and(|previous| previous != *value) {
                    return Err(RecordError::ChangedDuringEval(root.join(path)));
                }
            }
        }
    }
    let paths = paths
        .into_iter()
        .map(|((kind, scheme, path), value)| PathInput { kind, scheme, path, value })
        .collect();
    Ok((mounts.scheme, RecordedInputs { paths, flake_rev }))
}

/// Virtual store mounts seen during evaluation, and which one is the flake's
/// source tree.
struct Mounts {
    root: PathBuf,
    scheme: FlakeScheme,
    mounts: HashMap<PathBuf, Mount>,
}

enum Mount {
    /// A local tree at a `root`-relative directory.
    Inside(FlakeScheme, PathBuf),
    /// A local tree elsewhere, by URL.
    Outside(String),
    /// Locked.
    Locked,
}

impl Mounts {
    fn new(ops: &[EvalOp], root: &Path) -> Result<Self, RecordError> {
        let mut scheme = None;
        let mut mounts = HashMap::new();
        for op in ops {
            let EvalOp::MountedInput { store_path, url } = op else {
                continue;
            };
            let mount = match local_url(url) {
                Some((s, dir)) => match dir.strip_prefix(root) {
                    Ok(rel) => {
                        if rel.as_os_str().is_empty() {
                            scheme.get_or_insert(s);
                        }
                        Mount::Inside(s, rel.to_path_buf())
                    }
                    Err(_) => Mount::Outside(url.clone()),
                },
                None => Mount::Locked,
            };
            mounts.insert(store_path.clone(), mount);
        }
        Ok(Self {
            root: root.to_path_buf(),
            scheme: scheme.ok_or_else(|| RecordError::FlakeNotMounted(root.to_path_buf()))?,
            mounts,
        })
    }

    fn is_root(&self, store_path: &Path) -> bool {
        matches!(self.mounts.get(store_path), Some(Mount::Inside(_, rel)) if rel.as_os_str().is_empty())
    }

    /// The scheme and `root`-relative path of an observed virtual store path.
    fn resolve(&self, path: &Path) -> Result<Option<(FlakeScheme, PathBuf)>, RecordError> {
        let Ok(rest) = path.strip_prefix("/nix/store") else {
            return Ok(None);
        };
        let mut components = rest.components();
        let Some(first) = components.next() else {
            return Ok(None);
        };
        match self.mounts.get(&Path::new("/nix/store").join(first)) {
            Some(Mount::Inside(scheme, base)) => Ok(Some((*scheme, normalize(&base.join(components.as_path()))))),
            Some(Mount::Outside(url)) => Err(RecordError::OutsideSource(url.clone(), self.root.clone())),
            Some(Mount::Locked) | None => Ok(None),
        }
    }
}

/// `git+file:///x?...` or `path:/x?...` -> scheme and directory of the
/// fetched tree (the repository for git, even with `?dir=`).
fn local_url(url: &str) -> Option<(FlakeScheme, PathBuf)> {
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("git+file://") {
        (FlakeScheme::Git, rest)
    } else if let Some(rest) = url.strip_prefix("path:") {
        (FlakeScheme::Path, rest)
    } else {
        return None;
    };
    let path = rest.split(['?', '#']).next()?;
    // Flake URLs percent-encode unusual characters; the common ones used in
    // checkout paths do not need decoding.
    path.starts_with('/').then(|| (scheme, PathBuf::from(path)))
}

fn normalize(rel: &Path) -> PathBuf {
    rel.components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT_MOUNT: &str = "/nix/store/00000000000000000000000000000000-source";
    const NIXPKGS_MOUNT: &str = "/nix/store/11111111111111111111111111111111-source";
    const SUB_MOUNT: &str = "/nix/store/22222222222222222222222222222222-source";

    fn mount(store_path: &str, url: String) -> EvalOp {
        EvalOp::MountedInput { store_path: store_path.into(), url }
    }

    fn observed(source: &str, kind: ObservedKind, value: &str) -> EvalOp {
        EvalOp::Observed { source: source.into(), kind, value: value.into() }
    }

    fn input(kind: ObservedKind, scheme: FlakeScheme, path: &str, value: &str) -> PathInput {
        PathInput { kind, scheme, path: path.into(), value: value.into() }
    }

    #[test]
    fn observations_become_root_relative_inputs() {
        let root = Path::new("/work/checkout");
        let ops = vec![
            mount(ROOT_MOUNT, format!("git+file://{}?dir=sub", root.display())),
            mount(NIXPKGS_MOUNT, "github:NixOS/nixpkgs/abc".into()),
            mount(SUB_MOUNT, format!("path:{}/sub/vendored?narHash=x", root.display())),
            observed(&format!("{ROOT_MOUNT}/sub/flake.nix"), ObservedKind::File, "aa"),
            observed(ROOT_MOUNT, ObservedKind::Dir, "bb"),
            observed(&format!("{SUB_MOUNT}/a.nix"), ObservedKind::File, "cc"),
            // Repeated identical observations collapse.
            observed(&format!("{ROOT_MOUNT}/sub/flake.nix"), ObservedKind::File, "aa"),
            EvalOp::ForcedInputAttr { store_path: NIXPKGS_MOUNT.into(), name: "rev".into() },
        ];
        let (scheme, recorded) = record_inputs(&ops, root).unwrap();
        assert_eq!(scheme, FlakeScheme::Git);
        assert_eq!(
            recorded.paths,
            vec![
                input(ObservedKind::File, FlakeScheme::Git, "sub/flake.nix", "aa"),
                input(ObservedKind::File, FlakeScheme::Path, "sub/vendored/a.nix", "cc"),
                input(ObservedKind::Dir, FlakeScheme::Git, "", "bb"),
            ]
        );
        // Only the flake's own source-info is an input; other inputs' is locked.
        assert!(!recorded.flake_rev);
    }

    #[test]
    fn conflicting_observations_are_not_recorded() {
        let root = Path::new("/work/checkout");
        let ops = vec![
            mount(ROOT_MOUNT, format!("path:{}", root.display())),
            observed(&format!("{ROOT_MOUNT}/a"), ObservedKind::File, "aa"),
            observed(&format!("{ROOT_MOUNT}/a"), ObservedKind::File, "bb"),
        ];
        assert!(matches!(record_inputs(&ops, root), Err(RecordError::ChangedDuringEval(_))));
    }

    #[test]
    fn local_inputs_outside_the_source_are_not_recorded() {
        let root = Path::new("/work/checkout");
        let ops = vec![
            mount(ROOT_MOUNT, format!("path:{}", root.display())),
            mount(SUB_MOUNT, "git+file:///elsewhere".into()),
            observed(&format!("{SUB_MOUNT}/a"), ObservedKind::File, "aa"),
        ];
        assert!(matches!(record_inputs(&ops, root), Err(RecordError::OutsideSource(..))));
    }

    #[test]
    fn flake_must_be_mounted() {
        let err = record_inputs(&[], Path::new("/work/checkout")).unwrap_err();
        assert!(matches!(err, RecordError::FlakeNotMounted(_)));
    }
}

//! Cache keys and the mapping from evaluation effects to input identities.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::{Component, Path, PathBuf};

use devenv_core::eval_op::EvalOp;

use crate::eval_inputs::{
    Anchor, Checkout, EnvInputDesc, FileHashes, FileInputDesc, FlakeScheme, Input, RevInputDesc,
};

/// Cache key for an evaluation operation.
///
/// The key covers everything an evaluation depends on that is not an observed
/// input: the attribute, how the flake is fetched, and caller-supplied context
/// such as the system, `flake.lock` and the evaluator version. It deliberately
/// excludes the flake's location, so every checkout shares entries.
#[derive(Clone, Debug)]
pub struct EvalCacheKey {
    /// Hash of the attribute name, scheme and context
    pub key_hash: String,
    /// Human-readable attribute name for debugging
    pub attr_name: String,
    pub scheme: FlakeScheme,
}

impl EvalCacheKey {
    pub fn new(attr_name: &str, scheme: FlakeScheme, context: &[&[u8]]) -> Self {
        let mut hasher = blake3::Hasher::new();
        for part in [b"rho-eval-v1".as_slice(), attr_name.as_bytes(), scheme.as_str().as_bytes()]
            .into_iter()
            .chain(context.iter().copied())
        {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part);
        }
        Self {
            key_hash: hasher.finalize().to_hex().to_string(),
            attr_name: attr_name.to_owned(),
            scheme,
        }
    }
}

/// Distinct inputs observed during an evaluation, before their state is
/// captured.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvalInputIdentities {
    /// Path -> whether it was copied to the store (recursive observation wins).
    pub paths: BTreeMap<(Anchor, PathBuf), bool>,
    pub envs: BTreeSet<String>,
    /// Whether the flake's own source-info metadata was forced.
    pub flake_rev: bool,
}

impl EvalInputIdentities {
    fn insert_path(&mut self, anchor: Anchor, path: PathBuf, recursive: bool) {
        self.paths
            .entry((anchor, path))
            .and_modify(|existing| *existing |= recursive)
            .or_insert(recursive);
    }

    /// Capture the current state of every identity in `checkout`.
    pub fn to_inputs(
        &self,
        checkout: &Checkout,
        hashes: &mut dyn FileHashes,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> io::Result<Vec<Input>> {
        let mut inputs = Vec::with_capacity(self.paths.len() + self.envs.len() + 1);
        for ((anchor, path), recursive) in &self.paths {
            inputs.push(Input::File(FileInputDesc::new(
                *anchor,
                path.clone(),
                *recursive,
                checkout,
                hashes,
            )?));
        }
        for name in &self.envs {
            inputs.push(Input::Env(EnvInputDesc::new(name.clone(), env)));
        }
        if self.flake_rev {
            inputs.push(Input::FlakeRev(RevInputDesc::new(checkout)?));
        }
        Ok(inputs)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("the evaluator never mounted the flake at {0}")]
    FlakeNotMounted(PathBuf),
}

/// Convert the effects of evaluating the flake at `flake_dir` into input
/// identities, and report how the flake was fetched.
///
/// Paths inside the flake (physically, or through the virtual store path Nix
/// mounted it at) become [`Anchor::Flake`]-relative. Paths inside other
/// mounted inputs are dropped: they are locked, and `flake.lock` is part of
/// the cache key. The same holds for the rest of `/nix/store` (immutable) and
/// Nix's built-in sources, which are not absolute paths.
pub fn ops_to_identities(
    ops: &[EvalOp],
    flake_dir: &Path,
) -> Result<(FlakeScheme, EvalInputIdentities), RecordError> {
    let mounts = Mounts::new(ops, flake_dir)?;
    let mut identities = EvalInputIdentities::default();

    for op in ops {
        let (source, recursive) = match op {
            EvalOp::ReadFile { source }
            | EvalOp::ReadDir { source }
            | EvalOp::ReadFileType { source }
            | EvalOp::HashFile { source, .. }
            | EvalOp::PathExists { source }
            | EvalOp::EvaluatedFile { source, .. } => (source, false),
            EvalOp::CopiedSource { source, .. } | EvalOp::FilteredSource { source, .. } => {
                (source, true)
            }
            EvalOp::GetEnv { name } => {
                identities.envs.insert(name.clone());
                continue;
            }
            EvalOp::ForcedInputAttr { store_path, .. } => {
                identities.flake_rev |= mounts.is_flake(store_path);
                continue;
            }
            EvalOp::MountedInput { .. } => continue,
        };
        if let Some((anchor, path)) = mounts.resolve(source) {
            identities.insert_path(anchor, path, recursive);
        }
    }
    Ok((mounts.scheme, identities))
}

/// Virtual store mounts seen during evaluation, and which one is the flake.
struct Mounts {
    flake_dir: PathBuf,
    scheme: FlakeScheme,
    /// Mount point -> directory it mirrors, relative to the flake (`Some`) or
    /// locked (`None`).
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
            let mapped = match local_url(url) {
                Some((s, dir)) if dir == flake_dir => {
                    scheme.get_or_insert(s);
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

    fn resolve(&self, path: &Path) -> Option<(Anchor, PathBuf)> {
        if let Ok(rest) = path.strip_prefix("/nix/store") {
            let mut components = rest.components();
            let mount = Path::new("/nix/store").join(components.next()?);
            let base = self.mounts.get(&mount)?.as_ref()?;
            return Some((Anchor::Flake, normalize(&base.join(components.as_path()))));
        }
        if !path.is_absolute() {
            // Nix's built-in sources (`<nix/fetchurl.nix>`, `«nix-internal»/...`).
            return None;
        }
        match path.strip_prefix(&self.flake_dir) {
            Ok(rel) => Some((Anchor::Flake, normalize(rel))),
            Err(_) => Some((Anchor::Absolute, path.to_path_buf())),
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

    const FLAKE_MOUNT: &str = "/nix/store/00000000000000000000000000000000-source";
    const NIXPKGS_MOUNT: &str = "/nix/store/11111111111111111111111111111111-source";

    fn ops(flake: &Path) -> Vec<EvalOp> {
        vec![
            EvalOp::MountedInput {
                store_path: FLAKE_MOUNT.into(),
                url: format!("git+file://{}?dir=", flake.display()),
            },
            EvalOp::MountedInput {
                store_path: NIXPKGS_MOUNT.into(),
                url: "github:NixOS/nixpkgs/abc".into(),
            },
            EvalOp::EvaluatedFile { source: flake.join("flake.nix"), cached: false },
            EvalOp::ReadFile { source: Path::new(FLAKE_MOUNT).join("nix/a.nix") },
            EvalOp::CopiedSource {
                source: Path::new(FLAKE_MOUNT).join("nix"),
                target: "/nix/store/x-nix".into(),
            },
            EvalOp::ReadFile { source: Path::new(NIXPKGS_MOUNT).join("lib.nix") },
            EvalOp::EvaluatedFile {
                source: "«nix-internal»/derivation-internal.nix".into(),
                cached: false,
            },
            EvalOp::PathExists { source: "/home/someone/.config/nixpkgs/config.nix".into() },
            EvalOp::GetEnv { name: "HOME".into() },
            EvalOp::ForcedInputAttr { store_path: NIXPKGS_MOUNT.into(), name: "rev".into() },
        ]
    }

    #[test]
    fn effects_become_flake_relative_identities() {
        let flake = Path::new("/work/checkout");
        let (scheme, ids) = ops_to_identities(&ops(flake), flake).unwrap();
        assert_eq!(scheme, FlakeScheme::Git);
        let paths: Vec<_> = ids.paths.into_iter().collect();
        assert_eq!(
            paths,
            vec![
                ((Anchor::Flake, "flake.nix".into()), false),
                ((Anchor::Flake, "nix".into()), true),
                ((Anchor::Flake, "nix/a.nix".into()), false),
                ((Anchor::Absolute, "/home/someone/.config/nixpkgs/config.nix".into()), false),
            ]
        );
        assert_eq!(ids.envs.into_iter().collect::<Vec<_>>(), vec!["HOME".to_string()]);
        // Only the flake's own source-info is an input; other inputs' is locked.
        assert!(!ids.flake_rev);
    }

    #[test]
    fn flake_must_be_mounted() {
        let err = ops_to_identities(&[], Path::new("/work/checkout")).unwrap_err();
        assert!(matches!(err, RecordError::FlakeNotMounted(_)));
    }

    #[test]
    fn keys_depend_on_every_part() {
        let key = |attr, scheme, lock: &[u8]| EvalCacheKey::new(attr, scheme, &[b"x86_64-linux", lock]).key_hash;
        let base = key("devShells.x.default", FlakeScheme::Git, b"{}");
        assert_eq!(base, key("devShells.x.default", FlakeScheme::Git, b"{}"));
        assert_ne!(base, key("devShells.x.other", FlakeScheme::Git, b"{}"));
        assert_ne!(base, key("devShells.x.default", FlakeScheme::Path, b"{}"));
        assert_ne!(base, key("devShells.x.default", FlakeScheme::Git, b"{ }"));
    }
}

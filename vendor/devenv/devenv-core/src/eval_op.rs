//! Evaluation operation types and structured Nix effect parsing.
//!
//! Nix emits evaluation dependencies through a dedicated one-shot callback.
//! This is the typed form of the effects that cache invalidation consumes.
//! The evaluator runs in pure mode with `record-input-reads`, so what it read
//! from mutable local inputs is fully described by [`EvalOp::Observed`]; the
//! per-builtin effects Nix also emits are ignored.

use std::path::PathBuf;
use std::sync::Arc;

/// An evaluator dependency reported by Nix.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EvalOp {
    /// Mounted a fetched input at a virtual store path. Effects on paths
    /// below `store_path` observe that input's tree.
    MountedInput { store_path: PathBuf, url: String },
    /// Forced a source-info metadata attribute (`rev`, `lastModified`, ...)
    /// of the input mounted at `store_path`.
    ForcedInputAttr { store_path: PathBuf, name: String },
    /// Read `source`, below a mounted local input, and saw `value`.
    Observed {
        source: PathBuf,
        kind: ObservedKind,
        value: String,
    },
}

/// What a read of a mounted local input observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObservedKind {
    /// The path's type: `regular`, `executable`, `directory`, `symlink`,
    /// `other` or `missing`.
    Stat,
    /// BLAKE3 of the file's contents, base16.
    File,
    /// BLAKE3 of the directory's entry names, each followed by a NUL byte,
    /// in byte order; base16.
    Dir,
    /// The symlink's target.
    Link,
}

impl ObservedKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ObservedKind::Stat => "stat",
            ObservedKind::File => "file",
            ObservedKind::Dir => "dir",
            ObservedKind::Link => "link",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stat" => Some(ObservedKind::Stat),
            "file" => Some(ObservedKind::File),
            "dir" => Some(ObservedKind::Dir),
            "link" => Some(ObservedKind::Link),
            _ => None,
        }
    }
}

impl EvalOp {
    /// Extract an operation from the dedicated one-shot evaluator effect
    /// callback. This is the canonical wire conversion used by the Nix FFI.
    pub fn from_effect(kind: &str, subject: &str, detail: Option<&str>) -> Option<Self> {
        let path = || PathBuf::from(subject);
        let observed = |kind| {
            Some(EvalOp::Observed {
                source: path(),
                kind,
                value: detail?.to_owned(),
            })
        };

        match (kind, detail) {
            ("mount-input", Some(url)) => Some(EvalOp::MountedInput {
                store_path: path(),
                url: url.to_owned(),
            }),
            ("input-attr", Some(name)) => Some(EvalOp::ForcedInputAttr {
                store_path: path(),
                name: name.to_owned(),
            }),
            ("observed-stat", _) => observed(ObservedKind::Stat),
            ("observed-file", _) => observed(ObservedKind::File),
            ("observed-dir", _) => observed(ObservedKind::Dir),
            ("observed-link", _) => observed(ObservedKind::Link),
            _ => None,
        }
    }
}

/// Observer trait for receiving evaluation operations.
///
/// Implementations can be registered with `NixLogBridge` to receive the
/// dependencies of an evaluation.
pub trait OpObserver: Send + Sync + 'static {
    /// Called when an operation is observed during evaluation.
    fn record(&self, op: EvalOp);
}

/// Wrapper to allow `Arc<dyn OpObserver>` to implement `OpObserver`.
impl OpObserver for Arc<dyn OpObserver> {
    fn record(&self, op: EvalOp) {
        (**self).record(op);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_eval_effects() {
        let observed = |kind, value: &str| EvalOp::Observed {
            source: "/nix/store/x-source/a".into(),
            kind,
            value: value.into(),
        };
        let cases = [
            (
                "mount-input",
                "/nix/store/x-source",
                Some("git+file:///src/x"),
                EvalOp::MountedInput {
                    store_path: "/nix/store/x-source".into(),
                    url: "git+file:///src/x".into(),
                },
            ),
            (
                "input-attr",
                "/nix/store/x-source",
                Some("rev"),
                EvalOp::ForcedInputAttr {
                    store_path: "/nix/store/x-source".into(),
                    name: "rev".into(),
                },
            ),
            ("observed-stat", "/nix/store/x-source/a", Some("regular"), observed(ObservedKind::Stat, "regular")),
            ("observed-file", "/nix/store/x-source/a", Some("ab"), observed(ObservedKind::File, "ab")),
            ("observed-dir", "/nix/store/x-source/a", Some("cd"), observed(ObservedKind::Dir, "cd")),
            // A symlink may point at the empty string.
            ("observed-link", "/nix/store/x-source/a", Some(""), observed(ObservedKind::Link, "")),
        ];

        for (kind, subject, detail, expected) in cases {
            assert_eq!(EvalOp::from_effect(kind, subject, detail), Some(expected));
        }
    }

    #[test]
    fn rejects_unknown_or_malformed_effects() {
        assert_eq!(EvalOp::from_effect("unknown", "/file", None), None);
        assert_eq!(EvalOp::from_effect("read-file", "/file", None), None);
        assert_eq!(EvalOp::from_effect("mount-input", "/nix/store/x", None), None);
        assert_eq!(EvalOp::from_effect("observed-file", "/file", None), None);
        for kind in [ObservedKind::Stat, ObservedKind::File, ObservedKind::Dir, ObservedKind::Link] {
            assert_eq!(ObservedKind::parse(kind.as_str()), Some(kind));
        }
    }
}

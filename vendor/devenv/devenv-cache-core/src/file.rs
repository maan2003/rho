use crate::error::{CacheError, CacheResult};
use blake3::Hasher;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use walkdir::WalkDir;

/// Get file metadata with consistent error handling
fn get_metadata<P: AsRef<Path>>(path: P) -> CacheResult<std::fs::Metadata> {
    let path = path.as_ref();
    std::fs::metadata(path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            CacheError::FileNotFound(path.to_path_buf())
        } else {
            e.into()
        }
    })
}

/// Helper to open a file with consistent error handling
fn open_file<P: AsRef<Path>>(path: P) -> CacheResult<std::fs::File> {
    let path = path.as_ref();
    std::fs::File::open(path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            CacheError::FileNotFound(path.to_path_buf())
        } else {
            e.into()
        }
    })
}

/// Compute a hash of a file's contents
pub fn compute_file_hash<P: AsRef<Path>>(path: P) -> CacheResult<String> {
    let path = path.as_ref();
    let mut file = open_file(path)?;
    let mut hasher = Hasher::new();

    io::copy(&mut file, &mut hasher).map_err(|e| CacheError::HashFailure {
        path: path.to_path_buf(),
        reason: format!("Failed to read file: {e}"),
    })?;

    Ok(hasher.finalize().to_hex().to_string())
}

/// Compute the content identity of a regular file copied into the Nix store.
///
/// Nix archives preserve the owner's executable bit in addition to file
/// contents. Other permission bits are normalized and therefore intentionally
/// ignored.
pub fn compute_source_file_hash<P: AsRef<Path>>(path: P) -> CacheResult<String> {
    let path = path.as_ref();
    let content_hash = compute_file_hash(path)?;
    let metadata = get_metadata(path)?;
    Ok(source_file_hash(&content_hash, is_executable(&metadata)))
}

/// Combine a [`compute_file_hash`] result with the executable bit into the
/// identity [`compute_source_file_hash`] returns, for callers that already
/// know the content hash.
pub fn source_file_hash(content_hash: &str, executable: bool) -> String {
    compute_string_hash(&format!("file {content_hash} executable={executable}"))
}

/// Whether Nix would store the file as executable.
pub fn is_executable(metadata: &std::fs::Metadata) -> bool {
    metadata.permissions().mode() & 0o100 != 0
}

/// Compute a content-only hash of a directory's contents, recursively.
///
/// Unlike [`compute_directory_hash`], this ignores modification times, so
/// touching a file without changing its contents does not change the hash. It
/// also keys entries by their path relative to `path`, so the same tree hashes
/// identically regardless of where it lives on disk. This mirrors how Nix
/// hashes a source tree copied into the store, and is what the eval cache needs
/// to detect edits to files nested inside a copied source directory.
///
/// Returns the hash of the empty string for an empty directory.
pub fn compute_directory_content_hash<P: AsRef<Path>>(path: P) -> CacheResult<String> {
    compute_directory_content_hash_with(path.as_ref(), &|_| true, &mut |file| {
        compute_source_file_hash(file)
    })
}

/// [`compute_directory_content_hash`] over a subset of the tree, with a custom
/// file identity.
///
/// `visible` receives each entry's path relative to `path`; a rejected
/// directory is skipped together with everything below it, matching how a git
/// flake only exposes tracked files. `source_hash` must return what
/// [`compute_source_file_hash`] would, but may answer from a cache.
pub fn compute_directory_content_hash_with(
    path: &Path,
    visible: &dyn Fn(&str) -> bool,
    source_hash: &mut dyn FnMut(&Path) -> CacheResult<String>,
) -> CacheResult<String> {
    let mut entries = Vec::new();
    let relative = |entry: &walkdir::DirEntry| {
        entry
            .path()
            .strip_prefix(path)
            .unwrap_or_else(|_| entry.path())
            .to_string_lossy()
            .into_owned()
    };

    // Skip the root directory itself, sort by file name for consistent ordering
    let walk = WalkDir::new(path)
        .min_depth(1)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| visible(&relative(entry)));
    for entry in walk {
        match entry {
            Ok(entry) => {
                let rel = relative(&entry);
                let file_type = entry.file_type();

                if file_type.is_dir() {
                    entries.push(format!("dir {rel}"));
                } else if file_type.is_symlink() {
                    // Record the link target, not its contents, matching Nix.
                    let target = std::fs::read_link(entry.path())
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    entries.push(format!("symlink {rel} -> {target}"));
                } else {
                    match source_hash(entry.path()) {
                        Ok(hash) => entries.push(format!("file {rel} {hash}")),
                        Err(_) => entries.push(format!("file_error {rel}")),
                    }
                }
            }
            Err(e) => {
                // Include error entries as well to detect when errors change
                entries.push(format!("error {e}"));
            }
        }
    }

    Ok(compute_string_hash(&entries.join("\n")))
}

/// Compute a hash of a string
pub fn compute_string_hash(content: &str) -> String {
    let hash = blake3::hash(content.as_bytes());
    hash.to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[test]
    fn test_file_hash() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        // Create test file
        {
            let mut file = File::create(&file_path).unwrap();
            file.write_all(b"test content").unwrap();
        }

        let hash = compute_file_hash(&file_path).unwrap();
        assert!(!hash.is_empty());

        // Same content should produce same hash
        let hash2 = compute_file_hash(&file_path).unwrap();
        assert_eq!(hash, hash2);

        // Different content should produce different hash
        {
            let mut file = File::create(&file_path).unwrap();
            file.write_all(b"different content").unwrap();
        }

        let hash3 = compute_file_hash(&file_path).unwrap();
        assert_ne!(hash, hash3);
    }

    #[test]
    fn test_directory_content_hash_detects_nested_changes() {
        let temp_dir = TempDir::new().unwrap();
        let nested = temp_dir.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        let file_path = nested.join("c.txt");
        std::fs::write(&file_path, b"hello").unwrap();

        let hash = compute_directory_content_hash(temp_dir.path()).unwrap();
        assert!(!hash.is_empty());

        // Same contents hash identically.
        assert_eq!(
            hash,
            compute_directory_content_hash(temp_dir.path()).unwrap()
        );

        // Editing a deeply nested file changes the hash, even though the
        // directory listing is unchanged.
        std::fs::write(&file_path, b"world").unwrap();
        assert_ne!(
            hash,
            compute_directory_content_hash(temp_dir.path()).unwrap()
        );
    }

    #[test]
    fn test_directory_content_hash_is_location_independent() {
        // The same tree at two different locations hashes identically, because
        // entries are keyed by their path relative to the root.
        let make_tree = || {
            let dir = TempDir::new().unwrap();
            std::fs::create_dir(dir.path().join("src")).unwrap();
            std::fs::write(dir.path().join("src").join("main.rs"), b"fn main() {}").unwrap();
            dir
        };
        let a = make_tree();
        let b = make_tree();
        assert_eq!(
            compute_directory_content_hash(a.path()).unwrap(),
            compute_directory_content_hash(b.path()).unwrap(),
        );
    }

    #[test]
    fn test_source_file_hash_detects_executable_bit_changes() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("script.sh");
        std::fs::write(&file_path, b"#!/bin/sh\nexit 0\n").unwrap();

        let mut permissions = std::fs::metadata(&file_path).unwrap().permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&file_path, permissions.clone()).unwrap();
        let non_executable = compute_source_file_hash(&file_path).unwrap();

        permissions.set_mode(0o744);
        std::fs::set_permissions(&file_path, permissions).unwrap();
        let executable = compute_source_file_hash(&file_path).unwrap();

        assert_ne!(non_executable, executable);
    }

    #[test]
    fn test_directory_content_hash_detects_executable_bit_changes() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("script.sh");
        std::fs::write(&file_path, b"#!/bin/sh\nexit 0\n").unwrap();

        let mut permissions = std::fs::metadata(&file_path).unwrap().permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&file_path, permissions.clone()).unwrap();
        let non_executable = compute_directory_content_hash(temp_dir.path()).unwrap();

        permissions.set_mode(0o744);
        std::fs::set_permissions(&file_path, permissions).unwrap();
        let executable = compute_directory_content_hash(temp_dir.path()).unwrap();

        assert_ne!(non_executable, executable);
    }

    #[test]
    fn test_directory_content_hash_skips_invisible_entries() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::create_dir(temp_dir.path().join("src")).unwrap();
        std::fs::write(temp_dir.path().join("src").join("main.rs"), b"fn main() {}").unwrap();
        let visible = |rel: &str| !rel.starts_with("target");
        let hash = |dir: &Path| {
            compute_directory_content_hash_with(dir, &visible, &mut |f| compute_source_file_hash(f))
                .unwrap()
        };
        let before = hash(temp_dir.path());
        assert_eq!(before, compute_directory_content_hash(temp_dir.path()).unwrap());

        std::fs::create_dir(temp_dir.path().join("target")).unwrap();
        std::fs::write(temp_dir.path().join("target").join("out"), b"junk").unwrap();
        assert_eq!(before, hash(temp_dir.path()));
        assert_ne!(before, compute_directory_content_hash(temp_dir.path()).unwrap());
    }
}

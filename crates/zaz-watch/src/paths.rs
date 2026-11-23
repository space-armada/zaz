//! The path normal form shared by watched sources and watcher events.
//!
//! macOS reports real paths, so a project under a symlink such as `/var/folders` would never match
//! a source named through the link. Both sides are therefore compared in one form. The parent
//! directory is canonicalized through its deepest existing ancestor. The last component is kept as
//! written and never resolved, so a source that does not exist yet and a deleted source keep one
//! key across create and delete.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Put `path` in the normal form described in the module docs.
pub fn normalize(path: &Path) -> PathBuf {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());

    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => canonical_through_ancestor(parent).join(name),
        _ => path,
    }
}

/// [`normalize`] with the parent resolutions of one batch of paths remembered in `parents`.
///
/// A burst of events shares a handful of directories, so the canonicalize calls are made once per
/// directory.
pub fn normalize_cached(path: &Path, parents: &mut HashMap<PathBuf, PathBuf>) -> PathBuf {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());

    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => parents
            .entry(parent.to_path_buf())
            .or_insert_with(|| canonical_through_ancestor(parent))
            .join(name),
        _ => path,
    }
}

/// Resolve every component of `dir`, through its deepest existing ancestor.
pub fn resolve_dir(dir: &Path) -> PathBuf {
    let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());

    canonical_through_ancestor(&dir)
}

/// Canonicalize `dir`, or its deepest existing ancestor with the missing tail appended as written.
fn canonical_through_ancestor(dir: &Path) -> PathBuf {
    let mut tail: Vec<OsString> = Vec::new();
    let mut current = dir.to_path_buf();

    loop {
        if let Ok(mut resolved) = current.canonicalize() {
            resolved.extend(tail.iter().rev());
            return resolved;
        }

        match (current.file_name(), current.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                current = parent.to_path_buf();
            }
            _ => return dir.to_path_buf(),
        }
    }
}

/// The fully resolved target of `path` when it is a symlink that points at something.
pub fn link_target(path: &Path) -> Option<PathBuf> {
    let is_link = std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false);

    if !is_link {
        return None;
    }

    path.canonicalize().ok().map(|target| normalize(&target))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_existing_parent_resolves_and_the_last_component_stays() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        std::fs::create_dir(real.join("real")).unwrap();
        std::os::unix::fs::symlink(real.join("real"), real.join("alias")).unwrap();

        assert_eq!(
            normalize(&real.join("alias").join(".env")),
            real.join("real").join(".env")
        );
    }

    #[test]
    fn a_missing_parent_keeps_its_tail_under_the_deepest_existing_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();

        assert_eq!(
            normalize(&real.join("a").join("b").join(".env")),
            real.join("a").join("b").join(".env")
        );
    }

    #[test]
    fn a_symlinked_last_component_is_not_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        std::fs::write(real.join("target"), "x").unwrap();
        std::os::unix::fs::symlink(real.join("target"), real.join("link")).unwrap();

        assert_eq!(normalize(&real.join("link")), real.join("link"));
        assert_eq!(link_target(&real.join("link")), Some(real.join("target")));
        assert_eq!(link_target(&real.join("target")), None);
    }

    #[test]
    fn a_dangling_link_has_no_target() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        std::os::unix::fs::symlink(real.join("missing"), real.join("link")).unwrap();

        assert_eq!(link_target(&real.join("link")), None);
    }
}

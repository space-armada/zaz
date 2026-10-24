//! File system watcher implementation.

use crate::paths::{normalize, normalize_cached};
use crate::{EventBatch, PatternSet, WatchError};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use tokio::sync::broadcast;

/// Configuration for the file watcher.
#[derive(Debug, Clone)]
pub struct WatcherConfig {
    /// Root directory to watch.
    pub root: PathBuf,

    /// Debounce duration.
    pub debounce: Duration,

    /// Default patterns to ignore.
    pub default_ignores: Vec<String>,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("."),
            debounce: Duration::from_millis(100),
            default_ignores: default_ignores(),
        }
    }
}

/// Default ignore patterns for common files that shouldn't trigger rebuilds.
pub fn default_ignores() -> Vec<String> {
    vec![
        "**/.git/**".to_string(),
        "**/.hg/**".to_string(),
        "**/.svn/**".to_string(),
        "**/.DS_Store".to_string(),
        "**/node_modules/**".to_string(),
        "**/target/**".to_string(),
        "**/*.swp".to_string(),
        "**/*~".to_string(),
        "**/#*#".to_string(),
        "**/.#*".to_string(),
    ]
}

/// A file event from the watcher.
#[derive(Debug, Clone)]
pub struct FileEvent {
    /// The path that changed.
    pub path: PathBuf,

    /// The kind of change.
    pub kind: FileEventKind,

    /// The registered source this event concerns, as [`Watcher::set_sources`] normalized it.
    ///
    /// An event on a source symlink and an event on the file the link points at both name the
    /// link. It is `None` for an event that only the group patterns matched.
    pub source: Option<PathBuf>,
}

/// The kind of file change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEventKind {
    Created,
    Modified,
    Deleted,
    Renamed,
}

/// One watched source and the keys that name it.
#[derive(Debug, Clone)]
struct Source {
    /// The source path in normal form. The last component is never resolved.
    key: PathBuf,

    /// The resolved target while the source is a symlink.
    ///
    /// notify reports an edit made through a link under the target's own entry, so the target
    /// is a second key for the same source.
    target: Option<PathBuf>,
}

/// A source that could not be watched, and why.
#[derive(Debug)]
pub struct SourceIssue {
    /// The directory that could not be watched.
    pub dir: PathBuf,

    /// What the backend said.
    pub error: WatchError,
}

/// File system watcher that monitors for changes.
pub struct Watcher {
    watcher: RecommendedWatcher,
    receiver: mpsc::Receiver<Result<notify::Event, notify::Error>>,
    sender: broadcast::Sender<Vec<FileEvent>>,
    config: WatcherConfig,
    batch: EventBatch,

    /// Directories watched recursively, in normal form.
    roots: Vec<PathBuf>,

    /// Directories watched only for the sources that sit directly in them.
    source_dirs: HashSet<PathBuf>,

    /// Registered sources, by their link key.
    sources: HashMap<PathBuf, Source>,

    /// The link key of the source each resolved target belongs to.
    by_target: HashMap<PathBuf, PathBuf>,

    /// The last component of every key. An event whose file name is not here names no source, so
    /// it never needs its path normalized to find out.
    names: HashSet<OsString>,
}

impl Watcher {
    /// Create a new watcher with the given configuration.
    pub fn new(config: WatcherConfig) -> Result<Self, WatchError> {
        let (tx, rx) = mpsc::channel();

        let watcher = notify::recommended_watcher(tx)?;
        let (broadcast_tx, _) = broadcast::channel(16);

        Ok(Self {
            watcher,
            receiver: rx,
            sender: broadcast_tx,
            config,
            batch: EventBatch::new(),
            roots: Vec::new(),
            source_dirs: HashSet::new(),
            sources: HashMap::new(),
            by_target: HashMap::new(),
            names: HashSet::new(),
        })
    }

    /// Start watching a directory and everything under it.
    pub fn watch(&mut self, path: &Path) -> Result<(), WatchError> {
        self.watcher
            .watch(path, RecursiveMode::Recursive)
            .map_err(|e| WatchError::WatchPath {
                path: path.to_path_buf(),
                source: e,
            })?;

        let root = crate::paths::resolve_dir(path);
        if !self.roots.contains(&root) {
            self.roots.push(root);
        }

        Ok(())
    }

    /// Start watching the entries directly inside a directory.
    pub fn watch_dir(&mut self, path: &Path) -> Result<(), WatchError> {
        self.watcher
            .watch(path, RecursiveMode::NonRecursive)
            .map_err(|e| WatchError::WatchPath {
                path: path.to_path_buf(),
                source: e,
            })
    }

    /// Stop watching a path.
    pub fn unwatch(&mut self, path: &Path) -> Result<(), WatchError> {
        self.watcher
            .unwatch(path)
            .map_err(|e| WatchError::WatchPath {
                path: path.to_path_buf(),
                source: e,
            })
    }

    /// Replace the set of source paths the watcher reports regardless of group patterns.
    ///
    /// A source inside a recursively watched directory needs no watch of its own. Any other
    /// source is watched through its parent directory, non-recursively, and filtered by path. That
    /// survives an editor's write-and-rename, and it sees a file that does not exist yet. A source
    /// that is a symlink also registers its target, and the target's parent is watched the same way
    /// when it lies outside the watched trees.
    ///
    /// Returns the directories that could not be watched. A later call tries them again.
    pub fn set_sources(&mut self, paths: &[PathBuf]) -> Vec<SourceIssue> {
        self.sources.clear();

        for path in paths {
            let key = normalize(path);
            let target = crate::paths::link_target(&key);

            self.sources.insert(key.clone(), Source { key, target });
        }

        self.reindex();
        self.sync_source_dirs()
    }

    fn reindex(&mut self) {
        self.by_target.clear();
        self.names.clear();

        for source in self.sources.values() {
            for key in std::iter::once(&source.key).chain(source.target.as_ref()) {
                if let Some(name) = key.file_name() {
                    self.names.insert(name.to_os_string());
                }
            }

            if let Some(target) = &source.target {
                self.by_target.insert(target.clone(), source.key.clone());
            }
        }
    }

    /// Make the non-recursive watches match what the registered sources need.
    fn sync_source_dirs(&mut self) -> Vec<SourceIssue> {
        let mut wanted: HashSet<PathBuf> = HashSet::new();

        for source in self.sources.values() {
            for key in std::iter::once(&source.key).chain(source.target.as_ref()) {
                let Some(parent) = key.parent() else { continue };

                if !self.under_a_root(parent) {
                    wanted.insert(parent.to_path_buf());
                }
            }
        }

        let stale: Vec<PathBuf> = self.source_dirs.difference(&wanted).cloned().collect();
        for dir in stale {
            let _ = self.unwatch(&dir);
            self.source_dirs.remove(&dir);
        }

        let mut issues = Vec::new();
        let missing: Vec<PathBuf> = wanted.difference(&self.source_dirs).cloned().collect();

        for dir in missing {
            match self.watch_dir(&dir) {
                Ok(()) => {
                    self.source_dirs.insert(dir);
                }
                Err(error) => issues.push(SourceIssue { dir, error }),
            }
        }

        issues
    }

    fn under_a_root(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| path.starts_with(root))
    }

    /// The link key of the registered source that `path`, in normal form, names by either key.
    fn source_for(&self, path: &Path) -> Option<&PathBuf> {
        self.sources
            .get_key_value(path)
            .map(|(key, _)| key)
            .or_else(|| self.by_target.get(path))
    }

    /// Subscribe to file events.
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<FileEvent>> {
        self.sender.subscribe()
    }

    /// Process pending events and return batched events if ready.
    ///
    /// An event passes when it names a registered source, or when a group pattern matches a path
    /// under a recursively watched directory. A source is never subject to the patterns' ignores.
    pub fn poll(&mut self, patterns: &PatternSet) -> Option<Vec<FileEvent>> {
        let mut resync = false;
        let mut parents: HashMap<PathBuf, PathBuf> = HashMap::new();

        while let Ok(result) = self.receiver.try_recv() {
            let Ok(event) = result else { continue };

            let kind = match event.kind {
                notify::EventKind::Create(_) => FileEventKind::Created,
                notify::EventKind::Modify(_) => FileEventKind::Modified,
                notify::EventKind::Remove(_) => FileEventKind::Deleted,
                _ => continue,
            };

            for path in event.paths {
                let by_pattern = patterns.matches(&path);
                let could_be_source = path
                    .file_name()
                    .is_some_and(|name| self.names.contains(name));

                // Normalizing costs syscalls, so a path that no source and no pattern can match
                // is dropped before it.
                if !by_pattern && !could_be_source {
                    continue;
                }

                let normal = normalize_cached(&path, &mut parents);
                let source = if could_be_source {
                    self.source_for(&normal).cloned()
                } else {
                    None
                };

                if source.as_ref() == Some(&normal) {
                    resync = true;
                }

                let by_pattern =
                    by_pattern && (self.roots.is_empty() || self.under_a_root(&normal));

                if source.is_some() || by_pattern {
                    self.batch.add(FileEvent { path, kind, source });
                }
            }
        }

        if resync {
            self.resync_targets();
        }

        if !self.batch.is_empty() && self.batch.is_ready(self.config.debounce) {
            let events = self.batch.drain();
            let _ = self.sender.send(events.clone());
            Some(events)
        } else {
            None
        }
    }

    /// Recompute the target of every source after an event on a link.
    ///
    /// A link can be created, deleted, or repointed, so its second key follows it.
    fn resync_targets(&mut self) {
        for source in self.sources.values_mut() {
            source.target = crate::paths::link_target(&source.key);
        }

        self.reindex();

        for issue in self.sync_source_dirs() {
            tracing::warn!(
                dir = %issue.dir.display(),
                error = %issue.error,
                "cannot watch the directory of a source symlink target"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn watcher() -> Watcher {
        Watcher::new(WatcherConfig {
            debounce: Duration::from_millis(30),
            ..Default::default()
        })
        .unwrap()
    }

    /// Poll until some batch satisfies `found`, or fail after a generous wait.
    fn wait_for(
        watcher: &mut Watcher,
        patterns: &PatternSet,
        mut found: impl FnMut(&[FileEvent]) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(15);

        while Instant::now() < deadline {
            if let Some(events) = watcher.poll(patterns) {
                if found(&events) {
                    return;
                }
            }

            std::thread::sleep(Duration::from_millis(25));
        }

        panic!("the expected event never arrived");
    }

    fn drain(watcher: &mut Watcher, patterns: &PatternSet) {
        let quiet_until = Instant::now() + Duration::from_millis(700);

        while Instant::now() < quiet_until {
            watcher.poll(patterns);
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn all_files() -> PatternSet {
        PatternSet::new(&["**/*".to_string()], &["**/.*".to_string()]).unwrap()
    }

    #[test]
    fn a_source_outside_the_watched_tree_is_seen_through_its_parent_and_bypasses_ignores() {
        let tree = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_dir = outside.path().canonicalize().unwrap();
        let source = outside_dir.join(".env");

        let mut watcher = watcher();
        watcher.watch(tree.path()).unwrap();
        let issues = watcher.set_sources(&[source.clone()]);
        assert!(issues.is_empty(), "{issues:?}");
        drain(&mut watcher, &all_files());

        std::fs::write(outside_dir.join("unrelated.txt"), "x").unwrap();
        std::fs::write(&source, "A=1\n").unwrap();

        wait_for(&mut watcher, &all_files(), |events| {
            assert!(
                events
                    .iter()
                    .all(|e| e.source.is_some() || !e.path.ends_with("unrelated.txt")),
                "a pattern leaked from outside the watched tree: {events:?}"
            );
            events.iter().any(|e| e.source.as_ref() == Some(&source))
        });
    }

    #[test]
    fn a_symlinked_source_follows_its_link_and_its_target() {
        let tree = tempfile::tempdir().unwrap();
        let tree_dir = tree.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_dir = outside.path().canonicalize().unwrap();
        let link = tree_dir.join(".env");
        let target = outside_dir.join("real.env");
        std::fs::write(&target, "A=1\n").unwrap();

        let mut watcher = watcher();
        watcher.watch(&tree_dir).unwrap();
        assert!(watcher.set_sources(&[link.clone()]).is_empty());
        drain(&mut watcher, &all_files());

        std::os::unix::fs::symlink(&target, &link).unwrap();
        wait_for(&mut watcher, &all_files(), |events| {
            events.iter().any(|e| e.source.as_ref() == Some(&link))
        });
        drain(&mut watcher, &all_files());

        std::fs::write(&target, "A=2\n").unwrap();
        wait_for(&mut watcher, &all_files(), |events| {
            events.iter().any(|e| e.source.as_ref() == Some(&link))
        });
        drain(&mut watcher, &all_files());

        std::fs::remove_file(&link).unwrap();
        wait_for(&mut watcher, &all_files(), |events| {
            events
                .iter()
                .any(|e| e.source.as_ref() == Some(&link) && e.kind == FileEventKind::Deleted)
        });
    }

    #[test]
    fn set_sources_replaces_the_watch_set() {
        let tree = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let first_dir = first.path().canonicalize().unwrap();
        let second = tempfile::tempdir().unwrap();
        let second_dir = second.path().canonicalize().unwrap();

        let mut watcher = watcher();
        watcher.watch(tree.path()).unwrap();
        watcher.set_sources(&[first_dir.join("a")]);
        assert_eq!(watcher.source_dirs, HashSet::from([first_dir.clone()]));

        watcher.set_sources(&[second_dir.join("b")]);
        assert_eq!(watcher.source_dirs, HashSet::from([second_dir.clone()]));

        let missing = second_dir.join("gone").join("c");
        let issues = watcher.set_sources(&[missing]);
        assert_eq!(issues.len(), 1);
        assert!(watcher.source_dirs.is_empty());
    }
}

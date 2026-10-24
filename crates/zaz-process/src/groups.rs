//! A shared record of the process groups of commands that are running.
//!
//! Each run-to-completion command lives in a process group of its own, so a terminal's Ctrl+C
//! reaches none of them. A caller that must stop them on its own interrupt keeps this registry.

use nix::sys::signal::Signal;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::SignalHandler;

/// The process groups of the commands that are running now.
#[derive(Debug, Clone, Default)]
pub struct GroupRegistry {
    groups: Arc<Mutex<HashSet<u32>>>,
}

/// Removes its group from the registry when dropped.
#[derive(Debug)]
pub struct GroupGuard {
    groups: Arc<Mutex<HashSet<u32>>>,
    pgid: u32,
}

impl GroupRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a running group until the guard drops.
    pub fn track(&self, pgid: u32) -> GroupGuard {
        lock(&self.groups).insert(pgid);

        GroupGuard {
            groups: Arc::clone(&self.groups),
            pgid,
        }
    }

    /// Whether no group is recorded.
    pub fn is_empty(&self) -> bool {
        lock(&self.groups).is_empty()
    }

    /// Send SIGTERM to every recorded group.
    pub fn terminate(&self) {
        self.signal(Signal::SIGTERM);
    }

    /// Send SIGKILL to every recorded group.
    pub fn kill(&self) {
        self.signal(Signal::SIGKILL);
    }

    fn signal(&self, signal: Signal) {
        for pgid in lock(&self.groups).iter() {
            let _ = SignalHandler::send_to_group(*pgid as i32, signal);
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        lock(&self.groups).remove(&self.pgid);
    }
}

fn lock(groups: &Mutex<HashSet<u32>>) -> MutexGuard<'_, HashSet<u32>> {
    groups.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guard_removes_its_group_when_dropped() {
        let registry = GroupRegistry::new();
        assert!(registry.is_empty());

        let guard = registry.track(4_000_000);
        assert!(!registry.is_empty());

        drop(guard);
        assert!(registry.is_empty());
    }
}

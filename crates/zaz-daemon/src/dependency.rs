//! Dependency graph management for group orchestration.
//!
//! The `DependencyResolver` tracks group dependencies and waiting states,
//! providing pure functions for computing which groups should start or skip
//! when dependencies complete or fail.

use crate::state::GroupStatus;
use std::collections::{HashMap, HashSet};

/// Result of marking a group as complete.
#[derive(Debug, Clone, Default)]
pub struct CompletionResult {
    /// Groups that are now ready to start.
    pub ready_to_start: Vec<String>,
    /// Groups that should be skipped (dependency failed).
    pub needs_skip: Vec<String>,
}

/// Result of marking a group as failed.
#[derive(Debug, Clone, Default)]
pub struct FailureResult {
    /// Groups that should be skipped (transitively).
    pub to_skip: Vec<String>,
}

/// Manages dependency relationships between groups.
///
/// This struct tracks:
/// - Which groups depend on which other groups (reverse mapping)
/// - Which groups are waiting for dependencies
/// - Current status of each group
///
/// It provides pure functions for computing state transitions without
/// performing any I/O or side effects.
#[derive(Debug, Clone)]
pub struct DependencyResolver {
    /// Reverse dependency map: group -> groups that depend on it.
    dependents: HashMap<String, Vec<String>>,

    /// Groups waiting for dependencies: group -> deps still waiting for.
    waiting: HashMap<String, HashSet<String>>,

    /// Current status of each group.
    statuses: HashMap<String, GroupStatus>,

    /// Forward dependency map: group -> groups it depends on.
    dependencies: HashMap<String, Vec<String>>,
}

impl DependencyResolver {
    /// Create a new empty dependency resolver.
    pub fn new() -> Self {
        Self {
            dependents: HashMap::new(),
            waiting: HashMap::new(),
            statuses: HashMap::new(),
            dependencies: HashMap::new(),
        }
    }

    /// Build a dependency resolver from group configurations.
    ///
    /// Takes an iterator of (group_name, dependencies) pairs.
    pub fn from_groups<'a, I>(groups: I) -> Self
    where
        I: IntoIterator<Item = (&'a str, &'a [String])>,
    {
        let mut resolver = Self::new();

        for (name, deps) in groups {
            resolver.add_group(name, deps);
        }

        resolver
    }

    /// Add a group with its dependencies.
    pub fn add_group(&mut self, name: &str, dependencies: &[String]) {
        // Store forward dependencies
        self.dependencies
            .insert(name.to_string(), dependencies.to_vec());

        // Build reverse dependency map
        for dep in dependencies {
            self.dependents
                .entry(dep.clone())
                .or_default()
                .push(name.to_string());
        }

        // Initialize status as Pending
        self.statuses.insert(name.to_string(), GroupStatus::Pending);
    }

    /// Get the dependencies for a group.
    pub fn get_dependencies(&self, group: &str) -> Vec<String> {
        self.dependencies.get(group).cloned().unwrap_or_default()
    }

    /// Get the dependents (reverse dependencies) for a group.
    pub fn get_dependents(&self, group: &str) -> Vec<String> {
        self.dependents.get(group).cloned().unwrap_or_default()
    }

    /// Set the status of a group.
    pub fn set_status(&mut self, group: &str, status: GroupStatus) {
        self.statuses.insert(group.to_string(), status);
    }

    /// Check if a group is in Ready status.
    pub fn is_ready(&self, group: &str) -> bool {
        self.statuses.get(group) == Some(&GroupStatus::Ready)
    }

    /// Check if a group is in Failed or Skipped status.
    pub fn is_failed_or_skipped(&self, group: &str) -> bool {
        matches!(
            self.statuses.get(group),
            Some(&GroupStatus::Failed) | Some(&GroupStatus::Skipped)
        )
    }

    /// Check if any dependents are waiting for this group.
    ///
    /// Returns true if at least one group is waiting for the given group
    /// to complete. This is used to determine lifecycle phase - if dependents
    /// are waiting, we're in Startup phase; otherwise, we're in Runtime phase.
    pub fn has_waiting_dependents(&self, group: &str) -> bool {
        // Get all groups that depend on this group
        let dependents = self.get_dependents(group);

        // Check if any of them are still waiting for this group
        for dependent in dependents {
            if let Some(waiting_for) = self.waiting.get(&dependent) {
                if waiting_for.contains(group) {
                    return true;
                }
            }
        }
        false
    }

    /// Check if any dependency of a group has failed or been skipped.
    pub fn any_dependency_failed(&self, group: &str) -> bool {
        self.get_dependencies(group)
            .iter()
            .any(|dep| self.is_failed_or_skipped(dep))
    }

    /// Mark a group as waiting for its dependencies.
    ///
    /// Returns the set of dependencies the group is waiting for,
    /// or None if all dependencies are already satisfied.
    pub fn mark_waiting(&mut self, group: &str) -> Option<HashSet<String>> {
        let deps = self.get_dependencies(group);
        let unready: HashSet<String> = deps.into_iter().filter(|d| !self.is_ready(d)).collect();

        if unready.is_empty() {
            None
        } else {
            self.waiting.insert(group.to_string(), unready.clone());
            self.set_status(group, GroupStatus::Waiting);
            Some(unready)
        }
    }

    /// Mark a group as complete (Ready status).
    ///
    /// Returns groups that are now ready to start because all their
    /// dependencies are satisfied, and groups that should be skipped
    /// because a dependency failed.
    pub fn mark_complete(&mut self, group: &str) -> CompletionResult {
        self.set_status(group, GroupStatus::Ready);

        let mut result = CompletionResult::default();

        // Get dependents of this group
        let dependents = self.get_dependents(group);

        for dependent in dependents {
            if let Some(waiting_for) = self.waiting.get_mut(&dependent) {
                waiting_for.remove(group);

                if waiting_for.is_empty() {
                    // All dependencies satisfied
                    self.waiting.remove(&dependent);

                    // Check if any dependency failed
                    if self.any_dependency_failed(&dependent) {
                        result.needs_skip.push(dependent);
                    } else {
                        result.ready_to_start.push(dependent);
                    }
                }
            }
        }

        result
    }

    /// Mark a group as skipped and compute cascade skips.
    ///
    /// Use this when a group should be skipped due to a failed dependency.
    /// Returns all dependent groups that should also be skipped (transitively).
    pub fn mark_skipped(&mut self, group: &str) -> FailureResult {
        self.set_status(group, GroupStatus::Skipped);
        self.cascade_skip_from(group)
    }

    /// Reset waiting state and statuses for a new full execution wave.
    pub fn reset_for_rerun(&mut self) {
        self.waiting.clear();
        for status in self.statuses.values_mut() {
            *status = GroupStatus::Pending;
        }
    }

    /// Compute groups to skip starting from a failed/skipped group.
    ///
    /// This is called internally and can also be used to compute skips
    /// without changing the source group's status.
    fn cascade_skip_from(&mut self, group: &str) -> FailureResult {
        let mut result = FailureResult::default();
        let mut to_process = vec![group.to_string()];

        while let Some(current) = to_process.pop() {
            let dependents = self.get_dependents(&current);

            for dependent in dependents {
                if let Some(waiting_for) = self.waiting.get_mut(&dependent) {
                    waiting_for.remove(&current);

                    if waiting_for.is_empty() {
                        self.waiting.remove(&dependent);
                        self.set_status(&dependent, GroupStatus::Skipped);
                        result.to_skip.push(dependent.clone());
                        to_process.push(dependent);
                    }
                }
            }
        }

        result
    }
}

impl Default for DependencyResolver {
    fn default() -> Self {
        Self::new()
    }
}

/// Test-only methods for DependencyResolver.
///
/// These methods are only used in tests and are not part of the public API.
#[cfg(test)]
impl DependencyResolver {
    /// Remove a group and all its references.
    pub fn remove_group(&mut self, name: &str) {
        // Remove from dependencies map
        if let Some(deps) = self.dependencies.remove(name) {
            // Remove from dependents (reverse map)
            for dep in deps {
                if let Some(dependents) = self.dependents.get_mut(&dep) {
                    dependents.retain(|d| d != name);
                }
            }
        }

        // Also remove from dependents as a key (if other groups depend on this one)
        self.dependents.remove(name);

        // Remove status
        self.statuses.remove(name);

        // Remove from waiting
        self.waiting.remove(name);
    }

    /// Reset all statuses to Pending.
    pub fn reset_statuses(&mut self) {
        for status in self.statuses.values_mut() {
            *status = GroupStatus::Pending;
        }
        self.waiting.clear();
    }

    /// Get the status of a group.
    pub fn get_status(&self, group: &str) -> Option<GroupStatus> {
        self.statuses.get(group).cloned()
    }

    /// Check if all dependencies of a group are ready.
    pub fn all_dependencies_ready(&self, group: &str) -> bool {
        self.get_dependencies(group)
            .iter()
            .all(|dep| self.is_ready(dep))
    }

    /// Check if a group is in the waiting state.
    pub fn is_waiting(&self, group: &str) -> bool {
        self.waiting.contains_key(group)
    }

    /// Get the set of dependencies a group is waiting for.
    pub fn waiting_for(&self, group: &str) -> Option<&HashSet<String>> {
        self.waiting.get(group)
    }

    /// Mark a group as failed and compute cascade skips.
    pub fn mark_failed(&mut self, group: &str) -> FailureResult {
        self.set_status(group, GroupStatus::Failed);
        self.cascade_skip_from(group)
    }

    /// Compute the initial state for all groups.
    /// Returns the list of groups that are ready to start immediately.
    pub fn compute_initial_state(&mut self) -> Vec<String> {
        let groups: Vec<String> = self.statuses.keys().cloned().collect();
        let mut ready_to_start = Vec::new();

        for group in groups {
            if self.get_dependencies(&group).is_empty() {
                ready_to_start.push(group);
            } else {
                self.mark_waiting(&group);
            }
        }

        ready_to_start
    }

    /// Clear all waiting state.
    pub fn clear_waiting(&mut self) {
        self.waiting.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Construction and basic operations
    // =========================================================================

    #[test]
    fn test_new_resolver_is_empty() {
        let resolver = DependencyResolver::new();
        assert!(resolver.get_dependencies("a").is_empty());
        assert!(resolver.get_dependents("a").is_empty());
        assert!(resolver.get_status("a").is_none());
    }

    #[test]
    fn test_add_group_no_deps() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("a", &[]);

        assert!(resolver.get_dependencies("a").is_empty());
        assert_eq!(resolver.get_status("a"), Some(GroupStatus::Pending));
    }

    #[test]
    fn test_add_group_with_deps() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("b", &[]);
        resolver.add_group("a", &["b".to_string()]);

        assert_eq!(resolver.get_dependencies("a"), vec!["b".to_string()]);
        assert_eq!(resolver.get_dependents("b"), vec!["a".to_string()]);
    }

    #[test]
    fn test_from_groups() {
        let deps_a = vec!["b".to_string(), "c".to_string()];
        let deps_b = vec!["d".to_string()];
        let deps_c = vec!["d".to_string()];
        let deps_d: Vec<String> = vec![];

        let groups = vec![
            ("a", deps_a.as_slice()),
            ("b", deps_b.as_slice()),
            ("c", deps_c.as_slice()),
            ("d", deps_d.as_slice()),
        ];

        let resolver = DependencyResolver::from_groups(groups);

        assert_eq!(
            resolver.get_dependencies("a"),
            vec!["b".to_string(), "c".to_string()]
        );
        assert_eq!(resolver.get_dependencies("b"), vec!["d".to_string()]);
        assert!(resolver.get_dependents("d").contains(&"b".to_string()));
        assert!(resolver.get_dependents("d").contains(&"c".to_string()));
    }

    #[test]
    fn test_remove_group() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("b", &[]);
        resolver.add_group("a", &["b".to_string()]);

        resolver.remove_group("a");

        assert!(resolver.get_status("a").is_none());
        assert!(resolver.get_dependents("b").is_empty());
    }

    // =========================================================================
    // Status checks
    // =========================================================================

    #[test]
    fn test_is_ready() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("a", &[]);

        assert!(!resolver.is_ready("a"));

        resolver.set_status("a", GroupStatus::Ready);
        assert!(resolver.is_ready("a"));
    }

    #[test]
    fn test_is_failed_or_skipped() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("a", &[]);

        assert!(!resolver.is_failed_or_skipped("a"));

        resolver.set_status("a", GroupStatus::Failed);
        assert!(resolver.is_failed_or_skipped("a"));

        resolver.set_status("a", GroupStatus::Skipped);
        assert!(resolver.is_failed_or_skipped("a"));

        resolver.set_status("a", GroupStatus::Ready);
        assert!(!resolver.is_failed_or_skipped("a"));
    }

    #[test]
    fn test_any_dependency_failed() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("c", &[]);
        resolver.add_group("b", &[]);
        resolver.add_group("a", &["b".to_string(), "c".to_string()]);

        assert!(!resolver.any_dependency_failed("a"));

        resolver.set_status("b", GroupStatus::Failed);
        assert!(resolver.any_dependency_failed("a"));
    }

    #[test]
    fn test_all_dependencies_ready() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("c", &[]);
        resolver.add_group("b", &[]);
        resolver.add_group("a", &["b".to_string(), "c".to_string()]);

        assert!(!resolver.all_dependencies_ready("a"));

        resolver.set_status("b", GroupStatus::Ready);
        assert!(!resolver.all_dependencies_ready("a"));

        resolver.set_status("c", GroupStatus::Ready);
        assert!(resolver.all_dependencies_ready("a"));
    }

    // =========================================================================
    // Waiting state
    // =========================================================================

    #[test]
    fn test_mark_waiting_with_unready_deps() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("b", &[]);
        resolver.add_group("a", &["b".to_string()]);

        let waiting = resolver.mark_waiting("a");

        assert!(waiting.is_some());
        assert!(waiting.unwrap().contains(&"b".to_string()));
        assert!(resolver.is_waiting("a"));
        assert_eq!(resolver.get_status("a"), Some(GroupStatus::Waiting));
    }

    #[test]
    fn test_mark_waiting_with_ready_deps() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("b", &[]);
        resolver.add_group("a", &["b".to_string()]);
        resolver.set_status("b", GroupStatus::Ready);

        let waiting = resolver.mark_waiting("a");

        assert!(waiting.is_none());
        assert!(!resolver.is_waiting("a"));
    }

    #[test]
    fn test_mark_waiting_no_deps() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("a", &[]);

        let waiting = resolver.mark_waiting("a");

        assert!(waiting.is_none());
        assert!(!resolver.is_waiting("a"));
    }

    // =========================================================================
    // mark_complete - linear chain
    // =========================================================================

    #[test]
    fn test_mark_complete_triggers_dependent() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("b", &[]);
        resolver.add_group("a", &["b".to_string()]);

        // a is waiting for b
        resolver.mark_waiting("a");
        assert!(resolver.is_waiting("a"));

        // b completes
        let result = resolver.mark_complete("b");

        assert_eq!(result.ready_to_start, vec!["a".to_string()]);
        assert!(!resolver.is_waiting("a"));
        assert!(resolver.is_ready("b"));
    }

    #[test]
    fn test_mark_complete_chain() {
        // c -> b -> a (a depends on b, b depends on c)
        let mut resolver = DependencyResolver::new();
        resolver.add_group("c", &[]);
        resolver.add_group("b", &["c".to_string()]);
        resolver.add_group("a", &["b".to_string()]);

        resolver.mark_waiting("b");
        resolver.mark_waiting("a");

        // c completes -> b ready
        let result1 = resolver.mark_complete("c");
        assert_eq!(result1.ready_to_start, vec!["b".to_string()]);

        // b completes -> a ready
        let result2 = resolver.mark_complete("b");
        assert_eq!(result2.ready_to_start, vec!["a".to_string()]);
    }

    #[test]
    fn test_mark_complete_no_dependents() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("a", &[]);

        let result = resolver.mark_complete("a");

        assert!(result.ready_to_start.is_empty());
        assert!(resolver.is_ready("a"));
    }

    // =========================================================================
    // mark_complete - diamond pattern
    // =========================================================================

    #[test]
    fn test_mark_complete_diamond_one_path() {
        // Diamond: a -> b,c -> d
        let mut resolver = DependencyResolver::new();
        resolver.add_group("d", &[]);
        resolver.add_group("b", &["d".to_string()]);
        resolver.add_group("c", &["d".to_string()]);
        resolver.add_group("a", &["b".to_string(), "c".to_string()]);

        resolver.mark_waiting("b");
        resolver.mark_waiting("c");
        resolver.mark_waiting("a");

        // d completes -> b and c ready
        let result = resolver.mark_complete("d");
        assert_eq!(result.ready_to_start.len(), 2);
        assert!(result.ready_to_start.contains(&"b".to_string()));
        assert!(result.ready_to_start.contains(&"c".to_string()));

        // b completes -> a still waiting for c
        let result2 = resolver.mark_complete("b");
        assert!(result2.ready_to_start.is_empty());

        // c completes -> a ready
        let result3 = resolver.mark_complete("c");
        assert_eq!(result3.ready_to_start, vec!["a".to_string()]);
    }

    // =========================================================================
    // mark_failed and cascade_skip
    // =========================================================================

    #[test]
    fn test_mark_failed_cascades_to_dependent() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("b", &[]);
        resolver.add_group("a", &["b".to_string()]);

        resolver.mark_waiting("a");

        let result = resolver.mark_failed("b");

        assert!(resolver.is_failed_or_skipped("b"));
        assert_eq!(result.to_skip, vec!["a".to_string()]);
        assert_eq!(resolver.get_status("a"), Some(GroupStatus::Skipped));
    }

    #[test]
    fn test_mark_failed_cascades_chain() {
        // c -> b -> a
        let mut resolver = DependencyResolver::new();
        resolver.add_group("c", &[]);
        resolver.add_group("b", &["c".to_string()]);
        resolver.add_group("a", &["b".to_string()]);

        resolver.mark_waiting("b");
        resolver.mark_waiting("a");

        let result = resolver.mark_failed("c");

        // Both b and a should be skipped
        assert_eq!(result.to_skip.len(), 2);
        assert!(result.to_skip.contains(&"b".to_string()));
        assert!(result.to_skip.contains(&"a".to_string()));
        assert_eq!(resolver.get_status("b"), Some(GroupStatus::Skipped));
        assert_eq!(resolver.get_status("a"), Some(GroupStatus::Skipped));
    }

    #[test]
    fn test_mark_failed_diamond_cascade() {
        // Diamond: a -> b,c -> d
        let mut resolver = DependencyResolver::new();
        resolver.add_group("d", &[]);
        resolver.add_group("b", &["d".to_string()]);
        resolver.add_group("c", &["d".to_string()]);
        resolver.add_group("a", &["b".to_string(), "c".to_string()]);

        resolver.mark_waiting("b");
        resolver.mark_waiting("c");
        resolver.mark_waiting("a");

        let result = resolver.mark_failed("d");

        // b, c, and a should all be skipped
        assert_eq!(result.to_skip.len(), 3);
        assert!(result.to_skip.contains(&"b".to_string()));
        assert!(result.to_skip.contains(&"c".to_string()));
        assert!(result.to_skip.contains(&"a".to_string()));
    }

    #[test]
    fn test_mark_failed_partial_diamond() {
        // Diamond: a -> b,c -> d
        // d completes, then b fails
        let mut resolver = DependencyResolver::new();
        resolver.add_group("d", &[]);
        resolver.add_group("b", &["d".to_string()]);
        resolver.add_group("c", &["d".to_string()]);
        resolver.add_group("a", &["b".to_string(), "c".to_string()]);

        resolver.mark_waiting("b");
        resolver.mark_waiting("c");
        resolver.mark_waiting("a");

        // d completes
        resolver.mark_complete("d");

        // b fails - a should NOT be skipped yet (still waiting for c)
        let result = resolver.mark_failed("b");
        assert!(result.to_skip.is_empty());

        // c completes - a should be in needs_skip because b failed
        let result2 = resolver.mark_complete("c");
        // a is not in ready_to_start because b failed
        assert!(result2.ready_to_start.is_empty());
        // a is in needs_skip because b failed
        assert_eq!(result2.needs_skip, vec!["a".to_string()]);
        assert!(resolver.any_dependency_failed("a"));
    }

    #[test]
    fn test_mark_failed_no_waiting_dependents() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("b", &[]);
        resolver.add_group("a", &["b".to_string()]);
        // Don't mark a as waiting

        let result = resolver.mark_failed("b");

        assert!(result.to_skip.is_empty());
        assert!(resolver.is_failed_or_skipped("b"));
    }

    // =========================================================================
    // compute_initial_state
    // =========================================================================

    #[test]
    fn test_compute_initial_state_no_deps() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("a", &[]);
        resolver.add_group("b", &[]);

        let ready_to_start = resolver.compute_initial_state();

        // Both groups should start (no dependencies)
        assert!(ready_to_start.contains(&"a".to_string()));
        assert!(ready_to_start.contains(&"b".to_string()));
    }

    #[test]
    fn test_compute_initial_state_with_deps() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("b", &[]);
        resolver.add_group("a", &["b".to_string()]);

        let ready_to_start = resolver.compute_initial_state();

        // b should start, a is waiting
        assert!(ready_to_start.contains(&"b".to_string()));
        assert!(!ready_to_start.contains(&"a".to_string()));
        assert!(resolver.is_waiting("a"));
    }

    #[test]
    fn test_compute_initial_state_chain() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("c", &[]);
        resolver.add_group("b", &["c".to_string()]);
        resolver.add_group("a", &["b".to_string()]);

        let ready_to_start = resolver.compute_initial_state();

        // Only c should start
        assert!(ready_to_start.contains(&"c".to_string()));
        assert!(!ready_to_start.contains(&"b".to_string()));
        assert!(!ready_to_start.contains(&"a".to_string()));
        assert!(resolver.is_waiting("b"));
        assert!(resolver.is_waiting("a"));
    }

    // =========================================================================
    // Edge cases
    // =========================================================================

    #[test]
    fn test_nonexistent_group() {
        let resolver = DependencyResolver::new();

        assert!(resolver.get_dependencies("nonexistent").is_empty());
        assert!(resolver.get_dependents("nonexistent").is_empty());
        assert!(resolver.get_status("nonexistent").is_none());
        assert!(!resolver.is_ready("nonexistent"));
        assert!(!resolver.is_failed_or_skipped("nonexistent"));
    }

    #[test]
    fn test_reset_statuses() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("a", &[]);
        resolver.add_group("b", &["a".to_string()]);

        resolver.set_status("a", GroupStatus::Ready);
        resolver.mark_waiting("b");

        resolver.reset_statuses();

        assert_eq!(resolver.get_status("a"), Some(GroupStatus::Pending));
        assert_eq!(resolver.get_status("b"), Some(GroupStatus::Pending));
        assert!(!resolver.is_waiting("b"));
    }

    #[test]
    fn test_clear_waiting() {
        let mut resolver = DependencyResolver::new();
        resolver.add_group("b", &[]);
        resolver.add_group("a", &["b".to_string()]);
        resolver.mark_waiting("a");

        assert!(resolver.is_waiting("a"));

        resolver.clear_waiting();

        assert!(!resolver.is_waiting("a"));
    }

    // =========================================================================
    // Complex scenarios
    // =========================================================================

    #[test]
    fn test_multiple_dependents_same_dependency() {
        // b and c both depend on a
        let mut resolver = DependencyResolver::new();
        resolver.add_group("a", &[]);
        resolver.add_group("b", &["a".to_string()]);
        resolver.add_group("c", &["a".to_string()]);

        resolver.mark_waiting("b");
        resolver.mark_waiting("c");

        let result = resolver.mark_complete("a");

        assert_eq!(result.ready_to_start.len(), 2);
        assert!(result.ready_to_start.contains(&"b".to_string()));
        assert!(result.ready_to_start.contains(&"c".to_string()));
    }

    #[test]
    fn test_deep_chain_failure() {
        // e -> d -> c -> b -> a
        let mut resolver = DependencyResolver::new();
        resolver.add_group("a", &[]);
        resolver.add_group("b", &["a".to_string()]);
        resolver.add_group("c", &["b".to_string()]);
        resolver.add_group("d", &["c".to_string()]);
        resolver.add_group("e", &["d".to_string()]);

        resolver.mark_waiting("b");
        resolver.mark_waiting("c");
        resolver.mark_waiting("d");
        resolver.mark_waiting("e");

        let result = resolver.mark_failed("a");

        assert_eq!(result.to_skip.len(), 4);
        for g in &["b", "c", "d", "e"] {
            assert!(
                result.to_skip.contains(&g.to_string()),
                "{} should be skipped",
                g
            );
            assert_eq!(
                resolver.get_status(g),
                Some(GroupStatus::Skipped),
                "{} should have Skipped status",
                g
            );
        }
    }
}

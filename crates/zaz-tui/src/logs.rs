//! Log buffer system for the TUI.
//!
//! Provides per-process log storage with filtering and search capabilities.

use regex::Regex;
use std::collections::{HashMap, VecDeque};

use crate::daemon::{LogLine, LogSource};

/// A stored log entry with source info.
#[derive(Debug, Clone)]
pub struct StoredLog {
    /// The log content.
    pub content: String,
    /// Source of the log.
    pub source: LogSource,
}

/// Default maximum lines per process.
const DEFAULT_MAX_LINES: usize = 10_000;

/// Search state for navigating matches.
#[derive(Debug, Clone)]
pub struct SearchState {
    /// The compiled regex pattern.
    pub pattern: Regex,
    /// Current match index (0-based).
    pub current_match: usize,
    /// Total number of matches.
    pub total_matches: usize,
    /// Line indices of matches (for quick navigation).
    pub match_indices: Vec<usize>,
}

impl SearchState {
    /// Create a new search state from a pattern.
    pub fn new(pattern: &str) -> Result<Self, regex::Error> {
        let regex = Regex::new(pattern)?;
        Ok(Self {
            pattern: regex,
            current_match: 0,
            total_matches: 0,
            match_indices: Vec::new(),
        })
    }

    /// Update match indices based on visible lines.
    pub fn update_matches(&mut self, lines: &[&str]) {
        self.match_indices.clear();
        for (idx, line) in lines.iter().enumerate() {
            if self.pattern.is_match(line) {
                self.match_indices.push(idx);
            }
        }
        self.total_matches = self.match_indices.len();
        // Clamp current match to valid range
        if self.current_match >= self.total_matches && self.total_matches > 0 {
            self.current_match = self.total_matches - 1;
        }
    }

    /// Move to next match, returning the line index.
    pub fn next_match(&mut self) -> Option<usize> {
        if self.match_indices.is_empty() {
            return None;
        }
        self.current_match = (self.current_match + 1) % self.total_matches;
        Some(self.match_indices[self.current_match])
    }

    /// Move to previous match, returning the line index.
    pub fn prev_match(&mut self) -> Option<usize> {
        if self.match_indices.is_empty() {
            return None;
        }
        if self.current_match == 0 {
            self.current_match = self.total_matches - 1;
        } else {
            self.current_match -= 1;
        }
        Some(self.match_indices[self.current_match])
    }

    /// Get the current match line index.
    pub fn current_line(&self) -> Option<usize> {
        self.match_indices.get(self.current_match).copied()
    }
}

/// Per-process log storage with filtering and search.
pub struct LogBuffer {
    /// Logs keyed by process name.
    logs: HashMap<String, VecDeque<StoredLog>>,
    /// Maximum lines to keep per process.
    max_lines: usize,
    /// Active filter pattern (hides non-matching lines).
    filter: Option<Regex>,
    /// Active search state (highlights and navigates matches).
    search: Option<SearchState>,
    /// Whether to auto-scroll on new logs.
    follow_mode: bool,
    /// Currently selected process for log viewing.
    selected_process: Option<String>,
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl LogBuffer {
    /// Create a new log buffer with default settings.
    pub fn new() -> Self {
        Self {
            logs: HashMap::new(),
            max_lines: DEFAULT_MAX_LINES,
            filter: None,
            search: None,
            follow_mode: true,
            selected_process: None,
        }
    }

    /// Create a new log buffer with custom max lines.
    pub fn with_max_lines(max_lines: usize) -> Self {
        Self {
            logs: HashMap::new(),
            max_lines,
            filter: None,
            search: None,
            follow_mode: true,
            selected_process: None,
        }
    }

    /// Add a log line for a process.
    pub fn push(&mut self, log: LogLine) {
        let buffer = self
            .logs
            .entry(log.process)
            .or_insert_with(VecDeque::new);
        buffer.push_back(StoredLog {
            content: log.content,
            source: log.source,
        });

        // Enforce max lines
        while buffer.len() > self.max_lines {
            buffer.pop_front();
        }
    }

    /// Add a log line with process, content, and source directly.
    pub fn push_line(&mut self, process: &str, content: String, source: LogSource) {
        let buffer = self
            .logs
            .entry(process.to_string())
            .or_insert_with(VecDeque::new);
        buffer.push_back(StoredLog { content, source });

        // Enforce max lines
        while buffer.len() > self.max_lines {
            buffer.pop_front();
        }
    }

    /// Clear all logs.
    pub fn clear_all(&mut self) {
        self.logs.clear();
    }

    /// Clear logs for a specific process.
    pub fn clear_process(&mut self, process: &str) {
        if let Some(buffer) = self.logs.get_mut(process) {
            buffer.clear();
        }
    }

    /// Get all process names with logs.
    pub fn processes(&self) -> Vec<&String> {
        self.logs.keys().collect()
    }

    /// Get the number of logs for a process.
    pub fn len_for(&self, process: &str) -> usize {
        self.logs.get(process).map(|b| b.len()).unwrap_or(0)
    }

    /// Get total log count across all processes.
    pub fn total_len(&self) -> usize {
        self.logs.values().map(|b| b.len()).sum()
    }

    /// Check if there are any logs.
    pub fn is_empty(&self) -> bool {
        self.logs.is_empty() || self.logs.values().all(|b| b.is_empty())
    }

    /// Get raw logs for a process (unfiltered).
    pub fn raw_logs(&self, process: &str) -> Option<&VecDeque<StoredLog>> {
        self.logs.get(process)
    }

    /// Get filtered logs for a process.
    ///
    /// If a filter is active, only matching lines are returned.
    /// Returns (line_index, StoredLog) pairs for scroll position tracking.
    pub fn filtered_logs(&self, process: &str) -> Vec<(usize, &StoredLog)> {
        let Some(buffer) = self.logs.get(process) else {
            return Vec::new();
        };

        match &self.filter {
            Some(regex) => buffer
                .iter()
                .enumerate()
                .filter(|(_, log)| regex.is_match(&log.content))
                .collect(),
            None => buffer.iter().enumerate().collect(),
        }
    }

    /// Get all logs combined (for full style) with process prefix.
    ///
    /// Returns (process, line_index, StoredLog) tuples.
    pub fn all_logs_combined(&self) -> Vec<(&str, usize, &StoredLog)> {
        // For combined view, we interleave logs chronologically
        // Since we don't have timestamps, we just concatenate per-process
        let mut result = Vec::new();

        for (process, buffer) in &self.logs {
            for (idx, log) in buffer.iter().enumerate() {
                let filtered = match &self.filter {
                    Some(regex) => regex.is_match(&log.content),
                    None => true,
                };
                if filtered {
                    result.push((process.as_str(), idx, log));
                }
            }
        }

        result
    }

    /// Set a filter pattern.
    ///
    /// Returns an error if the pattern is invalid.
    pub fn set_filter(&mut self, pattern: &str) -> Result<(), regex::Error> {
        if pattern.is_empty() {
            self.filter = None;
        } else {
            self.filter = Some(Regex::new(pattern)?);
        }
        Ok(())
    }

    /// Clear the active filter.
    pub fn clear_filter(&mut self) {
        self.filter = None;
    }

    /// Check if a filter is active.
    pub fn has_filter(&self) -> bool {
        self.filter.is_some()
    }

    /// Get the current filter pattern (if any).
    pub fn filter_pattern(&self) -> Option<&str> {
        self.filter.as_ref().map(|r| r.as_str())
    }

    /// Start a search with the given pattern.
    ///
    /// Returns an error if the pattern is invalid.
    pub fn start_search(&mut self, pattern: &str) -> Result<(), regex::Error> {
        if pattern.is_empty() {
            self.search = None;
        } else {
            self.search = Some(SearchState::new(pattern)?);
        }
        Ok(())
    }

    /// Clear the active search.
    pub fn clear_search(&mut self) {
        self.search = None;
    }

    /// Check if a search is active.
    pub fn has_search(&self) -> bool {
        self.search.is_some()
    }

    /// Get a reference to the search state.
    pub fn search_state(&self) -> Option<&SearchState> {
        self.search.as_ref()
    }

    /// Get a mutable reference to the search state.
    pub fn search_state_mut(&mut self) -> Option<&mut SearchState> {
        self.search.as_mut()
    }

    /// Update search matches for the given lines.
    pub fn update_search_matches(&mut self, lines: &[&str]) {
        if let Some(search) = &mut self.search {
            search.update_matches(lines);
        }
    }

    /// Move to next search match.
    pub fn next_search_match(&mut self) -> Option<usize> {
        self.search.as_mut().and_then(|s| s.next_match())
    }

    /// Move to previous search match.
    pub fn prev_search_match(&mut self) -> Option<usize> {
        self.search.as_mut().and_then(|s| s.prev_match())
    }

    /// Check if a line matches the current search pattern.
    pub fn is_search_match(&self, line: &str) -> bool {
        self.search
            .as_ref()
            .map(|s| s.pattern.is_match(line))
            .unwrap_or(false)
    }

    /// Check if follow mode is enabled.
    pub fn is_following(&self) -> bool {
        self.follow_mode
    }

    /// Enable follow mode (auto-scroll on new logs).
    pub fn enable_follow(&mut self) {
        self.follow_mode = true;
    }

    /// Disable follow mode.
    pub fn disable_follow(&mut self) {
        self.follow_mode = false;
    }

    /// Toggle follow mode.
    pub fn toggle_follow(&mut self) {
        self.follow_mode = !self.follow_mode;
    }

    /// Get the selected process.
    pub fn selected_process(&self) -> Option<&str> {
        self.selected_process.as_deref()
    }

    /// Set the selected process.
    pub fn select_process(&mut self, process: Option<String>) {
        self.selected_process = process;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_log_buffer() {
        let buffer = LogBuffer::new();
        assert!(buffer.is_empty());
        assert_eq!(buffer.max_lines, DEFAULT_MAX_LINES);
    }

    #[test]
    fn test_push_logs() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "Started on :8080".to_string(), LogSource::Process);
        buffer.push_line("server", "Connection accepted".to_string(), LogSource::Process);
        buffer.push_line("worker", "Processing job 1".to_string(), LogSource::Process);

        assert!(!buffer.is_empty());
        assert_eq!(buffer.len_for("server"), 2);
        assert_eq!(buffer.len_for("worker"), 1);
        assert_eq!(buffer.total_len(), 3);
    }

    #[test]
    fn test_max_lines_enforced() {
        let mut buffer = LogBuffer::with_max_lines(3);

        for i in 0..5 {
            buffer.push_line("test", format!("Line {}", i), LogSource::Process);
        }

        assert_eq!(buffer.len_for("test"), 3);

        let logs = buffer.raw_logs("test").unwrap();
        let lines: Vec<&str> = logs.iter().map(|s| s.content.as_str()).collect();
        assert_eq!(lines, vec!["Line 2", "Line 3", "Line 4"]);
    }

    #[test]
    fn test_filter() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "INFO: Started".to_string(), LogSource::Process);
        buffer.push_line("server", "DEBUG: Details".to_string(), LogSource::Process);
        buffer.push_line("server", "ERROR: Failed".to_string(), LogSource::Process);
        buffer.push_line("server", "INFO: Running".to_string(), LogSource::Process);

        // No filter
        let logs = buffer.filtered_logs("server");
        assert_eq!(logs.len(), 4);

        // Set filter
        buffer.set_filter("INFO").unwrap();
        assert!(buffer.has_filter());

        let logs = buffer.filtered_logs("server");
        assert_eq!(logs.len(), 2);
        assert!(logs.iter().all(|(_, log)| log.content.contains("INFO")));

        // Clear filter
        buffer.clear_filter();
        assert!(!buffer.has_filter());
        let logs = buffer.filtered_logs("server");
        assert_eq!(logs.len(), 4);
    }

    #[test]
    fn test_invalid_filter() {
        let mut buffer = LogBuffer::new();
        let result = buffer.set_filter("[invalid");
        assert!(result.is_err());
        assert!(!buffer.has_filter());
    }

    #[test]
    fn test_search() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "Line 1".to_string(), LogSource::Process);
        buffer.push_line("server", "match here".to_string(), LogSource::Process);
        buffer.push_line("server", "Line 3".to_string(), LogSource::Process);
        buffer.push_line("server", "Another match".to_string(), LogSource::Process);

        buffer.start_search("match").unwrap();
        assert!(buffer.has_search());

        // Check if lines match
        assert!(!buffer.is_search_match("Line 1"));
        assert!(buffer.is_search_match("match here"));
        assert!(buffer.is_search_match("Another match"));

        buffer.clear_search();
        assert!(!buffer.has_search());
    }

    #[test]
    fn test_search_case_insensitive() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "Match here".to_string(), LogSource::Process);

        // Case-insensitive search with regex
        buffer.start_search("(?i)match").unwrap();
        assert!(buffer.is_search_match("Match here"));
        assert!(buffer.is_search_match("MATCH"));
        assert!(buffer.is_search_match("match"));
    }

    #[test]
    fn test_search_navigation() {
        let lines = vec!["Line 1", "Match A", "Line 3", "Match B", "Match C"];

        let mut state = SearchState::new("Match").unwrap();
        state.update_matches(&lines);

        assert_eq!(state.total_matches, 3);
        assert_eq!(state.current_line(), Some(1)); // Match A

        assert_eq!(state.next_match(), Some(3)); // Match B
        assert_eq!(state.next_match(), Some(4)); // Match C
        assert_eq!(state.next_match(), Some(1)); // Wrap to Match A

        assert_eq!(state.prev_match(), Some(4)); // Match C
        assert_eq!(state.prev_match(), Some(3)); // Match B
    }

    #[test]
    fn test_follow_mode() {
        let mut buffer = LogBuffer::new();
        assert!(buffer.is_following()); // Default on

        buffer.disable_follow();
        assert!(!buffer.is_following());

        buffer.toggle_follow();
        assert!(buffer.is_following());

        buffer.toggle_follow();
        assert!(!buffer.is_following());
    }

    #[test]
    fn test_clear() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "Line 1".to_string(), LogSource::Process);
        buffer.push_line("worker", "Line 2".to_string(), LogSource::Process);

        buffer.clear_process("server");
        assert_eq!(buffer.len_for("server"), 0);
        assert_eq!(buffer.len_for("worker"), 1);

        buffer.clear_all();
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_all_logs_combined() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "Server line 1".to_string(), LogSource::Process);
        buffer.push_line("worker", "Worker line 1".to_string(), LogSource::Process);
        buffer.push_line("server", "Server line 2".to_string(), LogSource::Process);

        let combined = buffer.all_logs_combined();
        assert_eq!(combined.len(), 3);
    }

    #[test]
    fn test_selected_process() {
        let mut buffer = LogBuffer::new();
        assert!(buffer.selected_process().is_none());

        buffer.select_process(Some("server".to_string()));
        assert_eq!(buffer.selected_process(), Some("server"));

        buffer.select_process(None);
        assert!(buffer.selected_process().is_none());
    }

    #[test]
    fn test_push_log_line_struct() {
        use crate::daemon::LogSource;

        let mut buffer = LogBuffer::new();
        buffer.push(LogLine {
            timestamp: 0,
            process: "server".to_string(),
            group: None,
            content: "Started".to_string(),
            source: LogSource::Process,
        });

        assert_eq!(buffer.len_for("server"), 1);
    }
}

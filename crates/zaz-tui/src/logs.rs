//! Log buffer system for the TUI.
//!
//! Provides per-process log storage with filtering and search capabilities.

use regex::Regex;
use std::collections::{HashMap, VecDeque};
use std::sync::LazyLock;

use crate::daemon::{LogLine, LogSource};

/// Regex to match ANSI escape sequences that are NOT color/style codes.
/// Keeps: `\x1b[...m` (SGR color/style codes)
/// Strips: everything else that could corrupt TUI display.
static STRIP_ANSI_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        // CSI: ESC[ or 0x9B, with optional params/intermediates, final byte NOT 'm'
        // Parameter bytes: 0x30-0x3F, Intermediate bytes: 0x20-0x2F, Final: 0x40-0x7E (not m)
        r"\x1b\[[\x20-\x3f]*[\x40-\x6c\x6e-\x7e]",
        r"|\x9b[\x20-\x3f]*[\x40-\x6c\x6e-\x7e]",
        // Character set: ESC with ( ) * + - . / and designator
        r"|\x1b[()*.+\-./].",
        // nF sequences: ESC + SP ! " # $ % & followed by char (0x20-0x26)
        r"|\x1b[\x20-\x26].",
        // Fe escapes (ESC + 0x40-0x5F) - all except [ ] P X ^ _ (multi-char introducers)
        r"|\x1b[@A-OQ-WYZ\x5c]",
        // Fs escapes (ESC + 0x60-0x7E) - all lowercase except m (0x6D)
        r"|\x1b[\x60-\x6c\x6e-\x7e]",
        // Fp escapes (ESC + 0x30-0x3F) - private use, includes 7,8,=,>,etc
        r"|\x1b[\x30-\x3f]",
        // OSC: ESC] or 0x9D, terminated by BEL (0x07) or ST (ESC\ or 0x9C)
        r"|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)?",
        r"|\x9d[^\x07\x1b\x9c]*(?:\x07|\x1b\\|\x9c)?",
        // DCS, SOS, APC, PM: ESC P/X/_/^ or C1 equivalents, terminated by ST
        r"|\x1b[PX_^][^\x1b]*(?:\x1b\\)?",
        r"|[\x90\x98\x9e\x9f][^\x1b\x9c]*(?:\x1b\\|\x9c)?",
        // Other 8-bit C1 controls (0x80-0x9F)
        r"|[\x80-\x9a\x9c]",
        // C0 controls (except TAB 0x09, LF 0x0A, ESC 0x1B) and DEL
        r"|[\x00-\x08\x0b-\x0c\x0e-\x1a\x1c-\x1f\x7f]",
        // Unicode control characters that affect display
        // U+200B-U+200F: zero-width/formatting, U+2028-U+2029: line/para sep
        // U+202A-U+202E: bidi controls, U+2060-U+206F: word joiner etc, U+FEFF: BOM
        r"|[\u{200B}-\u{200F}\u{2028}-\u{2029}\u{202A}-\u{202E}\u{2060}-\u{206F}\u{FEFF}]",
    ))
    .unwrap()
});

/// Sanitize a log line by stripping terminal control sequences that would
/// corrupt the TUI display while preserving ANSI SGR color codes.
fn sanitize_log_content(content: &str) -> String {
    // Strip escape sequences and control characters
    let sanitized = STRIP_ANSI_REGEX.replace_all(content, "");
    // Also strip carriage returns (progress overwrites) and backspaces
    let sanitized = sanitized.replace(['\r', '\x08'], "");
    // Convert tabs to spaces to avoid terminal tab expansion misalignment
    // Use 4 spaces per tab as a reasonable default for code output
    sanitized.replace('\t', "    ")
}

/// A stored log entry with source info.
#[derive(Debug, Clone)]
pub struct StoredLog {
    /// Timestamp in milliseconds since Unix epoch.
    pub timestamp: u64,
    /// The log content.
    pub content: String,
    /// Source of the log.
    pub source: LogSource,
}

impl StoredLog {
    /// Format the timestamp for display.
    ///
    /// - `reference_day`: The day number of the first log (for calculating +N days)
    /// - `full`: If true, show full date-time; if false, show compact time with day offset
    pub fn format_timestamp(&self, reference_day: u64, full: bool) -> String {
        format_timestamp_ms(self.timestamp, reference_day, full)
    }
}

/// Format a timestamp in milliseconds for display.
///
/// - `timestamp_ms`: Unix timestamp in milliseconds
/// - `reference_day`: The day number of the first log (for calculating +N days)
/// - `full`: If true, show full date-time; if false, show compact time with day offset
pub fn format_timestamp_ms(timestamp_ms: u64, reference_day: u64, full: bool) -> String {
    let secs = timestamp_ms / 1000;

    // Convert to local time
    let local_secs = secs as i64 + local_offset_secs();

    // Calculate time components (handle negative values from timezone)
    let adjusted_secs = if local_secs < 0 { 0 } else { local_secs };
    let time_of_day = adjusted_secs % 86400;
    let hours = (time_of_day / 3600) % 24;
    let minutes = (time_of_day / 60) % 60;
    let seconds = time_of_day % 60;

    if full {
        // Full format: YYYY-MM-DD HH:MM:SS
        let days_since_epoch = adjusted_secs / 86400;
        let (year, month, day) = days_to_ymd(days_since_epoch);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            year, month, day, hours, minutes, seconds
        )
    } else {
        // Compact format: HH:MM:SS or HH:MM:SS +N
        let current_day = (adjusted_secs / 86400) as u64;
        let day_offset = current_day.saturating_sub(reference_day);

        if day_offset == 0 {
            format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
        } else {
            format!("{:02}:{:02}:{:02} +{}", hours, minutes, seconds, day_offset)
        }
    }
}

/// Get the day number for a timestamp (for reference calculations).
pub fn timestamp_to_day(timestamp_ms: u64) -> u64 {
    let secs = timestamp_ms / 1000;
    let local_secs = secs as i64 + local_offset_secs();
    let adjusted_secs = if local_secs < 0 { 0 } else { local_secs };
    (adjusted_secs / 86400) as u64
}

/// Get local timezone offset in seconds.
///
/// Uses the system's current local time to calculate offset from UTC.
fn local_offset_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    // Get current time in both UTC and local
    let now = SystemTime::now();
    let utc_secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Use the tm_gmtoff from localtime if available (Unix)
    // For simplicity, we'll calculate based on current time behavior
    #[cfg(unix)]
    {
        use std::mem::MaybeUninit;

        unsafe {
            let time_t = utc_secs as i64;
            let mut tm = MaybeUninit::<libc_tm>::uninit();

            extern "C" {
                fn localtime_r(timep: *const i64, result: *mut libc_tm) -> *mut libc_tm;
            }

            if !localtime_r(&time_t, tm.as_mut_ptr()).is_null() {
                let tm = tm.assume_init();
                return tm.tm_gmtoff;
            }
        }
    }

    0 // Default to UTC
}

/// Minimal tm struct for Unix localtime_r
#[cfg(unix)]
#[repr(C)]
struct libc_tm {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
    tm_gmtoff: i64,
    tm_zone: *const i8,
}

/// Convert days since epoch to year/month/day.
fn days_to_ymd(days: i64) -> (i32, u32, u32) {
    // Simple algorithm - doesn't handle all edge cases perfectly
    let mut remaining = days;
    let mut year = 1970i32;

    // Count years
    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        year += 1;
    }

    // Count months
    let month_days = if is_leap_year(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 1u32;
    for &days_in_month in &month_days {
        if remaining < days_in_month {
            break;
        }
        remaining -= days_in_month;
        month += 1;
    }

    let day = remaining as u32 + 1;
    (year, month, day)
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
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
        let buffer = self.logs.entry(log.process).or_default();
        buffer.push_back(StoredLog {
            timestamp: log.timestamp,
            content: sanitize_log_content(&log.content),
            source: log.source,
        });

        // Enforce max lines
        while buffer.len() > self.max_lines {
            buffer.pop_front();
        }
    }

    /// Add a log line with process, content, timestamp, and source directly.
    pub fn push_line(&mut self, process: &str, content: String, timestamp: u64, source: LogSource) {
        let buffer = self.logs.entry(process.to_string()).or_default();
        buffer.push_back(StoredLog {
            timestamp,
            content: sanitize_log_content(&content),
            source,
        });

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
    /// Returns (process, line_index, StoredLog) tuples sorted by timestamp.
    pub fn all_logs_combined(&self) -> Vec<(&str, usize, &StoredLog)> {
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

        // Sort by timestamp for chronological display
        result.sort_by_key(|(_, _, log)| log.timestamp);

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
        buffer.push_line(
            "server",
            "Started on :8080".to_string(),
            1000,
            LogSource::Process,
        );
        buffer.push_line(
            "server",
            "Connection accepted".to_string(),
            2000,
            LogSource::Process,
        );
        buffer.push_line(
            "worker",
            "Processing job 1".to_string(),
            3000,
            LogSource::Process,
        );

        assert!(!buffer.is_empty());
        assert_eq!(buffer.len_for("server"), 2);
        assert_eq!(buffer.len_for("worker"), 1);
        assert_eq!(buffer.total_len(), 3);
    }

    #[test]
    fn test_max_lines_enforced() {
        let mut buffer = LogBuffer::with_max_lines(3);

        for i in 0..5 {
            buffer.push_line(
                "test",
                format!("Line {}", i),
                i as u64 * 1000,
                LogSource::Process,
            );
        }

        assert_eq!(buffer.len_for("test"), 3);

        let logs = buffer.raw_logs("test").unwrap();
        let lines: Vec<&str> = logs.iter().map(|s| s.content.as_str()).collect();
        assert_eq!(lines, vec!["Line 2", "Line 3", "Line 4"]);
    }

    #[test]
    fn test_filter() {
        let mut buffer = LogBuffer::new();
        buffer.push_line(
            "server",
            "INFO: Started".to_string(),
            1000,
            LogSource::Process,
        );
        buffer.push_line(
            "server",
            "DEBUG: Details".to_string(),
            2000,
            LogSource::Process,
        );
        buffer.push_line(
            "server",
            "ERROR: Failed".to_string(),
            3000,
            LogSource::Process,
        );
        buffer.push_line(
            "server",
            "INFO: Running".to_string(),
            4000,
            LogSource::Process,
        );

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
        buffer.push_line("server", "Line 1".to_string(), 1000, LogSource::Process);
        buffer.push_line("server", "match here".to_string(), 2000, LogSource::Process);
        buffer.push_line("server", "Line 3".to_string(), 3000, LogSource::Process);
        buffer.push_line(
            "server",
            "Another match".to_string(),
            4000,
            LogSource::Process,
        );

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
        buffer.push_line("server", "Match here".to_string(), 1000, LogSource::Process);

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
        buffer.push_line("server", "Line 1".to_string(), 1000, LogSource::Process);
        buffer.push_line("worker", "Line 2".to_string(), 2000, LogSource::Process);

        buffer.clear_process("server");
        assert_eq!(buffer.len_for("server"), 0);
        assert_eq!(buffer.len_for("worker"), 1);

        buffer.clear_all();
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_all_logs_combined() {
        let mut buffer = LogBuffer::new();
        buffer.push_line(
            "server",
            "Server line 1".to_string(),
            1000,
            LogSource::Process,
        );
        buffer.push_line(
            "worker",
            "Worker line 1".to_string(),
            2000,
            LogSource::Process,
        );
        buffer.push_line(
            "server",
            "Server line 2".to_string(),
            3000,
            LogSource::Process,
        );

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

    #[test]
    fn test_sanitize_log_content() {
        // Cursor movement codes should be stripped
        let input = "Building \x1b[2K\x1b[1Gcrate v1.0";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Building crate v1.0");

        // Color codes should be preserved
        let input = "\x1b[31mError\x1b[0m: something failed";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "\x1b[31mError\x1b[0m: something failed");

        // Carriage returns should be stripped
        let input = "Progress: 50%\r";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Progress: 50%");

        // Mix of both
        let input = "\x1b[2K\x1b[1G\x1b[32mDone\x1b[0m\r";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "\x1b[32mDone\x1b[0m");

        // Hide/show cursor should be stripped
        let input = "\x1b[?25lBuilding...\x1b[?25h";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Building...");

        // Delete/insert characters should be stripped
        let input = "Test\x1b[2P\x1b[3@output";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Testoutput");

        // Backspaces should be stripped
        let input = "abc\x08\x08xy";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "abcxy");

        // Insert/delete lines should be stripped
        let input = "\x1b[2L\x1b[1MContent";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Content");

        // Scrolling region should be stripped
        let input = "\x1b[1;24rScrolled content";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Scrolled content");

        // OSC sequences with BEL terminator should be stripped
        let input = "\x1b]0;Window Title\x07Some content";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Some content");

        // OSC sequences with ST terminator should be stripped
        let input = "\x1b]0;Window Title\x1b\\Some content";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Some content");

        // Hyperlink OSC sequences should be stripped
        let input = "\x1b]8;;https://example.com\x07Link Text\x1b]8;;\x07 more text";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Link Text more text");

        // ESC SP sequences are stripped (7/8-bit control mode switches)
        let input = "\x1b Psome device control\x1b\\Content";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "some device controlContent");

        // Actual DCS sequence
        let input = "\x1bPsome;data\x1b\\Content";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Content");

        // APC sequences should be stripped
        let input = "\x1b_application program command\x1b\\Content";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Content");

        // PM sequences should be stripped
        let input = "\x1b^privacy message\x1b\\Content";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Content");

        // Mix of OSC and color codes - colors preserved
        let input = "\x1b]8;;url\x07\x1b[31mRed Link\x1b[0m\x1b]8;;\x07";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "\x1b[31mRed Link\x1b[0m");

        // VPA (line position absolute) - \x1b[nd
        let input = "Start\x1b[5dMiddle";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "StartMiddle");

        // HPR (character position relative) - \x1b[na
        let input = "Start\x1b[10aMiddle";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "StartMiddle");

        // REP (repeat previous character) - \x1b[nb
        let input = "X\x1b[5bY";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "XY");

        // CHA (cursor horizontal absolute) - \x1b[nG
        let input = "Start\x1b[20GEnd";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "StartEnd");

        // Non-CSI escape sequences: IND, NEL, RI, HTS
        let input = "Line1\x1bDLine2"; // IND (index)
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Line1Line2");

        let input = "Line1\x1bELine2"; // NEL (next line)
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Line1Line2");

        let input = "Line1\x1bMLine2"; // RI (reverse index)
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Line1Line2");

        let input = "Tab\x1bHhere"; // HTS (horizontal tab set)
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Tabhere");

        // CBT (cursor backward tabulation) - \x1b[nZ
        let input = "Start\x1b[2ZEnd";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "StartEnd");

        // CHT (cursor forward tabulation) - \x1b[nI
        let input = "Start\x1b[3IEnd";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "StartEnd");

        // Window manipulation - \x1b[nt
        let input = "\x1b[22tContent\x1b[23t";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Content");

        // Keypad modes - \x1b= and \x1b>
        let input = "\x1b=Content\x1b>";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Content");

        // RIS (full reset) - \x1bc
        let input = "\x1bcContent";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Content");

        // C0 controls (NUL, etc.) should be stripped
        let input = "A\x00\x01\x02B";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "AB");

        // DEL (0x7F) should be stripped
        let input = "A\x7fB";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "AB");

        // Tab (0x09) should be converted to spaces
        let input = "A\tB";
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "A    B");

        // Character set designation variants
        let input = "\x1b)0\x1b*B\x1b+AContent"; // G1, G2, G3 sets
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Content");

        // Hash sequences (DECALN, etc.)
        let input = "\x1b#8Content"; // DECALN screen alignment test
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Content");

        // Percent sequences (character set switching)
        let input = "\x1b%@Content\x1b%G"; // Switch to/from UTF-8
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Content");

        // CSI with intermediate bytes
        let input = "Start\x1b[0$xEnd"; // DECSCA with $ intermediate
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "StartEnd");

        // Fe escapes (ESC + uppercase)
        let input = "\x1b@\x1bA\x1bBContent"; // Various Fe sequences
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Content");

        // Fp escapes (ESC + digits/symbols 0x30-0x3F)
        let input = "\x1b0\x1b1\x1b<Content"; // Private use escapes
        let sanitized = sanitize_log_content(input);
        assert_eq!(sanitized, "Content");
    }

    #[test]
    fn test_push_sanitizes_content() {
        let mut buffer = LogBuffer::new();
        buffer.push_line(
            "test",
            "Building \x1b[2K\x1b[1Gcrate".to_string(),
            1000,
            LogSource::Process,
        );

        let logs = buffer.raw_logs("test").unwrap();
        assert_eq!(logs[0].content, "Building crate");
    }
}

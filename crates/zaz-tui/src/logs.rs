//! Log buffer system for the TUI.
//!
//! Provides per-process log storage with filtering and search capabilities.

use regex::Regex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::LazyLock;
use std::time::Instant;

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

/// Lines per page for lazy loading.
pub const PAGE_SIZE: usize = 200;

/// Maximum number of cached pages per name.
const MAX_CACHED_PAGES: usize = 10;

/// A log entry returned from paged access.
#[derive(Debug, Clone)]
pub struct PagedLogEntry {
    /// The log entry.
    pub log: StoredLog,
    /// Process name (relevant for combined "*" view).
    pub process: String,
}

/// A page of cached log lines fetched from the daemon.
#[derive(Debug, Clone)]
struct CachedPage {
    /// Lines in this page: (process_name, stored_log).
    lines: Vec<(String, StoredLog)>,
    /// When this page was fetched.
    #[allow(dead_code)]
    fetched_at: Instant,
}

/// LRU page cache for lazily-loaded historical logs.
#[derive(Debug)]
struct PageCache {
    /// Cached pages keyed by page number.
    pages: HashMap<usize, CachedPage>,
    /// LRU ordering (front = least recently inserted).
    lru_order: VecDeque<usize>,
    /// Maximum number of cached pages.
    max_pages: usize,
    /// Pages currently being fetched.
    pending: HashSet<usize>,
}

impl PageCache {
    fn new() -> Self {
        Self {
            pages: HashMap::new(),
            lru_order: VecDeque::new(),
            max_pages: MAX_CACHED_PAGES,
            pending: HashSet::new(),
        }
    }

    /// Insert a page of data, evicting the oldest if at capacity.
    fn insert(&mut self, page_num: usize, lines: Vec<(String, StoredLog)>) {
        // Remove from LRU if already present
        self.lru_order.retain(|&p| p != page_num);

        // Add to back of LRU (most recently inserted)
        self.lru_order.push_back(page_num);

        self.pages.insert(
            page_num,
            CachedPage {
                lines,
                fetched_at: Instant::now(),
            },
        );

        self.pending.remove(&page_num);

        // Evict if over capacity
        while self.pages.len() > self.max_pages {
            if let Some(evicted) = self.lru_order.pop_front() {
                self.pages.remove(&evicted);
            }
        }
    }

    /// Get a line from a cached page.
    fn get_line(&self, page_num: usize, offset_in_page: usize) -> Option<&(String, StoredLog)> {
        self.pages
            .get(&page_num)
            .and_then(|p| p.lines.get(offset_in_page))
    }

    /// Check if a page is cached.
    fn has_page(&self, page_num: usize) -> bool {
        self.pages.contains_key(&page_num)
    }

    /// Check if a page is pending (being fetched).
    fn is_pending(&self, page_num: usize) -> bool {
        self.pending.contains(&page_num)
    }

    /// Mark a page as pending.
    fn mark_pending(&mut self, page_num: usize) {
        self.pending.insert(page_num);
    }

    /// Clear pending status for a page.
    fn clear_pending(&mut self, page_num: usize) {
        self.pending.remove(&page_num);
    }
}

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
    /// Per-name page caches for lazily-loaded historical logs.
    page_caches: HashMap<String, PageCache>,
    /// Total log counts reported by the daemon (per name).
    daemon_total_counts: HashMap<String, usize>,
    /// Total streamed log counts seen by this TUI client (per process).
    local_received_counts: HashMap<String, usize>,
    /// Total streamed log count seen by this TUI client across all processes.
    local_received_total: usize,
    /// Local clear cutoffs keyed by view name (`"*"` for combined view).
    clear_cutoffs: HashMap<String, ClearCutoff>,
}

#[derive(Debug, Clone, Copy)]
struct ClearCutoff {
    /// Absolute daemon/log index hidden by this clear, when known.
    daemon_base: Option<usize>,
    /// Absolute locally streamed index hidden by this clear.
    local_base: usize,
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
            page_caches: HashMap::new(),
            daemon_total_counts: HashMap::new(),
            local_received_counts: HashMap::new(),
            local_received_total: 0,
            clear_cutoffs: HashMap::new(),
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
            page_caches: HashMap::new(),
            daemon_total_counts: HashMap::new(),
            local_received_counts: HashMap::new(),
            local_received_total: 0,
            clear_cutoffs: HashMap::new(),
        }
    }

    /// Add a log line for a process.
    pub fn push(&mut self, log: LogLine) {
        self.local_received_total += 1;
        *self
            .local_received_counts
            .entry(log.process.clone())
            .or_default() += 1;

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
        self.local_received_total += 1;
        *self
            .local_received_counts
            .entry(process.to_string())
            .or_default() += 1;

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
        self.page_caches.clear();
        self.daemon_total_counts.clear();
        self.local_received_counts.clear();
        self.local_received_total = 0;
        self.clear_cutoffs.clear();
    }

    /// Clear logs for a specific process.
    pub fn clear_process(&mut self, process: &str) {
        if let Some(buffer) = self.logs.get_mut(process) {
            buffer.clear();
        }
        self.page_caches.remove(process);
        self.daemon_total_counts.remove(process);
        self.local_received_counts.remove(process);
        self.clear_cutoffs.remove(process);
    }

    /// Clear a view locally for this TUI client without mutating daemon state.
    pub fn clear_view(&mut self, name: &str) {
        let daemon_base = if name == "*" || self.daemon_total_counts.contains_key(name) {
            Some(self.raw_total_count(name))
        } else {
            None
        };

        self.clear_cutoffs.insert(
            name.to_string(),
            ClearCutoff {
                daemon_base,
                local_base: self.local_received_count(name),
            },
        );
        self.page_caches.remove(name);
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

        let local_start = self.local_buffer_start(process, buffer.len());
        let cutoff = self
            .clear_cutoffs
            .get(process)
            .map(|c| c.local_base)
            .unwrap_or(0);

        match &self.filter {
            Some(regex) => buffer
                .iter()
                .enumerate()
                .filter(|(idx, log)| local_start + *idx >= cutoff && regex.is_match(&log.content))
                .collect(),
            None => buffer
                .iter()
                .enumerate()
                .filter(|(idx, _)| local_start + *idx >= cutoff)
                .collect(),
        }
    }

    /// Get all logs combined (for full style) with process prefix.
    ///
    /// Returns (process, line_index, StoredLog) tuples sorted by timestamp.
    pub fn all_logs_combined(&self) -> Vec<(&str, usize, &StoredLog)> {
        let local_start = self.local_received_total.saturating_sub(self.total_len());
        let cutoff = self
            .clear_cutoffs
            .get("*")
            .map(|c| c.local_base)
            .unwrap_or(0);
        let mut result = self.all_logs_combined_raw();
        result.sort_by_key(|(_, _, log)| log.timestamp);
        result
            .into_iter()
            .enumerate()
            .filter_map(|(idx, (process, _raw_idx, log))| {
                let filtered = match &self.filter {
                    Some(regex) => regex.is_match(&log.content),
                    None => true,
                };
                let absolute_idx = local_start + idx;
                if filtered && absolute_idx >= cutoff {
                    Some((process, absolute_idx, log))
                } else {
                    None
                }
            })
            .collect()
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

    // === Page cache methods ===

    /// Update the total log count from the daemon for a given name.
    pub fn set_total_count(&mut self, name: &str, count: usize) {
        self.daemon_total_counts.insert(name.to_string(), count);
    }

    /// Get the total log count for a name.
    ///
    /// When no filter is active, returns the maximum of the daemon's reported
    /// count and the local buffer length. When a filter is active, returns
    /// the filtered local count (lazy loading is disabled with filters).
    pub fn total_count(&self, name: &str) -> usize {
        if self.filter.is_some() {
            // With filter active, only local data
            if name == "*" {
                self.all_logs_combined().len()
            } else {
                self.filtered_logs(name).len()
            }
        } else {
            let clear = self.clear_cutoffs.get(name);
            if let Some(clear) = clear {
                if let Some(daemon_base) = clear.daemon_base {
                    self.raw_total_count(name).saturating_sub(daemon_base)
                } else {
                    self.local_received_count(name)
                        .saturating_sub(clear.local_base)
                }
            } else {
                self.raw_total_count(name)
            }
        }
    }

    /// Get lines for display using paginated access.
    ///
    /// Returns a vector of entries for offsets `[start, start+count)`.
    /// Each entry is `Some(PagedLogEntry)` if the line is available, or `None`
    /// if it needs to be fetched from the daemon.
    ///
    /// When a filter is active, falls back to existing local-only behavior.
    pub fn get_display_lines(
        &self,
        name: &str,
        start: usize,
        count: usize,
    ) -> Vec<Option<PagedLogEntry>> {
        if self.filter.is_some() {
            return self.get_filtered_display_lines(name, start, count);
        }

        let Some(daemon_start) = self.visible_to_daemon_offset(name, start) else {
            return self.get_local_only_display_lines(name, start, count);
        };

        let total = self.total_count(name);
        let raw_total = self.raw_total_count(name);
        let cache = self.page_caches.get(name);

        // Compute tail data
        let tail_data: Vec<(&str, &StoredLog)> = if name == "*" {
            self.all_logs_combined_raw()
                .into_iter()
                .map(|(proc, _idx, log)| (proc, log))
                .collect()
        } else {
            self.logs
                .get(name)
                .map(|buf| buf.iter().map(|log| (name, log)).collect())
                .unwrap_or_default()
        };
        let tail_len = tail_data.len();
        let tail_start = raw_total.saturating_sub(tail_len);

        let mut result = Vec::with_capacity(count);

        for visible_offset in start..start + count {
            if visible_offset >= total {
                break;
            }

            let offset = daemon_start + (visible_offset - start);

            if offset >= tail_start {
                // In tail range — serve from local buffer
                let tail_idx = offset - tail_start;
                if let Some(&(proc_name, log)) = tail_data.get(tail_idx) {
                    result.push(Some(PagedLogEntry {
                        log: log.clone(),
                        process: proc_name.to_string(),
                    }));
                } else {
                    result.push(None);
                }
            } else {
                // In page cache range
                let page_num = offset / PAGE_SIZE;
                let offset_in_page = offset % PAGE_SIZE;

                if let Some(c) = cache {
                    if let Some((proc_name, log)) = c.get_line(page_num, offset_in_page) {
                        result.push(Some(PagedLogEntry {
                            log: log.clone(),
                            process: proc_name.clone(),
                        }));
                    } else {
                        result.push(None);
                    }
                } else {
                    result.push(None);
                }
            }
        }

        result
    }

    /// Fallback for `get_display_lines` when a filter is active.
    fn get_filtered_display_lines(
        &self,
        name: &str,
        start: usize,
        count: usize,
    ) -> Vec<Option<PagedLogEntry>> {
        if name == "*" {
            self.all_logs_combined()
                .into_iter()
                .skip(start)
                .take(count)
                .map(|(proc, _idx, log)| {
                    Some(PagedLogEntry {
                        log: log.clone(),
                        process: proc.to_string(),
                    })
                })
                .collect()
        } else {
            self.filtered_logs(name)
                .into_iter()
                .skip(start)
                .take(count)
                .map(|(_idx, log)| {
                    Some(PagedLogEntry {
                        log: log.clone(),
                        process: name.to_string(),
                    })
                })
                .collect()
        }
    }

    fn get_local_only_display_lines(
        &self,
        name: &str,
        start: usize,
        count: usize,
    ) -> Vec<Option<PagedLogEntry>> {
        if name == "*" {
            self.all_logs_combined()
                .into_iter()
                .skip(start)
                .take(count)
                .map(|(proc, _idx, log)| {
                    Some(PagedLogEntry {
                        log: log.clone(),
                        process: proc.to_string(),
                    })
                })
                .collect()
        } else {
            let Some(buffer) = self.logs.get(name) else {
                return Vec::new();
            };
            let local_start = self.local_buffer_start(name, buffer.len());
            let cutoff = self
                .clear_cutoffs
                .get(name)
                .map(|c| c.local_base)
                .unwrap_or(0);

            buffer
                .iter()
                .enumerate()
                .filter(|(idx, _)| local_start + *idx >= cutoff)
                .skip(start)
                .take(count)
                .map(|(_, log)| {
                    Some(PagedLogEntry {
                        log: log.clone(),
                        process: name.to_string(),
                    })
                })
                .collect()
        }
    }

    /// Insert a fetched page of log lines into the cache.
    pub fn insert_page(
        &mut self,
        name: &str,
        offset: usize,
        lines: Vec<LogLine>,
        total_count: usize,
    ) {
        self.daemon_total_counts
            .insert(name.to_string(), total_count);

        let page_num = offset / PAGE_SIZE;
        let cached_lines: Vec<(String, StoredLog)> = lines
            .into_iter()
            .map(|l| {
                (
                    l.process,
                    StoredLog {
                        timestamp: l.timestamp,
                        content: sanitize_log_content(&l.content),
                        source: l.source,
                    },
                )
            })
            .collect();

        let cache = self
            .page_caches
            .entry(name.to_string())
            .or_insert_with(PageCache::new);
        cache.insert(page_num, cached_lines);
    }

    /// Determine which page ranges need to be fetched for the given offset range.
    ///
    /// Returns `(offset, limit)` pairs for pages that are not cached,
    /// not pending, and not covered by the tail buffer.
    pub fn needs_fetch(&self, name: &str, start: usize, end: usize) -> Vec<(usize, usize)> {
        if self.filter.is_some() || end == 0 {
            return vec![];
        }

        let Some(actual_start) = self.visible_to_daemon_offset(name, start) else {
            return vec![];
        };
        let Some(actual_end) = self.visible_to_daemon_offset(name, end) else {
            return vec![];
        };

        let total = self.raw_total_count(name);
        let tail_len = if name == "*" {
            self.total_len()
        } else {
            self.len_for(name)
        };
        let tail_start = total.saturating_sub(tail_len);

        let cache = self.page_caches.get(name);

        let first_page = actual_start / PAGE_SIZE;
        let last_page = (actual_end.saturating_sub(1)) / PAGE_SIZE;

        let mut fetches = Vec::new();

        for page_num in first_page..=last_page {
            let page_start = page_num * PAGE_SIZE;

            // Skip if entirely covered by the tail buffer
            if page_start >= tail_start {
                continue;
            }

            // Skip if already cached or pending
            if let Some(c) = cache {
                if c.has_page(page_num) || c.is_pending(page_num) {
                    continue;
                }
            }

            let page_end = ((page_num + 1) * PAGE_SIZE).min(total);
            fetches.push((page_start, page_end - page_start));
        }

        fetches
    }

    /// Mark a page as pending (being fetched).
    pub fn mark_pending(&mut self, name: &str, page: usize) {
        let cache = self
            .page_caches
            .entry(name.to_string())
            .or_insert_with(PageCache::new);
        cache.mark_pending(page);
    }

    /// Clear pending status for a page.
    pub fn clear_pending(&mut self, name: &str, page: usize) {
        if let Some(cache) = self.page_caches.get_mut(name) {
            cache.clear_pending(page);
        }
    }

    fn raw_total_count(&self, name: &str) -> usize {
        let local_len = if name == "*" {
            self.total_len()
        } else {
            self.len_for(name)
        };
        let daemon_count = self.daemon_total_counts.get(name).copied().unwrap_or(0);
        daemon_count.max(local_len)
    }

    fn local_received_count(&self, name: &str) -> usize {
        if name == "*" {
            self.local_received_total
        } else {
            self.local_received_counts.get(name).copied().unwrap_or(0)
        }
    }

    fn local_buffer_start(&self, name: &str, len: usize) -> usize {
        self.local_received_count(name).saturating_sub(len)
    }

    fn visible_to_daemon_offset(&self, name: &str, visible_offset: usize) -> Option<usize> {
        match self.clear_cutoffs.get(name) {
            Some(cutoff) => cutoff.daemon_base.map(|base| base + visible_offset),
            None => Some(visible_offset),
        }
    }

    fn all_logs_combined_raw(&self) -> Vec<(&str, usize, &StoredLog)> {
        let mut result = Vec::new();

        for (process, buffer) in &self.logs {
            for (idx, log) in buffer.iter().enumerate() {
                result.push((process.as_str(), idx, log));
            }
        }

        result.sort_by_key(|(_, _, log)| log.timestamp);
        result
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
    fn test_clear_view_combined_hides_old_logs_and_accepts_new_ones() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "old 1".to_string(), 1000, LogSource::Process);
        buffer.push_line("worker", "old 2".to_string(), 2000, LogSource::Process);
        buffer.set_total_count("*", 1000);

        buffer.clear_view("*");

        assert_eq!(buffer.total_count("*"), 0);
        assert!(buffer.get_display_lines("*", 0, 5).is_empty());
        assert!(buffer.needs_fetch("*", 0, 5).is_empty());

        buffer.push_line("server", "new 1".to_string(), 3000, LogSource::Process);
        buffer.push_line("worker", "new 2".to_string(), 4000, LogSource::Process);
        buffer.set_total_count("*", 1002);

        let lines = buffer.get_display_lines("*", 0, 5);
        assert_eq!(buffer.total_count("*"), 2);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].as_ref().unwrap().log.content, "new 1");
        assert_eq!(lines[1].as_ref().unwrap().log.content, "new 2");
        assert!(buffer.needs_fetch("*", 0, 2).is_empty());
    }

    #[test]
    fn test_clear_view_process_hides_old_logs_without_daemon_total() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "old 1".to_string(), 1000, LogSource::Process);
        buffer.push_line("server", "old 2".to_string(), 2000, LogSource::Process);
        buffer.push_line("worker", "worker old".to_string(), 3000, LogSource::Process);

        buffer.clear_view("server");

        assert_eq!(buffer.total_count("server"), 0);
        assert!(buffer.get_display_lines("server", 0, 5).is_empty());
        assert!(buffer.needs_fetch("server", 0, 5).is_empty());
        assert_eq!(buffer.total_count("worker"), 1);

        buffer.push_line("server", "new 1".to_string(), 4000, LogSource::Process);
        buffer.push_line("server", "new 2".to_string(), 5000, LogSource::Process);

        let lines = buffer.get_display_lines("server", 0, 5);
        assert_eq!(buffer.total_count("server"), 2);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].as_ref().unwrap().log.content, "new 1");
        assert_eq!(lines[1].as_ref().unwrap().log.content, "new 2");
    }

    #[test]
    fn test_clear_view_combined_filter_uses_post_clear_subset() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "INFO old".to_string(), 1000, LogSource::Process);
        buffer.push_line(
            "worker",
            "INFO old worker".to_string(),
            2000,
            LogSource::Process,
        );
        buffer.clear_view("*");
        buffer.push_line("server", "INFO new".to_string(), 3000, LogSource::Process);
        buffer.push_line("worker", "DEBUG new".to_string(), 4000, LogSource::Process);

        buffer.set_filter("INFO").unwrap();

        let combined = buffer.all_logs_combined();
        assert_eq!(combined.len(), 1);
        assert_eq!(combined[0].2.content, "INFO new");
        assert_eq!(buffer.total_count("*"), 1);
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

    // === PageCache tests ===

    #[test]
    fn test_page_cache_insert_and_get() {
        let mut cache = PageCache::new();
        let lines = vec![
            (
                "proc".to_string(),
                StoredLog {
                    timestamp: 1000,
                    content: "line 0".to_string(),
                    source: LogSource::Process,
                },
            ),
            (
                "proc".to_string(),
                StoredLog {
                    timestamp: 2000,
                    content: "line 1".to_string(),
                    source: LogSource::Process,
                },
            ),
        ];

        cache.insert(0, lines);
        assert!(cache.has_page(0));
        assert!(!cache.has_page(1));

        let line = cache.get_line(0, 0).unwrap();
        assert_eq!(line.1.content, "line 0");

        let line = cache.get_line(0, 1).unwrap();
        assert_eq!(line.1.content, "line 1");

        assert!(cache.get_line(0, 2).is_none());
        assert!(cache.get_line(1, 0).is_none());
    }

    #[test]
    fn test_page_cache_eviction() {
        let mut cache = PageCache::new();
        // max_pages defaults to MAX_CACHED_PAGES (10)

        // Insert 11 pages — first should be evicted
        for i in 0..11 {
            cache.insert(
                i,
                vec![(
                    format!("proc_{}", i),
                    StoredLog {
                        timestamp: i as u64 * 1000,
                        content: format!("page {}", i),
                        source: LogSource::Process,
                    },
                )],
            );
        }

        // Page 0 should be evicted (first inserted)
        assert!(!cache.has_page(0));
        // Pages 1-10 should still be present
        for i in 1..11 {
            assert!(cache.has_page(i), "page {} should exist", i);
        }
    }

    #[test]
    fn test_page_cache_pending() {
        let mut cache = PageCache::new();

        assert!(!cache.is_pending(5));
        cache.mark_pending(5);
        assert!(cache.is_pending(5));

        cache.clear_pending(5);
        assert!(!cache.is_pending(5));

        // Insert clears pending automatically
        cache.mark_pending(3);
        cache.insert(3, vec![]);
        assert!(!cache.is_pending(3));
    }

    #[test]
    fn test_total_count_no_daemon() {
        let mut buffer = LogBuffer::new();
        // No daemon total set, should return local len
        assert_eq!(buffer.total_count("server"), 0);

        buffer.push_line("server", "line 1".to_string(), 1000, LogSource::Process);
        assert_eq!(buffer.total_count("server"), 1);
    }

    #[test]
    fn test_total_count_with_daemon() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "line 1".to_string(), 1000, LogSource::Process);

        // Daemon reports more than local
        buffer.set_total_count("server", 5000);
        assert_eq!(buffer.total_count("server"), 5000);

        // Daemon reports less than local (shouldn't happen, but max handles it)
        buffer.set_total_count("server", 0);
        assert_eq!(buffer.total_count("server"), 1);
    }

    #[test]
    fn test_total_count_combined() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "line 1".to_string(), 1000, LogSource::Process);
        buffer.push_line("worker", "line 2".to_string(), 2000, LogSource::Process);

        buffer.set_total_count("*", 10000);
        assert_eq!(buffer.total_count("*"), 10000);
    }

    #[test]
    fn test_total_count_with_filter() {
        let mut buffer = LogBuffer::new();
        buffer.push_line(
            "server",
            "INFO: started".to_string(),
            1000,
            LogSource::Process,
        );
        buffer.push_line(
            "server",
            "DEBUG: details".to_string(),
            2000,
            LogSource::Process,
        );
        buffer.push_line(
            "server",
            "INFO: running".to_string(),
            3000,
            LogSource::Process,
        );

        buffer.set_total_count("server", 10000);

        // Without filter, returns daemon total
        assert_eq!(buffer.total_count("server"), 10000);

        // With filter, returns filtered local count
        buffer.set_filter("INFO").unwrap();
        assert_eq!(buffer.total_count("server"), 2);
    }

    #[test]
    fn test_get_display_lines_tail_only() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "line 0".to_string(), 1000, LogSource::Process);
        buffer.push_line("server", "line 1".to_string(), 2000, LogSource::Process);
        buffer.push_line("server", "line 2".to_string(), 3000, LogSource::Process);

        // No daemon total set — total = local len = 3
        let lines = buffer.get_display_lines("server", 0, 3);
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.is_some()));
        assert_eq!(lines[0].as_ref().unwrap().log.content, "line 0");
        assert_eq!(lines[2].as_ref().unwrap().log.content, "line 2");
    }

    #[test]
    fn test_get_display_lines_with_gaps() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "tail line".to_string(), 5000, LogSource::Process);

        // Daemon has 1000 total lines, but we only have 1 locally
        buffer.set_total_count("server", 1000);

        // Request lines from the beginning — should have gaps
        let lines = buffer.get_display_lines("server", 0, 5);
        assert_eq!(lines.len(), 5);
        // Lines 0-4 are all before the tail (tail_start = 999)
        assert!(lines.iter().all(|l| l.is_none()));

        // Request the last line — should be from the tail
        let lines = buffer.get_display_lines("server", 999, 1);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].is_some());
        assert_eq!(lines[0].as_ref().unwrap().log.content, "tail line");
    }

    #[test]
    fn test_get_display_lines_with_cached_page() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "tail".to_string(), 5000, LogSource::Process);
        buffer.set_total_count("server", 1000);

        // Insert a page at offset 0
        let page_lines = vec![
            LogLine {
                timestamp: 100,
                process: "server".to_string(),
                group: None,
                content: "old line 0".to_string(),
                source: LogSource::Process,
            },
            LogLine {
                timestamp: 200,
                process: "server".to_string(),
                group: None,
                content: "old line 1".to_string(),
                source: LogSource::Process,
            },
        ];
        buffer.insert_page("server", 0, page_lines, 1000);

        // Request from the cached page
        let lines = buffer.get_display_lines("server", 0, 2);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].as_ref().unwrap().log.content, "old line 0");
        assert_eq!(lines[1].as_ref().unwrap().log.content, "old line 1");
    }

    #[test]
    fn test_get_display_lines_combined() {
        let mut buffer = LogBuffer::new();
        buffer.push_line(
            "server",
            "server line".to_string(),
            1000,
            LogSource::Process,
        );
        buffer.push_line(
            "worker",
            "worker line".to_string(),
            2000,
            LogSource::Process,
        );

        let lines = buffer.get_display_lines("*", 0, 2);
        assert_eq!(lines.len(), 2);
        // Should be sorted by timestamp
        assert_eq!(lines[0].as_ref().unwrap().process, "server");
        assert_eq!(lines[1].as_ref().unwrap().process, "worker");
    }

    #[test]
    fn test_needs_fetch_tail_range() {
        let mut buffer = LogBuffer::new();
        for i in 0..10 {
            buffer.push_line(
                "server",
                format!("line {}", i),
                i as u64 * 1000,
                LogSource::Process,
            );
        }
        // total = local = 10, tail covers all
        let fetches = buffer.needs_fetch("server", 0, 10);
        assert!(fetches.is_empty());
    }

    #[test]
    fn test_needs_fetch_history() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "tail".to_string(), 5000, LogSource::Process);
        buffer.set_total_count("server", 1000);

        // Request lines 0-200 (page 0) — should need fetching
        let fetches = buffer.needs_fetch("server", 0, 200);
        assert_eq!(fetches.len(), 1);
        assert_eq!(fetches[0], (0, 200));
    }

    #[test]
    fn test_needs_fetch_skips_cached() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "tail".to_string(), 5000, LogSource::Process);
        buffer.set_total_count("server", 1000);

        // Cache page 0
        buffer.insert_page("server", 0, vec![], 1000);

        let fetches = buffer.needs_fetch("server", 0, 200);
        assert!(fetches.is_empty());
    }

    #[test]
    fn test_needs_fetch_skips_pending() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "tail".to_string(), 5000, LogSource::Process);
        buffer.set_total_count("server", 1000);

        // Mark page 0 as pending
        buffer.mark_pending("server", 0);

        let fetches = buffer.needs_fetch("server", 0, 200);
        assert!(fetches.is_empty());
    }

    #[test]
    fn test_needs_fetch_no_fetch_with_filter() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "tail".to_string(), 5000, LogSource::Process);
        buffer.set_total_count("server", 1000);
        buffer.set_filter("tail").unwrap();

        // With filter active, never fetch pages
        let fetches = buffer.needs_fetch("server", 0, 200);
        assert!(fetches.is_empty());
    }

    #[test]
    fn test_clear_all_resets_page_cache() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "line".to_string(), 1000, LogSource::Process);
        buffer.set_total_count("server", 5000);
        buffer.insert_page("server", 0, vec![], 5000);

        buffer.clear_all();

        assert_eq!(buffer.total_count("server"), 0);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_clear_process_resets_page_cache() {
        let mut buffer = LogBuffer::new();
        buffer.push_line("server", "line".to_string(), 1000, LogSource::Process);
        buffer.push_line("worker", "line".to_string(), 2000, LogSource::Process);
        buffer.set_total_count("server", 5000);
        buffer.set_total_count("worker", 3000);

        buffer.clear_process("server");

        assert_eq!(buffer.total_count("server"), 0);
        assert_eq!(buffer.total_count("worker"), 3000);
    }

    #[test]
    fn test_follow_mode_no_page_fetches() {
        let mut buffer = LogBuffer::new();
        for i in 0..100 {
            buffer.push_line(
                "server",
                format!("line {}", i),
                i as u64 * 100,
                LogSource::Process,
            );
        }
        buffer.set_total_count("server", 10000);

        // In follow mode, the last visible_height lines are from the tail
        // Request lines from the end (follow mode position)
        let start = 10000 - 20; // visible_height = 20
        let lines = buffer.get_display_lines("server", start, 20);
        // tail_start = 10000 - 100 = 9900, start = 9980 >= 9900
        // All should be from the tail
        assert_eq!(lines.len(), 20);
        assert!(lines.iter().all(|l| l.is_some()));
    }
}

// oxidread/src/readline/history.rs
//
// Copyright (C) 2025 Ali Zain <alizain.x404@gmail.com>
// Part of the Oxidread project — a pure Rust, memory-safe rewrite of
// GNU Readline and ncurses, built for Zainium OS.
//
// Oxidread is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This file is part of Zainium OS (https://zainium.os) and is open source
// for all compatible Linux distributions under GPL-3.0-or-later.
// See <https://www.gnu.org/licenses/> for the full license text.
//
// -----------------------------------------------------------------------
// history.rs — Interactive command history engine.
//
// GNU readline equivalents: history.c  histfile.c  histsearch.c
//
// What GNU readline does (and we improve):
//   • GNU stores history in a global HIST_ENTRY** array with raw C pointers.
//     Navigation state (history_offset) is also global, making it impossible
//     to have two independent readline instances.
//   • We use a struct `History` that owns its entries as Vec<HistoryEntry>,
//     with a separate NavigationCursor for up/down-arrow traversal.
//     Multiple Editor instances each own their own History — no global state.
//   • File I/O uses std::fs / BufReader — no fopen/malloc/free.
//   • Deduplication (skip consecutive identical entries) is built in, matching
//     bash HISTCONTROL=ignoredups behaviour.
//   • Max-size truncation on save matches bash HISTSIZE behaviour.
//
// Zainium OS integration:
//   • Default history file: ~/.zainium_history  (zainium_default_history_path)
//   • Library identity embedded via OXIDREAD_IDENTITY constant.
//   • `History::zainium_info()` returns a human-readable build string for
//     use in --version output or debug logs.
// -----------------------------------------------------------------------

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use crate::readline::error::{OxidreadError, Result};

// -----------------------------------------------------------------------
// Zainium OS identity — permanently embedded in the binary
// -----------------------------------------------------------------------

/// Library identity string embedded in every oxidread binary.
/// Shows up in `strings(1)` output and version banners.
pub const OXIDREAD_IDENTITY: &str =
    "oxidread 0.1.0 — pure Rust readline+ncurses | Zainium OS | (c) 2025 Ali Zain <alizain.x404@gmail.com> | GPL-3.0-or-later";

/// Default history file name used by Zainium OS.
pub const ZAINIUM_HISTORY_FILENAME: &str = ".zainium_history";

/// Default maximum number of history entries kept in memory.
pub const ZAINIUM_HISTORY_MAX: usize = 2_000;

/// Default maximum number of lines written to the history file on save.
pub const ZAINIUM_HISTFILE_MAX: usize = 5_000;

/// Returns the default history file path for the current user on Zainium OS.
///
/// Resolves `$HOME/.zainium_history`. Falls back to `./.zainium_history`
/// if `$HOME` is not set (e.g. in a sandboxed environment).
///
/// ```rust
/// let path = oxidread::readline::history::zainium_default_history_path();
/// // → /home/ali-zain/.zainium_history  (on Zainium OS)
/// ```
pub fn zainium_default_history_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(ZAINIUM_HISTORY_FILENAME)
}

/// Returns a human-readable identity + build string for oxidread.
///
/// Intended for use in `--version` output of any tool that embeds oxidread.
///
/// ```rust
/// println!("{}", oxidread::readline::history::zainium_info());
/// // oxidread 0.1.0 | Zainium OS readline engine | Ali Zain <alizain.x404@gmail.com>
/// ```
pub fn zainium_info() -> &'static str {
    OXIDREAD_IDENTITY
}

// -----------------------------------------------------------------------
// HistoryEntry
// -----------------------------------------------------------------------

/// A single entry in the command history.
///
/// Mirrors GNU readline's `HIST_ENTRY` struct but without raw C pointers.
/// The `timestamp` field is optional — it is written/read only when the
/// history file uses the extended bash timestamp format (`# <unix_secs>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    /// The command text. Always valid UTF-8.
    pub line: String,
    /// Optional Unix timestamp (seconds since epoch) when the command was run.
    pub timestamp: Option<u64>,
}

impl HistoryEntry {
    /// Create a new entry without a timestamp.
    pub fn new(line: impl Into<String>) -> Self {
        HistoryEntry {
            line: line.into(),
            timestamp: None,
        }
    }

    /// Create a new entry with a Unix timestamp.
    pub fn with_timestamp(line: impl Into<String>, ts: u64) -> Self {
        HistoryEntry {
            line: line.into(),
            timestamp: Some(ts),
        }
    }
}

// -----------------------------------------------------------------------
// NavigationCursor
// -----------------------------------------------------------------------

/// Tracks the current position during up/down-arrow history traversal.
///
/// GNU readline stores this as a plain `int history_offset` global.
/// We encapsulate it so the Editor can reset it cleanly on each new prompt.
///
/// State diagram:
///   AtEnd  ──up──▶  InHistory(len-1)  ──up──▶  InHistory(0)
///   AtEnd  ◀──down──  InHistory(n)    ◀──down──  InHistory(0)  [with saved draft]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationCursor {
    /// Not navigating — cursor is on the live (unsaved) draft line.
    AtEnd,
    /// Navigating — currently showing history entry at index `idx`.
    InHistory { idx: usize },
}

impl Default for NavigationCursor {
    fn default() -> Self {
        NavigationCursor::AtEnd
    }
}

// -----------------------------------------------------------------------
// History
// -----------------------------------------------------------------------

/// Command history: storage, navigation, search, and file persistence.
///
/// # Example
/// ```rust
/// use oxidread::readline::history::History;
///
/// let mut h = History::new(500);
/// h.push("ls -la");
/// h.push("cargo build");
/// h.push("cargo test");
///
/// // Navigate backwards (up arrow)
/// assert_eq!(h.prev(None).map(|e| e.line.as_str()), Some("cargo test"));
/// assert_eq!(h.prev(None).map(|e| e.line.as_str()), Some("cargo build"));
/// ```
#[derive(Debug, Clone)]
pub struct History {
    /// The stored entries, oldest first (index 0 = oldest).
    entries: Vec<HistoryEntry>,

    /// Maximum number of entries to keep in memory.
    max_entries: usize,

    /// Current navigation position. Reset to `AtEnd` on each new accept.
    cursor: NavigationCursor,

    /// The draft line saved when the user started navigating backwards.
    /// Restored when the user presses Down past the newest entry.
    /// Mirrors GNU readline's `_rl_saved_line_for_history`.
    saved_draft: Option<String>,

    /// If true, skip adding an entry if it is identical to the most recent one.
    /// Matches bash `HISTCONTROL=ignoredups`.
    pub dedup: bool,

    /// Path to persist history on save/load. None = no file persistence.
    pub file_path: Option<PathBuf>,

    /// Maximum number of lines to write on save.
    pub file_max: usize,
}

impl History {
    // ------------------------------------------------------------------ //
    //  Construction                                                       //
    // ------------------------------------------------------------------ //

    /// Create a new `History` with the given in-memory capacity.
    pub fn new(max_entries: usize) -> Self {
        History {
            entries: Vec::new(),
            max_entries,
            cursor: NavigationCursor::AtEnd,
            saved_draft: None,
            dedup: true,
            file_path: None,
            file_max: ZAINIUM_HISTFILE_MAX,
        }
    }

    /// Create a `History` pre-configured for Zainium OS:
    ///   - max 2 000 in-memory entries
    ///   - file at `~/.zainium_history`
    ///   - dedup enabled
    ///
    /// This is the constructor used by `Editor::zainium_default()`.
    pub fn zainium_default() -> Self {
        History {
            entries: Vec::new(),
            max_entries: ZAINIUM_HISTORY_MAX,
            cursor: NavigationCursor::AtEnd,
            saved_draft: None,
            dedup: true,
            file_path: Some(zainium_default_history_path()),
            file_max: ZAINIUM_HISTFILE_MAX,
        }
    }

    // ------------------------------------------------------------------ //
    //  Read-only accessors                                                //
    // ------------------------------------------------------------------ //

    /// Number of entries currently in memory.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if the history is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get the entry at `idx` (0 = oldest).
    pub fn get(&self, idx: usize) -> Option<&HistoryEntry> {
        self.entries.get(idx)
    }

    /// Iterate over all entries, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &HistoryEntry> {
        self.entries.iter()
    }

    /// Return all entries as a slice.
    pub fn as_slice(&self) -> &[HistoryEntry] {
        &self.entries
    }

    // ------------------------------------------------------------------ //
    //  Mutating                                                           //
    // ------------------------------------------------------------------ //

    /// Add a line to history.
    ///
    /// Rules (matching GNU readline + bash defaults):
    ///   1. Empty lines are ignored.
    ///   2. If `dedup` is enabled, lines identical to the most recent entry
    ///      are skipped (bash `HISTCONTROL=ignoredups`).
    ///   3. If the history is at capacity, the oldest entry is dropped.
    ///
    /// Mirrors GNU readline's `add_history()`.
    pub fn push(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        // Dedup check.
        if self.dedup {
            if let Some(last) = self.entries.last() {
                if last.line == line {
                    return;
                }
            }
        }
        // Capacity: drop oldest if full.
        if self.entries.len() >= self.max_entries {
            self.entries.remove(0);
        }
        self.entries.push(HistoryEntry::new(line));
    }

    /// Remove the entry at `idx`.
    pub fn remove(&mut self, idx: usize) -> Result<HistoryEntry> {
        if idx >= self.entries.len() {
            return Err(OxidreadError::HistoryOutOfRange {
                index: idx,
                len: self.entries.len(),
            });
        }
        Ok(self.entries.remove(idx))
    }

    /// Clear all in-memory entries and reset navigation.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.cursor = NavigationCursor::AtEnd;
        self.saved_draft = None;
    }

    // ------------------------------------------------------------------ //
    //  Navigation (up / down arrow)                                      //
    // ------------------------------------------------------------------ //

    /// Move to the previous (older) history entry — **Up arrow**.
    ///
    /// `current_line` is the live draft line; it is saved on the first call
    /// so it can be restored when the user presses Down past the newest entry.
    ///
    /// Returns `Some(&HistoryEntry)` with the entry to display, or `None` if
    /// already at the oldest entry (bell signal).
    ///
    /// Mirrors GNU readline's `rl_get_previous_history`.
    pub fn prev(&mut self, current_line: Option<&str>) -> Option<&HistoryEntry> {
        if self.entries.is_empty() {
            return None;
        }
        match &self.cursor {
            NavigationCursor::AtEnd => {
                // Save the draft before we start navigating.
                self.saved_draft = current_line.map(str::to_owned);
                let idx = self.entries.len() - 1;
                self.cursor = NavigationCursor::InHistory { idx };
                self.entries.get(idx)
            }
            NavigationCursor::InHistory { idx } => {
                if *idx == 0 {
                    // Already at oldest — bell (return None).
                    return None;
                }
                let new_idx = idx - 1;
                self.cursor = NavigationCursor::InHistory { idx: new_idx };
                self.entries.get(new_idx)
            }
        }
    }

    /// Move to the next (newer) history entry — **Down arrow**.
    ///
    /// Returns `Some(&HistoryEntry)` for the next entry, or `None` when the
    /// caller should restore the saved draft (we've scrolled past the newest).
    ///
    /// The caller should check `is_at_end()` after calling this: if true,
    /// restore the saved draft via `take_saved_draft()`.
    ///
    /// Mirrors GNU readline's `rl_get_next_history`.
    pub fn next(&mut self) -> Option<&HistoryEntry> {
        match &self.cursor {
            NavigationCursor::AtEnd => None, // already at draft, nothing to do
            NavigationCursor::InHistory { idx } => {
                let next = idx + 1;
                if next >= self.entries.len() {
                    // Scrolled past newest → return to draft.
                    self.cursor = NavigationCursor::AtEnd;
                    None // caller should restore saved_draft
                } else {
                    self.cursor = NavigationCursor::InHistory { idx: next };
                    self.entries.get(next)
                }
            }
        }
    }

    /// `true` if navigation cursor is back at the live draft position.
    pub fn is_at_end(&self) -> bool {
        self.cursor == NavigationCursor::AtEnd
    }

    /// Take the saved draft line (consumes it).
    /// Returns `None` if no draft was saved (history was empty when nav started).
    pub fn take_saved_draft(&mut self) -> Option<String> {
        self.saved_draft.take()
    }

    /// Reset navigation cursor to AtEnd (call after accepting a line).
    pub fn reset_cursor(&mut self) {
        self.cursor = NavigationCursor::AtEnd;
        self.saved_draft = None;
    }

    // ------------------------------------------------------------------ //
    //  Search                                                             //
    // ------------------------------------------------------------------ //

    /// Search backwards from the current cursor position for an entry whose
    /// line contains `needle` as a substring.
    ///
    /// Returns `Some((index, &HistoryEntry))` for the first match found, or
    /// `None` if no match exists.
    ///
    /// Mirrors GNU readline's `history_search` (non-incremental, backward).
    pub fn search_backward(&self, needle: &str) -> Option<(usize, &HistoryEntry)> {
        let start = match &self.cursor {
            NavigationCursor::AtEnd => self.entries.len(),
            NavigationCursor::InHistory { idx } => *idx,
        };
        self.entries[..start]
            .iter()
            .enumerate()
            .rev()
            .find(|(_, e)| e.line.contains(needle))
    }

    /// Search forward from the current cursor position for `needle`.
    pub fn search_forward(&self, needle: &str) -> Option<(usize, &HistoryEntry)> {
        let start = match &self.cursor {
            NavigationCursor::AtEnd => return None,
            NavigationCursor::InHistory { idx } => idx + 1,
        };
        self.entries[start..]
            .iter()
            .enumerate()
            .find(|(_, e)| e.line.contains(needle))
            .map(|(i, e)| (start + i, e))
    }

    /// Search backwards for entries whose line **starts with** `prefix`.
    ///
    /// This is the fast-path used for prefix-search (Up arrow with partial
    /// input already typed), matching bash `history-search-backward`.
    pub fn search_prefix_backward(&self, prefix: &str) -> Option<(usize, &HistoryEntry)> {
        let start = match &self.cursor {
            NavigationCursor::AtEnd => self.entries.len(),
            NavigationCursor::InHistory { idx } => *idx,
        };
        self.entries[..start]
            .iter()
            .enumerate()
            .rev()
            .find(|(_, e)| e.line.starts_with(prefix))
    }

    // ------------------------------------------------------------------ //
    //  File persistence                                                   //
    // ------------------------------------------------------------------ //

    /// Load history from a file, appending to in-memory entries.
    ///
    /// Supports two formats:
    ///   1. Plain: one command per line.
    ///   2. Extended (bash): `# <unix_timestamp>` line followed by the command.
    ///
    /// If the file does not exist, returns `Ok(0)` (not an error).
    ///
    /// Mirrors GNU readline's `read_history()` / `history_truncate_file()`.
    pub fn load_file(&mut self, path: &Path) -> Result<usize> {
        if !path.exists() {
            return Ok(0);
        }
        let file = fs::File::open(path).map_err(|e| OxidreadError::HistoryFile {
            path: path.display().to_string(),
            source: e,
        })?;
        let reader = BufReader::new(file);
        let mut count = 0usize;
        let mut pending_ts: Option<u64> = None;

        for raw_line in reader.lines() {
            let line = raw_line.map_err(|e| OxidreadError::HistoryFile {
                path: path.display().to_string(),
                source: e,
            })?;

            // Bash extended timestamp format: "# <unix_secs>"
            if let Some(ts_str) = line.strip_prefix("# ") {
                if let Ok(ts) = ts_str.trim().parse::<u64>() {
                    pending_ts = Some(ts);
                    continue;
                }
            }

            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                pending_ts = None;
                continue;
            }

            // Dedup check against last loaded entry.
            if self.dedup {
                if let Some(last) = self.entries.last() {
                    if last.line == trimmed {
                        pending_ts = None;
                        continue;
                    }
                }
            }

            // Capacity check.
            if self.entries.len() >= self.max_entries {
                self.entries.remove(0);
            }

            let entry = match pending_ts.take() {
                Some(ts) => HistoryEntry::with_timestamp(trimmed, ts),
                None => HistoryEntry::new(trimmed),
            };
            self.entries.push(entry);
            count += 1;
        }
        Ok(count)
    }

    /// Load from the configured `file_path` (if set).
    /// Convenience wrapper around `load_file`.
    pub fn load(&mut self) -> Result<usize> {
        match self.file_path.clone() {
            Some(path) => self.load_file(&path),
            None => Ok(0),
        }
    }

    /// Save history to `path`, writing at most `max_lines` entries
    /// (the most recent ones).
    ///
    /// Format: plain text, one command per line.
    /// If a timestamp is present it is written as `# <unix_secs>` on the
    /// preceding line (bash-compatible extended format).
    ///
    /// Mirrors GNU readline's `write_history()`.
    pub fn save_file(&self, path: &Path, max_lines: usize) -> Result<()> {
        // Create parent directory if needed.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| OxidreadError::HistoryFile {
                    path: path.display().to_string(),
                    source: e,
                })?;
            }
        }

        let mut file = fs::File::create(path).map_err(|e| OxidreadError::HistoryFile {
            path: path.display().to_string(),
            source: e,
        })?;

        // Write the most recent `max_lines` entries.
        let skip = self.entries.len().saturating_sub(max_lines);
        for entry in &self.entries[skip..] {
            if let Some(ts) = entry.timestamp {
                writeln!(file, "# {}", ts).map_err(|e| OxidreadError::HistoryFile {
                    path: path.display().to_string(),
                    source: e,
                })?;
            }
            writeln!(file, "{}", entry.line).map_err(|e| OxidreadError::HistoryFile {
                path: path.display().to_string(),
                source: e,
            })?;
        }
        Ok(())
    }

    /// Save to the configured `file_path` (if set).
    /// Convenience wrapper around `save_file`.
    pub fn save(&self) -> Result<()> {
        match &self.file_path {
            Some(path) => self.save_file(path, self.file_max),
            None => Ok(()),
        }
    }

    /// Append only the last `n` entries to `path` without rewriting the
    /// whole file. Useful for long-running shells that want incremental saves.
    ///
    /// Mirrors GNU readline's `append_history()`.
    pub fn append_file(&self, path: &Path, n: usize) -> Result<()> {
        if self.entries.is_empty() || n == 0 {
            return Ok(());
        }
        // Create parent dir.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| OxidreadError::HistoryFile {
                    path: path.display().to_string(),
                    source: e,
                })?;
            }
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| OxidreadError::HistoryFile {
                path: path.display().to_string(),
                source: e,
            })?;

        let skip = self.entries.len().saturating_sub(n);
        for entry in &self.entries[skip..] {
            if let Some(ts) = entry.timestamp {
                writeln!(file, "# {}", ts).map_err(|e| OxidreadError::HistoryFile {
                    path: path.display().to_string(),
                    source: e,
                })?;
            }
            writeln!(file, "{}", entry.line).map_err(|e| OxidreadError::HistoryFile {
                path: path.display().to_string(),
                source: e,
            })?;
        }
        Ok(())
    }
}

// -----------------------------------------------------------------------
// Unit tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // -- zainium branding --

    #[test]
    fn zainium_identity_contains_author() {
        assert!(OXIDREAD_IDENTITY.contains("Ali Zain"));
        assert!(OXIDREAD_IDENTITY.contains("alizain.x404@gmail.com"));
        assert!(OXIDREAD_IDENTITY.contains("Zainium OS"));
        assert!(OXIDREAD_IDENTITY.contains("GPL-3.0"));
    }

    #[test]
    fn zainium_info_returns_identity() {
        assert_eq!(zainium_info(), OXIDREAD_IDENTITY);
    }

    #[test]
    fn zainium_history_path_ends_with_filename() {
        let p = zainium_default_history_path();
        assert_eq!(p.file_name().unwrap(), ZAINIUM_HISTORY_FILENAME);
    }

    #[test]
    fn zainium_default_constructor_sets_file_path() {
        let h = History::zainium_default();
        assert!(h.file_path.is_some());
        assert_eq!(
            h.file_path.unwrap().file_name().unwrap(),
            ZAINIUM_HISTORY_FILENAME
        );
        assert_eq!(h.max_entries, ZAINIUM_HISTORY_MAX);
    }

    // -- push / dedup --

    #[test]
    fn push_adds_entries() {
        let mut h = History::new(100);
        h.push("ls");
        h.push("pwd");
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn push_ignores_empty_lines() {
        let mut h = History::new(100);
        h.push("");
        h.push("   \n");
        assert_eq!(h.len(), 0);
    }

    #[test]
    fn push_dedup_skips_consecutive_duplicate() {
        let mut h = History::new(100);
        h.push("ls");
        h.push("ls");
        h.push("ls");
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn push_dedup_allows_non_consecutive_duplicate() {
        let mut h = History::new(100);
        h.push("ls");
        h.push("pwd");
        h.push("ls");
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn push_evicts_oldest_when_full() {
        let mut h = History::new(3);
        h.push("a");
        h.push("b");
        h.push("c");
        h.push("d");
        assert_eq!(h.len(), 3);
        assert_eq!(h.get(0).unwrap().line, "b");
    }

    // -- navigation --

    #[test]
    fn prev_returns_newest_first() {
        let mut h = History::new(100);
        h.push("first");
        h.push("second");
        h.push("third");

        let e = h.prev(Some("draft")).unwrap();
        assert_eq!(e.line, "third");
        let e = h.prev(None).unwrap();
        assert_eq!(e.line, "second");
        let e = h.prev(None).unwrap();
        assert_eq!(e.line, "first");
        // At oldest — should return None (bell).
        assert!(h.prev(None).is_none());
    }

    #[test]
    fn next_returns_newer_entries() {
        let mut h = History::new(100);
        h.push("one");
        h.push("two");
        h.push("three");

        // Go back two.
        h.prev(Some("live"));
        h.prev(None);

        // Now go forward.
        let e = h.next().unwrap();
        assert_eq!(e.line, "three");

        // One more → past end, returns None, cursor resets to AtEnd.
        assert!(h.next().is_none());
        assert!(h.is_at_end());
    }

    #[test]
    fn saved_draft_is_restored_after_navigation() {
        let mut h = History::new(100);
        h.push("cmd1");
        h.push("cmd2");

        h.prev(Some("my draft"));
        // Navigate back to end.
        h.next(); // cmd2 → goes back AtEnd
        // next() returned None → restore draft.
        let draft = h.take_saved_draft();
        assert_eq!(draft.as_deref(), Some("my draft"));
    }

    #[test]
    fn reset_cursor_clears_navigation() {
        let mut h = History::new(100);
        h.push("x");
        h.prev(Some("draft"));
        h.reset_cursor();
        assert!(h.is_at_end());
        assert!(h.take_saved_draft().is_none());
    }

    // -- search --

    #[test]
    fn search_backward_finds_match() {
        let mut h = History::new(100);
        h.push("cargo build");
        h.push("cargo test");
        h.push("git status");

        let result = h.search_backward("cargo");
        assert!(result.is_some());
        let (_, entry) = result.unwrap();
        assert_eq!(entry.line, "cargo test");
    }

    #[test]
    fn search_backward_returns_none_when_no_match() {
        let mut h = History::new(100);
        h.push("ls");
        h.push("pwd");
        assert!(h.search_backward("zzznomatch").is_none());
    }

    #[test]
    fn search_prefix_backward_matches_prefix_only() {
        let mut h = History::new(100);
        h.push("cargo build");
        h.push("cargo test");
        h.push("cat README.md");

        let result = h.search_prefix_backward("cargo");
        assert!(result.is_some());
        let (_, entry) = result.unwrap();
        assert_eq!(entry.line, "cargo test"); // most recent match
    }

    // -- file persistence --

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path();

        let mut h = History::new(100);
        h.push("echo hello");
        h.push("ls -la");
        h.push("cargo build --release");
        h.save_file(path, 100).unwrap();

        let mut h2 = History::new(100);
        let count = h2.load_file(path).unwrap();
        assert_eq!(count, 3);
        assert_eq!(h2.get(0).unwrap().line, "echo hello");
        assert_eq!(h2.get(2).unwrap().line, "cargo build --release");
    }

    #[test]
    fn save_respects_max_lines() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path();

        let mut h = History::new(100);
        for i in 0..10 {
            h.push(&format!("cmd {}", i));
        }
        h.save_file(path, 3).unwrap();

        let mut h2 = History::new(100);
        h2.load_file(path).unwrap();
        assert_eq!(h2.len(), 3);
        // Should have saved the most recent 3.
        assert_eq!(h2.get(0).unwrap().line, "cmd 7");
        assert_eq!(h2.get(2).unwrap().line, "cmd 9");
    }

    #[test]
    fn load_nonexistent_file_returns_zero() {
        let mut h = History::new(100);
        let count = h.load_file(Path::new("/tmp/__no_such_file_oxidread__")).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn load_plain_text_format() {
        let mut tmp = NamedTempFile::new().unwrap();
        writeln!(tmp, "echo zainium").unwrap();
        writeln!(tmp, "uname -r").unwrap();
        writeln!(tmp, "zx install code").unwrap();

        let mut h = History::new(100);
        let count = h.load_file(tmp.path()).unwrap();
        assert_eq!(count, 3);
        assert_eq!(h.get(1).unwrap().line, "uname -r");
    }

    #[test]
    fn load_extended_bash_timestamp_format() {
        let mut tmp = NamedTempFile::new().unwrap();
        writeln!(tmp, "# 1700000000").unwrap();
        writeln!(tmp, "echo hello").unwrap();
        writeln!(tmp, "# 1700000060").unwrap();
        writeln!(tmp, "ls -la").unwrap();

        let mut h = History::new(100);
        h.load_file(tmp.path()).unwrap();
        assert_eq!(h.get(0).unwrap().timestamp, Some(1_700_000_000));
        assert_eq!(h.get(1).unwrap().timestamp, Some(1_700_000_060));
        assert_eq!(h.get(0).unwrap().line, "echo hello");
    }

    #[test]
    fn append_file_adds_to_existing() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path();

        let mut h = History::new(100);
        h.push("first");
        h.push("second");
        h.save_file(path, 100).unwrap();

        h.push("third");
        h.append_file(path, 1).unwrap();

        let mut h2 = History::new(100);
        h2.load_file(path).unwrap();
        assert_eq!(h2.len(), 3);
        assert_eq!(h2.get(2).unwrap().line, "third");
    }

    #[test]
    fn clear_resets_all_state() {
        let mut h = History::new(100);
        h.push("cmd");
        h.prev(Some("draft"));
        h.clear();
        assert!(h.is_empty());
        assert!(h.is_at_end());
    }
}

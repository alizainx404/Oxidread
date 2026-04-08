// oxidread/src/readline/line_buffer.rs
//
// Copyright (C) 2025 Ali Zain <alizain.x404@gmail.com>
// Part of the Oxidread project — a pure Rust, memory-safe rewrite of
// GNU Readline and ncurses for Zainium OS and all compatible Linux distributions.
//
// Oxidread is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.
//
// -----------------------------------------------------------------------
// line_buffer.rs — Unicode-aware line editing buffer.
//
// GNU readline design (from text.c / rlprivate.h) that we improve upon:
//
//   • GNU uses a flat `char* rl_line_buffer` with byte-indexed `rl_point`
//     and `rl_end`. Multibyte characters are handled with MB_NEXTCHAR /
//     MB_PREVCHAR macros and _rl_find_next_mbchar() which walk the raw bytes.
//     This is error-prone: a bad `rl_point` can land mid-codepoint.
//
//   • We replace this with a `String` (guaranteed valid UTF-8) plus a
//     cursor measured in *grapheme cluster* units, not bytes. The cursor
//     can never land inside a multi-byte codepoint or inside a combining
//     character sequence. This is the correct mental model for what a
//     terminal user experiences as "one character".
//
//   • The undo stack (GNU's `UNDO_LIST` linked list) becomes a `Vec<UndoEntry>`
//     with group nesting tracked by a counter, matching GNU's
//     `rl_begin_undo_group` / `rl_end_undo_group` semantics.
//
//   • GNU's mark (`rl_mark`) is included for region/kill support.
//
//   • Kill ring is managed by the Editor, not this struct, to keep concerns
//     separated.
// -----------------------------------------------------------------------

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::readline::error::{BoundarySide, OxidreadError, Result};

// -----------------------------------------------------------------------
// Undo subsystem
// -----------------------------------------------------------------------

/// A single undoable editing operation.
///
/// Mirrors GNU readline's `UNDO_LIST` entries (UNDO_INSERT / UNDO_DELETE)
/// but expressed as a typed Rust enum instead of an int discriminant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoEntry {
    /// Text was inserted at `grapheme_pos`. To undo: delete `text.len()` graphemes
    /// starting at `grapheme_pos`.
    Insert {
        grapheme_pos: usize,
        text: String,
    },
    /// Text was deleted from `grapheme_pos`. To undo: re-insert `text` at
    /// `grapheme_pos`.
    Delete {
        grapheme_pos: usize,
        text: String,
    },
    /// A group boundary marker. Groups are delimited by matching Begin/End pairs.
    /// GNU readline calls these `rl_begin_undo_group` / `rl_end_undo_group`.
    GroupBegin,
    GroupEnd,
}

/// The undo stack — a `Vec` of `UndoEntry` values with group nesting support.
///
/// Push entries with `push()`. Call `undo()` to replay entries in reverse.
/// Group nesting is validated: `end_group()` without a matching `begin_group()`
/// returns an error.
#[derive(Debug, Default, Clone)]
pub struct UndoStack {
    entries: Vec<UndoEntry>,
    /// Depth counter for open groups (>0 means we're inside a group).
    group_depth: usize,
}

impl UndoStack {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a single undo entry.
    pub fn push(&mut self, entry: UndoEntry) {
        // Coalesce adjacent single-char inserts into one entry (same as GNU
        // readline coalescing logic in rl_insert_text when end - start < 20).
        // Only coalesce when not inside a group — group entries must stay discrete.
        if self.group_depth == 0 {
            if let UndoEntry::Insert {
                grapheme_pos: new_pos,
                text: new_text,
            } = &entry
            {
                if let Some(UndoEntry::Insert {
                    grapheme_pos: prev_pos,
                    text: prev_text,
                }) = self.entries.last_mut()
                {
                    let prev_grapheme_count = prev_text.graphemes(true).count();
                    if *new_pos == *prev_pos + prev_grapheme_count
                        && new_text.graphemes(true).count() == 1
                        && prev_grapheme_count < 20
                    {
                        prev_text.push_str(new_text);
                        return;
                    }
                }
            }
        }
        self.entries.push(entry);
    }

    /// Begin an undo group. All entries pushed until `end_group()` will be
    /// rolled back as a single atomic undo operation.
    pub fn begin_group(&mut self) {
        self.entries.push(UndoEntry::GroupBegin);
        self.group_depth += 1;
    }

    /// End an undo group.
    pub fn end_group(&mut self) -> Result<()> {
        if self.group_depth == 0 {
            return Err(OxidreadError::UndoGroupMismatch);
        }
        self.entries.push(UndoEntry::GroupEnd);
        self.group_depth -= 1;
        Ok(())
    }

    /// Returns `true` if the stack has any entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drain the undo stack and return the entries in reverse (ready to apply).
    /// If the top of the stack is inside a group, the entire group is returned.
    /// Returns `Err(UndoStackEmpty)` if nothing to undo.
    pub fn pop_undo_set(&mut self) -> Result<Vec<UndoEntry>> {
        if self.entries.is_empty() {
            return Err(OxidreadError::UndoStackEmpty);
        }

        let mut result = Vec::new();

        match self.entries.last() {
            None => return Err(OxidreadError::UndoStackEmpty),
            Some(UndoEntry::GroupEnd) => {
                // Pop everything back to the matching GroupBegin.
                // We collect entries in stack-pop order (newest first), then
                // reverse so that undo() can iterate front-to-back and replay
                // them in the correct oldest-first order.
                self.entries.pop(); // remove GroupEnd
                let mut depth = 1usize;
                loop {
                    match self.entries.pop() {
                        None => break, // malformed, but don't panic
                        Some(UndoEntry::GroupBegin) => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        Some(UndoEntry::GroupEnd) => {
                            depth += 1;
                        }
                        Some(entry) => result.push(entry),
                    }
                }
                // Reverse so oldest entry comes first — undo() will then
                // iterate front-to-back which is newest-first after the rev()
                // call inside undo(). Net effect: correct chronological undo.
                result.reverse();
            }
            Some(_) => {
                result.push(self.entries.pop().unwrap());
            }
        }

        Ok(result)
    }

    /// Clear the entire undo stack.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.group_depth = 0;
    }
}

// -----------------------------------------------------------------------
// LineBuffer
// -----------------------------------------------------------------------

/// A Unicode-aware, mutable line editing buffer.
///
/// # Coordinate system
///
/// All positions exposed in the public API are measured in **grapheme cluster
/// indices** — what a terminal user perceives as "one character position".
/// Internally we store a `String` (byte buffer) and translate on demand using
/// `unicode-segmentation`. This is O(n) in the grapheme count, which is fine
/// for interactive line lengths (usually < 1 000 graphemes).
///
/// The byte-level API (`as_str`, `len_bytes`) is also exposed so the display
/// layer can write raw bytes to the terminal without re-encoding.
///
/// # Mark
///
/// An optional mark position (GNU `rl_mark`) is stored for region-based
/// operations (kill-region, etc.). `None` means no active mark.
#[derive(Debug, Clone)]
pub struct LineBuffer {
    /// The line content. Always valid UTF-8.
    buf: String,

    /// Cursor position measured in grapheme cluster units.
    /// Invariant: `cursor <= grapheme_count()`.
    cursor: usize,

    /// Optional mark position in grapheme cluster units.
    /// GNU readline's `rl_mark`.
    mark: Option<usize>,

    /// Undo stack for this buffer.
    undo: UndoStack,
}

impl Default for LineBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl LineBuffer {
    // ------------------------------------------------------------------ //
    //  Construction                                                       //
    // ------------------------------------------------------------------ //

    /// Create an empty `LineBuffer` with cursor at position 0.
    pub fn new() -> Self {
        LineBuffer {
            buf: String::new(),
            cursor: 0,
            mark: None,
            undo: UndoStack::new(),
        }
    }

    /// Create a `LineBuffer` pre-filled with `text`, cursor at end.
    pub fn from_str(text: &str) -> Self {
        let count = text.graphemes(true).count();
        LineBuffer {
            buf: text.to_owned(),
            cursor: count,
            mark: None,
            undo: UndoStack::new(),
        }
    }

    // ------------------------------------------------------------------ //
    //  Read-only accessors                                                //
    // ------------------------------------------------------------------ //

    /// The full line content as a `&str`.
    pub fn as_str(&self) -> &str {
        &self.buf
    }

    /// Number of grapheme clusters in the buffer (what the user sees as length).
    pub fn grapheme_count(&self) -> usize {
        self.buf.graphemes(true).count()
    }

    /// Number of bytes in the buffer.
    pub fn len_bytes(&self) -> usize {
        self.buf.len()
    }

    /// `true` if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Current cursor position in grapheme cluster units.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Current mark position (if set).
    pub fn mark(&self) -> Option<usize> {
        self.mark
    }

    /// `true` if cursor is at the beginning of the line.
    pub fn at_start(&self) -> bool {
        self.cursor == 0
    }

    /// `true` if cursor is at the end of the line.
    pub fn at_end(&self) -> bool {
        self.cursor == self.grapheme_count()
    }

    /// The display width (in terminal columns) of the entire buffer.
    /// Wide characters (CJK, emoji) count as 2.
    pub fn display_width(&self) -> usize {
        UnicodeWidthStr::width(self.buf.as_str())
    }

    /// The display width of the buffer up to (but not including) the cursor.
    pub fn cursor_display_width(&self) -> usize {
        let prefix = self.grapheme_slice(0, self.cursor);
        UnicodeWidthStr::width(prefix.as_str())
    }

    // ------------------------------------------------------------------ //
    //  Internal helpers                                                   //
    // ------------------------------------------------------------------ //

    /// Convert a grapheme index to a byte offset in `self.buf`.
    /// Returns `buf.len()` if `grapheme_idx == grapheme_count()`.
    /// Returns `Err` if `grapheme_idx > grapheme_count()`.
    fn grapheme_to_byte_offset(&self, grapheme_idx: usize) -> Result<usize> {
        let count = self.grapheme_count();
        if grapheme_idx > count {
            return Err(OxidreadError::GraphemeOutOfRange {
                index: grapheme_idx,
                count,
            });
        }
        // Walk grapheme boundaries.
        let offset = self
            .buf
            .grapheme_indices(true)
            .nth(grapheme_idx)
            .map(|(byte_off, _)| byte_off)
            .unwrap_or(self.buf.len()); // grapheme_idx == count → end of string
        Ok(offset)
    }

    /// Return a `String` containing graphemes `[start, end)`.
    fn grapheme_slice(&self, start: usize, end: usize) -> String {
        self.buf
            .graphemes(true)
            .skip(start)
            .take(end.saturating_sub(start))
            .collect()
    }

    // ------------------------------------------------------------------ //
    //  Cursor movement                                                    //
    // ------------------------------------------------------------------ //

    /// Move cursor forward by `n` graphemes.
    /// Returns `Err(CursorAtBoundary::End)` if already at end (bell signal).
    pub fn move_forward(&mut self, n: usize) -> Result<()> {
        let count = self.grapheme_count();
        if self.cursor == count {
            return Err(OxidreadError::CursorAtBoundary {
                side: BoundarySide::End,
            });
        }
        self.cursor = (self.cursor + n).min(count);
        Ok(())
    }

    /// Move cursor backward by `n` graphemes.
    /// Returns `Err(CursorAtBoundary::Start)` if already at start.
    pub fn move_backward(&mut self, n: usize) -> Result<()> {
        if self.cursor == 0 {
            return Err(OxidreadError::CursorAtBoundary {
                side: BoundarySide::Start,
            });
        }
        self.cursor = self.cursor.saturating_sub(n);
        Ok(())
    }

    /// Move cursor to the beginning of the line (GNU `rl_beg_of_line`).
    pub fn move_to_start(&mut self) {
        self.cursor = 0;
    }

    /// Move cursor to the end of the line (GNU `rl_end_of_line`).
    pub fn move_to_end(&mut self) {
        self.cursor = self.grapheme_count();
    }

    /// Move cursor forward by one word (Emacs style — skip non-alpha, then alpha).
    /// Mirrors GNU readline's `rl_forward_word`.
    pub fn move_forward_word(&mut self) {
        let graphemes: Vec<&str> = self.buf.graphemes(true).collect();
        let count = graphemes.len();
        let mut pos = self.cursor;

        // Skip non-word characters.
        while pos < count && !is_word_char(graphemes[pos]) {
            pos += 1;
        }
        // Skip word characters.
        while pos < count && is_word_char(graphemes[pos]) {
            pos += 1;
        }
        self.cursor = pos;
    }

    /// Move cursor backward by one word (Emacs style).
    /// Mirrors GNU readline's `rl_backward_word`.
    pub fn move_backward_word(&mut self) {
        let graphemes: Vec<&str> = self.buf.graphemes(true).collect();
        let mut pos = self.cursor;

        if pos == 0 {
            return;
        }
        // Step back one to look at the char before cursor.
        pos -= 1;

        // Skip non-word characters.
        while pos > 0 && !is_word_char(graphemes[pos]) {
            pos -= 1;
        }
        // Skip word characters.
        while pos > 0 && is_word_char(graphemes[pos - 1]) {
            pos -= 1;
        }
        self.cursor = pos;
    }

    /// Set cursor to an absolute grapheme position.
    pub fn set_cursor(&mut self, pos: usize) -> Result<()> {
        let count = self.grapheme_count();
        if pos > count {
            return Err(OxidreadError::CursorOutOfBounds { pos, len: count });
        }
        self.cursor = pos;
        Ok(())
    }

    // ------------------------------------------------------------------ //
    //  Mark                                                               //
    // ------------------------------------------------------------------ //

    /// Set the mark at the current cursor position.
    pub fn set_mark(&mut self) {
        self.mark = Some(self.cursor);
    }

    /// Set the mark at an explicit grapheme position.
    pub fn set_mark_at(&mut self, pos: usize) -> Result<()> {
        let count = self.grapheme_count();
        if pos > count {
            return Err(OxidreadError::CursorOutOfBounds { pos, len: count });
        }
        self.mark = Some(pos);
        Ok(())
    }

    /// Clear the mark.
    pub fn clear_mark(&mut self) {
        self.mark = None;
    }

    /// Exchange cursor and mark positions (GNU `rl_exchange_point_and_mark`).
    pub fn exchange_point_and_mark(&mut self) -> Result<()> {
        let mark = self.mark.ok_or(OxidreadError::CursorAtBoundary {
            side: BoundarySide::Start, // no mark — treat as benign boundary
        })?;
        let old_cursor = self.cursor;
        self.cursor = mark;
        self.mark = Some(old_cursor);
        Ok(())
    }

    // ------------------------------------------------------------------ //
    //  Insert / Delete                                                    //
    // ------------------------------------------------------------------ //

    /// Insert `text` at the current cursor position and advance cursor.
    /// Pushes an `UndoEntry::Insert` unless `suppress_undo` is true.
    ///
    /// Mirrors GNU readline's `rl_insert_text`.
    pub fn insert(&mut self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let byte_off = self.grapheme_to_byte_offset(self.cursor)?;
        self.buf.insert_str(byte_off, text);

        let grapheme_count = text.graphemes(true).count();

        // Record undo entry.
        self.undo.push(UndoEntry::Insert {
            grapheme_pos: self.cursor,
            text: text.to_owned(),
        });

        self.cursor += grapheme_count;
        Ok(())
    }

    /// Delete `n` graphemes starting at `pos` (grapheme index).
    /// Returns the deleted text so callers can push it to the kill ring.
    ///
    /// Mirrors GNU readline's `rl_delete_text(from, to)`.
    pub fn delete_range(&mut self, pos: usize, n: usize) -> Result<String> {
        let count = self.grapheme_count();
        if pos > count {
            return Err(OxidreadError::GraphemeOutOfRange { index: pos, count });
        }
        let n = n.min(count - pos); // clamp so we never go past end
        if n == 0 {
            return Ok(String::new());
        }

        let byte_start = self.grapheme_to_byte_offset(pos)?;
        let byte_end = self.grapheme_to_byte_offset(pos + n)?;

        let deleted: String = self.buf[byte_start..byte_end].to_owned();
        self.buf.replace_range(byte_start..byte_end, "");

        // Record undo entry.
        self.undo.push(UndoEntry::Delete {
            grapheme_pos: pos,
            text: deleted.clone(),
        });

        // Adjust cursor if it falls inside or after the deleted range.
        if self.cursor > pos + n {
            self.cursor -= n;
        } else if self.cursor > pos {
            self.cursor = pos;
        }

        // Adjust mark similarly.
        if let Some(m) = self.mark {
            if m > pos + n {
                self.mark = Some(m - n);
            } else if m > pos {
                self.mark = Some(pos);
            }
        }

        Ok(deleted)
    }

    /// Delete the grapheme immediately before the cursor (Backspace).
    /// Returns the deleted text or `Err(CursorAtBoundary::Start)`.
    ///
    /// Mirrors GNU readline's `_rl_rubout_char`.
    pub fn backspace(&mut self) -> Result<String> {
        if self.cursor == 0 {
            return Err(OxidreadError::CursorAtBoundary {
                side: BoundarySide::Start,
            });
        }
        self.delete_range(self.cursor - 1, 1)
    }

    /// Delete the grapheme at the cursor (Delete key / Ctrl-D).
    /// Returns the deleted text or `Err(CursorAtBoundary::End)`.
    ///
    /// Mirrors GNU readline's `rl_delete`.
    pub fn delete_forward(&mut self) -> Result<String> {
        if self.at_end() {
            return Err(OxidreadError::CursorAtBoundary {
                side: BoundarySide::End,
            });
        }
        self.delete_range(self.cursor, 1)
    }

    /// Kill from cursor to end of line (Ctrl-K).
    /// Returns the killed text; buffer is truncated at cursor.
    pub fn kill_to_end(&mut self) -> Result<String> {
        let n = self.grapheme_count() - self.cursor;
        if n == 0 {
            return Err(OxidreadError::CursorAtBoundary {
                side: BoundarySide::End,
            });
        }
        self.delete_range(self.cursor, n)
    }

    /// Kill from beginning of line to cursor (Ctrl-U).
    /// Returns the killed text.
    pub fn kill_to_start(&mut self) -> Result<String> {
        if self.cursor == 0 {
            return Err(OxidreadError::CursorAtBoundary {
                side: BoundarySide::Start,
            });
        }
        let n = self.cursor;
        self.delete_range(0, n)
    }

    /// Kill the word before the cursor (Ctrl-W — Unix word rubout).
    /// Stops at whitespace boundary. Returns the killed text.
    pub fn kill_word_backward(&mut self) -> Result<String> {
        if self.cursor == 0 {
            return Err(OxidreadError::CursorAtBoundary {
                side: BoundarySide::Start,
            });
        }
        let graphemes: Vec<&str> = self.buf.graphemes(true).collect();
        let mut pos = self.cursor;

        // Skip trailing whitespace.
        while pos > 0 && graphemes[pos - 1].chars().all(|c| c.is_whitespace()) {
            pos -= 1;
        }
        // Skip the word.
        while pos > 0 && !graphemes[pos - 1].chars().all(|c| c.is_whitespace()) {
            pos -= 1;
        }

        let n = self.cursor - pos;
        self.delete_range(pos, n)
    }

    /// Kill the word after the cursor (Alt-D — kill word forward).
    pub fn kill_word_forward(&mut self) -> Result<String> {
        if self.at_end() {
            return Err(OxidreadError::CursorAtBoundary {
                side: BoundarySide::End,
            });
        }
        let graphemes: Vec<&str> = self.buf.graphemes(true).collect();
        let count = graphemes.len();
        let mut pos = self.cursor;

        // Skip leading whitespace.
        while pos < count && graphemes[pos].chars().all(|c| c.is_whitespace()) {
            pos += 1;
        }
        // Skip the word.
        while pos < count && !graphemes[pos].chars().all(|c| c.is_whitespace()) {
            pos += 1;
        }

        let n = pos - self.cursor;
        self.delete_range(self.cursor, n)
    }

    /// Replace the entire line content without recording an undo entry.
    /// Used when navigating history. Cursor moves to end.
    ///
    /// Mirrors GNU readline's `rl_replace_line(text, clear_undo)`.
    pub fn replace_line(&mut self, text: &str, clear_undo: bool) {
        self.buf = text.to_owned();
        self.cursor = self.buf.graphemes(true).count();
        self.mark = None;
        if clear_undo {
            self.undo.clear();
        }
    }

    /// Transpose the grapheme at the cursor with the one before it (Ctrl-T).
    /// Mirrors GNU readline's `rl_transpose_chars`.
    pub fn transpose_chars(&mut self) -> Result<()> {
        let count = self.grapheme_count();
        if count < 2 {
            return Err(OxidreadError::CursorAtBoundary {
                side: BoundarySide::Start,
            });
        }

        // At end of line, transpose the last two chars (GNU behaviour).
        let pos = if self.cursor == count {
            count - 1
        } else if self.cursor == 0 {
            return Err(OxidreadError::CursorAtBoundary {
                side: BoundarySide::Start,
            });
        } else {
            self.cursor
        };

        // pos is the second char; pos-1 is the first.
        let graphemes: Vec<&str> = self.buf.graphemes(true).collect();
        let a = graphemes[pos - 1].to_owned();
        let b = graphemes[pos].to_owned();

        // Delete both and re-insert in swapped order.
        // We do this via direct buffer manipulation to keep undo clean.
        self.undo.begin_group();
        let _ = self.delete_range(pos - 1, 2);
        let swapped = format!("{}{}", b, a);
        self.insert(&swapped)?;
        self.undo.end_group()?;

        // Advance cursor past the transposed pair if not at end.
        if self.cursor < count {
            self.cursor = (pos + 1).min(count);
        }

        Ok(())
    }

    // ------------------------------------------------------------------ //
    //  Undo / redo                                                        //
    // ------------------------------------------------------------------ //

    /// Apply one undo operation (or one group). Mutates the buffer.
    ///
    /// Mirrors GNU readline's `rl_do_undo`.
    pub fn undo(&mut self) -> Result<()> {
        let entries = self.undo.pop_undo_set()?;
        // Apply in reverse order. After each mutation the buffer length changes,
        // so we must recompute byte offsets fresh each time — never cache them
        // across iterations.
        for entry in entries.into_iter().rev() {
            match entry {
                UndoEntry::Insert { grapheme_pos, text } => {
                    // Undo an insert → delete the inserted text.
                    // Re-derive byte offsets from the current (post-mutation) buf.
                    let n = text.graphemes(true).count();
                    let byte_start = self
                        .buf
                        .grapheme_indices(true)
                        .nth(grapheme_pos)
                        .map(|(off, _)| off)
                        .unwrap_or(self.buf.len());
                    let byte_end = self
                        .buf
                        .grapheme_indices(true)
                        .nth(grapheme_pos + n)
                        .map(|(off, _)| off)
                        .unwrap_or(self.buf.len());
                    self.buf.replace_range(byte_start..byte_end, "");
                    self.cursor = grapheme_pos;
                }
                UndoEntry::Delete { grapheme_pos, text } => {
                    // Undo a delete → re-insert the deleted text.
                    let byte_off = self
                        .buf
                        .grapheme_indices(true)
                        .nth(grapheme_pos)
                        .map(|(off, _)| off)
                        .unwrap_or(self.buf.len());
                    self.buf.insert_str(byte_off, &text);
                    self.cursor = grapheme_pos + text.graphemes(true).count();
                }
                UndoEntry::GroupBegin | UndoEntry::GroupEnd => {
                    // Group markers are structural, not actionable.
                }
            }
        }
        Ok(())
    }

    /// Begin an undo group (all edits until `end_undo_group` are one undo unit).
    pub fn begin_undo_group(&mut self) {
        self.undo.begin_group();
    }

    /// End an undo group.
    pub fn end_undo_group(&mut self) -> Result<()> {
        self.undo.end_group()
    }

    // ------------------------------------------------------------------ //
    //  Clear / reset                                                      //
    // ------------------------------------------------------------------ //

    /// Clear the buffer, cursor, mark, and undo stack.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
        self.mark = None;
        self.undo.clear();
    }
}

// -----------------------------------------------------------------------
// Helper: word character test (Emacs-style — alphanumeric or underscore)
// -----------------------------------------------------------------------

/// Returns `true` if the given grapheme cluster is a "word character"
/// for purposes of word-movement commands.
///
/// Mirrors GNU readline's `_rl_walphabetic` macro.
fn is_word_char(g: &str) -> bool {
    g.chars()
        .next()
        .map(|c| c.is_alphanumeric() || c == '_')
        .unwrap_or(false)
}

// -----------------------------------------------------------------------
// Unit tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- construction --

    #[test]
    fn new_buffer_is_empty() {
        let buf = LineBuffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.cursor(), 0);
        assert_eq!(buf.grapheme_count(), 0);
    }

    #[test]
    fn from_str_sets_cursor_at_end() {
        let buf = LineBuffer::from_str("hello");
        assert_eq!(buf.grapheme_count(), 5);
        assert_eq!(buf.cursor(), 5);
        assert!(buf.at_end());
    }

    // -- insert --

    #[test]
    fn insert_ascii() {
        let mut buf = LineBuffer::new();
        buf.insert("hello").unwrap();
        assert_eq!(buf.as_str(), "hello");
        assert_eq!(buf.cursor(), 5);
    }

    #[test]
    fn insert_unicode_multibyte() {
        let mut buf = LineBuffer::new();
        buf.insert("こんにちは").unwrap();
        assert_eq!(buf.grapheme_count(), 5);
        assert_eq!(buf.cursor(), 5);
    }

    #[test]
    fn insert_emoji_grapheme_cluster() {
        let mut buf = LineBuffer::new();
        // 👨‍👩‍👧 is a ZWJ sequence — 1 grapheme cluster, multiple codepoints.
        buf.insert("👨‍👩‍👧").unwrap();
        assert_eq!(buf.grapheme_count(), 1);
        assert_eq!(buf.cursor(), 1);
    }

    #[test]
    fn insert_at_middle() {
        let mut buf = LineBuffer::from_str("helo");
        buf.set_cursor(3).unwrap(); // before 'o'
        buf.insert("l").unwrap();
        assert_eq!(buf.as_str(), "hello");
        assert_eq!(buf.cursor(), 4);
    }

    // -- backspace / delete --

    #[test]
    fn backspace_removes_before_cursor() {
        let mut buf = LineBuffer::from_str("hello");
        let deleted = buf.backspace().unwrap();
        assert_eq!(deleted, "o");
        assert_eq!(buf.as_str(), "hell");
        assert_eq!(buf.cursor(), 4);
    }

    #[test]
    fn backspace_at_start_returns_boundary_error() {
        let mut buf = LineBuffer::new();
        let err = buf.backspace().unwrap_err();
        assert!(matches!(
            err,
            OxidreadError::CursorAtBoundary {
                side: BoundarySide::Start
            }
        ));
    }

    #[test]
    fn delete_forward_removes_at_cursor() {
        let mut buf = LineBuffer::from_str("hello");
        buf.set_cursor(1).unwrap();
        let deleted = buf.delete_forward().unwrap();
        assert_eq!(deleted, "e");
        assert_eq!(buf.as_str(), "hllo");
        assert_eq!(buf.cursor(), 1);
    }

    #[test]
    fn backspace_unicode_grapheme() {
        let mut buf = LineBuffer::from_str("hi👨‍👩‍👧");
        // cursor is at end (3 graphemes)
        let deleted = buf.backspace().unwrap();
        // entire ZWJ cluster should be deleted
        assert_eq!(deleted, "👨‍👩‍👧");
        assert_eq!(buf.as_str(), "hi");
        assert_eq!(buf.cursor(), 2);
    }

    // -- cursor movement --

    #[test]
    fn move_to_start_and_end() {
        let mut buf = LineBuffer::from_str("hello");
        buf.move_to_start();
        assert_eq!(buf.cursor(), 0);
        buf.move_to_end();
        assert_eq!(buf.cursor(), 5);
    }

    #[test]
    fn move_forward_word() {
        let mut buf = LineBuffer::from_str("hello world");
        buf.move_to_start();
        buf.move_forward_word();
        assert_eq!(buf.cursor(), 5); // after "hello"
        buf.move_forward_word();
        assert_eq!(buf.cursor(), 11); // after "world"
    }

    #[test]
    fn move_backward_word() {
        let mut buf = LineBuffer::from_str("hello world");
        // cursor at end
        buf.move_backward_word();
        assert_eq!(buf.cursor(), 6); // beginning of "world"
        buf.move_backward_word();
        assert_eq!(buf.cursor(), 0); // beginning of "hello"
    }

    // -- kill commands --

    #[test]
    fn kill_to_end() {
        let mut buf = LineBuffer::from_str("hello world");
        buf.set_cursor(5).unwrap();
        let killed = buf.kill_to_end().unwrap();
        assert_eq!(killed, " world");
        assert_eq!(buf.as_str(), "hello");
    }

    #[test]
    fn kill_to_start() {
        let mut buf = LineBuffer::from_str("hello world");
        buf.set_cursor(5).unwrap();
        let killed = buf.kill_to_start().unwrap();
        assert_eq!(killed, "hello");
        assert_eq!(buf.as_str(), " world");
        assert_eq!(buf.cursor(), 0);
    }

    #[test]
    fn kill_word_backward() {
        let mut buf = LineBuffer::from_str("hello world");
        // cursor at end
        let killed = buf.kill_word_backward().unwrap();
        assert_eq!(killed, "world");
        assert_eq!(buf.as_str(), "hello ");
    }

    // -- transpose --

    #[test]
    fn transpose_chars_at_middle() {
        let mut buf = LineBuffer::from_str("ab");
        buf.set_cursor(1).unwrap();
        buf.transpose_chars().unwrap();
        assert_eq!(buf.as_str(), "ba");
    }

    #[test]
    fn transpose_chars_at_end() {
        let mut buf = LineBuffer::from_str("abc");
        // cursor at end → transpose last two
        buf.transpose_chars().unwrap();
        assert_eq!(buf.as_str(), "acb");
    }

    // -- undo --

    #[test]
    fn undo_single_insert() {
        let mut buf = LineBuffer::new();
        buf.insert("hello").unwrap();
        assert_eq!(buf.as_str(), "hello");
        buf.undo().unwrap();
        assert_eq!(buf.as_str(), "");
        assert_eq!(buf.cursor(), 0);
    }

    #[test]
    fn undo_delete() {
        let mut buf = LineBuffer::from_str("hello");
        let _ = buf.delete_range(2, 1).unwrap(); // delete 'l'
        assert_eq!(buf.as_str(), "helo");
        buf.undo().unwrap();
        assert_eq!(buf.as_str(), "hello");
    }

    #[test]
    fn undo_stack_empty_returns_error() {
        let mut buf = LineBuffer::new();
        let err = buf.undo().unwrap_err();
        assert!(matches!(err, OxidreadError::UndoStackEmpty));
    }

    // -- undo groups --

    #[test]
    fn undo_group_reverts_multiple_edits() {
        let mut buf = LineBuffer::new();
        buf.begin_undo_group();
        buf.insert("he").unwrap();
        buf.insert("llo").unwrap();
        buf.end_undo_group().unwrap();

        assert_eq!(buf.as_str(), "hello");
        buf.undo().unwrap();
        assert_eq!(buf.as_str(), "");
    }

    // -- mark --

    #[test]
    fn set_and_exchange_mark() {
        let mut buf = LineBuffer::from_str("hello");
        buf.set_cursor(2).unwrap();
        buf.set_mark();
        buf.move_to_end();
        buf.exchange_point_and_mark().unwrap();
        assert_eq!(buf.cursor(), 2);
        assert_eq!(buf.mark(), Some(5));
    }

    // -- replace_line --

    #[test]
    fn replace_line_clears_undo() {
        let mut buf = LineBuffer::new();
        buf.insert("old").unwrap();
        buf.replace_line("new content", true);
        assert_eq!(buf.as_str(), "new content");
        let err = buf.undo().unwrap_err();
        assert!(matches!(err, OxidreadError::UndoStackEmpty));
    }

    // -- display width --

    #[test]
    fn display_width_ascii() {
        let buf = LineBuffer::from_str("hello");
        assert_eq!(buf.display_width(), 5);
    }

    #[test]
    fn display_width_wide_chars() {
        // CJK characters are 2 columns wide each.
        let buf = LineBuffer::from_str("こんにちは");
        assert_eq!(buf.display_width(), 10);
    }
}

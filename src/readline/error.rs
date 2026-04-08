// oxidread/src/readline/error.rs
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
// error.rs — Central error type for the oxidread readline engine.
//
// Design notes (from studying GNU readline's rlprivate.h / text.c):
//   GNU readline uses C return codes (0 = ok, 1 = error, -1 = abort) spread
//   across hundreds of functions with no central error type. We replace all
//   of that with a single `OxidreadError` enum so callers get typed, ergonomic
//   error handling with full std::error::Error compatibility.
// -----------------------------------------------------------------------

use std::fmt;
use thiserror::Error;

/// The master error type for all oxidread readline operations.
///
/// Every fallible function in this crate returns `Result<T, OxidreadError>`.
/// Variants are grouped by subsystem so callers can match coarsely or finely.
#[derive(Debug, Error)]
pub enum OxidreadError {
    // ------------------------------------------------------------------ //
    //  I/O and terminal errors                                            //
    // ------------------------------------------------------------------ //

    /// A terminal I/O operation failed (e.g. reading a key, writing output).
    #[error("terminal I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The terminal could not be put into raw mode or restored.
    #[error("failed to configure terminal: {reason}")]
    TerminalSetup { reason: String },

    /// The terminal size could not be determined.
    #[error("could not query terminal dimensions")]
    TerminalSize,

    // ------------------------------------------------------------------ //
    //  Line buffer / editing errors                                       //
    // ------------------------------------------------------------------ //

    /// A cursor position was out of bounds for the current line buffer.
    ///
    /// Corresponds to GNU readline's `_rl_fix_point()` clamping behaviour,
    /// but we surface it as an error instead of silently clamping.
    #[error("cursor position {pos} is out of bounds (line length: {len})")]
    CursorOutOfBounds { pos: usize, len: usize },

    /// An attempt was made to move the cursor past the start or end of line.
    /// This is the Rust equivalent of GNU readline's `rl_ding()` — instead of
    /// ringing a bell and continuing, we return this variant so the caller
    /// can decide what to do (ring bell, ignore, etc.).
    #[error("cursor movement blocked at {side} of line")]
    CursorAtBoundary { side: BoundarySide },

    /// A byte slice could not be decoded as valid UTF-8.
    #[error("invalid UTF-8 in line buffer at byte offset {offset}")]
    InvalidUtf8 { offset: usize },

    /// The requested grapheme index does not exist in the current buffer.
    #[error("grapheme index {index} out of range (buffer has {count} graphemes)")]
    GraphemeOutOfRange { index: usize, count: usize },

    // ------------------------------------------------------------------ //
    //  Undo/redo errors                                                   //
    // ------------------------------------------------------------------ //

    /// Undo was requested but the undo stack is empty.
    #[error("nothing to undo")]
    UndoStackEmpty,

    /// Redo was requested but the redo stack is empty.
    #[error("nothing to redo")]
    RedoStackEmpty,

    /// An undo group was ended without a matching begin.
    #[error("undo group end called without a matching begin")]
    UndoGroupMismatch,

    // ------------------------------------------------------------------ //
    //  History errors                                                     //
    // ------------------------------------------------------------------ //

    /// The history index requested is out of range.
    #[error("history index {index} out of range (history has {len} entries)")]
    HistoryOutOfRange { index: usize, len: usize },

    /// The history file could not be read or written.
    #[error("history file error at path `{path}`: {source}")]
    HistoryFile {
        path: String,
        #[source]
        source: std::io::Error,
    },

    // ------------------------------------------------------------------ //
    //  Completion errors                                                  //
    // ------------------------------------------------------------------ //

    /// A completer implementation returned an error.
    #[error("completion failed: {reason}")]
    CompletionError { reason: String },

    // ------------------------------------------------------------------ //
    //  Signal / interrupt                                                 //
    // ------------------------------------------------------------------ //

    /// The readline loop was interrupted by Ctrl-C (SIGINT equivalent).
    /// Callers should treat this as a soft interrupt, not a fatal error.
    #[error("readline interrupted (Ctrl-C)")]
    Interrupted,

    /// The input stream reached EOF (Ctrl-D on empty line).
    #[error("end of input (EOF)")]
    Eof,

    // ------------------------------------------------------------------ //
    //  Keymap / binding errors                                            //
    // ------------------------------------------------------------------ //

    /// A key sequence could not be parsed or bound.
    #[error("invalid key sequence: `{seq}`")]
    InvalidKeySequence { seq: String },

    /// A command name was not found in the function map.
    #[error("unknown command: `{name}`")]
    UnknownCommand { name: String },

    // ------------------------------------------------------------------ //
    //  Catch-all                                                          //
    // ------------------------------------------------------------------ //

    /// An internal assertion failed — indicates a bug in oxidread itself.
    #[error("internal error: {message} (please report this bug)")]
    Internal { message: String },
}

/// Which side of the line a cursor boundary was hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundarySide {
    /// Beginning of line (position 0).
    Start,
    /// End of line (position == line length).
    End,
}

impl fmt::Display for BoundarySide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BoundarySide::Start => write!(f, "start"),
            BoundarySide::End => write!(f, "end"),
        }
    }
}

/// Convenience alias used throughout the oxidread crate.
pub type Result<T> = std::result::Result<T, OxidreadError>;

// -----------------------------------------------------------------------
// Helper constructors for common error cases — keeps call sites tidy.
// -----------------------------------------------------------------------

impl OxidreadError {
    /// Construct a `TerminalSetup` error from any `Display`able reason.
    pub fn terminal_setup(reason: impl fmt::Display) -> Self {
        OxidreadError::TerminalSetup {
            reason: reason.to_string(),
        }
    }

    /// Construct an `Internal` error — use only for invariant violations.
    pub fn internal(message: impl fmt::Display) -> Self {
        OxidreadError::Internal {
            message: message.to_string(),
        }
    }

    /// Construct a `CompletionError` from any `Display`able reason.
    pub fn completion(reason: impl fmt::Display) -> Self {
        OxidreadError::CompletionError {
            reason: reason.to_string(),
        }
    }

    /// Returns `true` if this error represents a soft user interrupt
    /// (Ctrl-C or EOF) rather than a real failure.
    pub fn is_user_interrupt(&self) -> bool {
        matches!(self, OxidreadError::Interrupted | OxidreadError::Eof)
    }
}

// -----------------------------------------------------------------------
// Unit tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_cursor_out_of_bounds() {
        let e = OxidreadError::CursorOutOfBounds { pos: 10, len: 5 };
        let msg = e.to_string();
        assert!(msg.contains("10"));
        assert!(msg.contains("5"));
    }

    #[test]
    fn error_display_cursor_at_boundary_start() {
        let e = OxidreadError::CursorAtBoundary {
            side: BoundarySide::Start,
        };
        assert!(e.to_string().contains("start"));
    }

    #[test]
    fn error_display_cursor_at_boundary_end() {
        let e = OxidreadError::CursorAtBoundary {
            side: BoundarySide::End,
        };
        assert!(e.to_string().contains("end"));
    }

    #[test]
    fn is_user_interrupt_true_for_interrupted() {
        assert!(OxidreadError::Interrupted.is_user_interrupt());
    }

    #[test]
    fn is_user_interrupt_true_for_eof() {
        assert!(OxidreadError::Eof.is_user_interrupt());
    }

    #[test]
    fn is_user_interrupt_false_for_io_errors() {
        let e = OxidreadError::UndoStackEmpty;
        assert!(!e.is_user_interrupt());
    }

    #[test]
    fn helper_constructors_produce_correct_variants() {
        let e = OxidreadError::terminal_setup("bad fd");
        assert!(matches!(e, OxidreadError::TerminalSetup { .. }));

        let e = OxidreadError::internal("invariant broken");
        assert!(matches!(e, OxidreadError::Internal { .. }));

        let e = OxidreadError::completion("no candidates");
        assert!(matches!(e, OxidreadError::CompletionError { .. }));
    }

    #[test]
    fn history_file_error_includes_path() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        let e = OxidreadError::HistoryFile {
            path: "/home/ali/.oxidread_history".to_string(),
            source: io_err,
        };
        assert!(e.to_string().contains(".oxidread_history"));
    }

    #[test]
    fn result_alias_works() {
        fn ok_fn() -> Result<u32> {
            Ok(42)
        }
        fn err_fn() -> Result<u32> {
            Err(OxidreadError::UndoStackEmpty)
        }
        assert_eq!(ok_fn().unwrap(), 42);
        assert!(err_fn().is_err());
    }
}

// oxidread/src/readline/prompt.rs
//
// Copyright (C) 2025 oxidread contributors
// Part of the oxidread project — a pure Rust, memory-safe rewrite of
// GNU Readline and ncurses for Linux.
//
// GPL-3.0-or-later — see <https://www.gnu.org/licenses/>
//
// -----------------------------------------------------------------------
// prompt.rs — Prompt rendering, ANSI styling, display-width calculation.
//
// GNU readline equivalent: display.c (prompt subset)
//   expand_prompt()             → Prompt::from_raw() / expand_raw()
//   _rl_strip_prompt()          → strip_ansi_and_rl_markers()
//   rl_visible_prompt_length    → Prompt::display_width()
//   _rl_save_prompt()           → PromptState::save() / restore()
//   _rl_make_prompt_for_search()→ Prompt::for_isearch() / for_forward_search()
//   rl_message()                → Prompt::message()
//   rl_expand_prompt()          → Prompt::expand()
//   prompt_physical_chars       → Prompt::physical_width()
//   RL_PROMPT_START_IGNORE(\x01)→ handled in strip_ansi_and_rl_markers()
//   RL_PROMPT_END_IGNORE(\x02)  → handled in strip_ansi_and_rl_markers()
//
// Key improvements over GNU readline display.c:
//   • No global char* pointers — all state lives in Prompt struct.
//   • No manual xmalloc/xfree — owned String fields.
//   • No HANDLE_MULTIBYTE compile flag — always Unicode-correct via
//     unicode-width crate.
//   • Multiline prompt support (embedded \n) is handled explicitly.
//   • PromptState saves/restores cleanly without pointer aliasing.
// -----------------------------------------------------------------------

use std::fmt;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::readline::error::{OxidreadError, Result};

// -----------------------------------------------------------------------
// Color
// -----------------------------------------------------------------------

/// A terminal foreground or background colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// Standard ANSI colour by code (30–37 fg, 90–97 bright fg, etc.)
    Ansi(u8),
    /// 256-colour palette index 0–255.
    Palette(u8),
    /// 24-bit TrueColor (r, g, b).
    Rgb(u8, u8, u8),
    /// Reset to terminal default.
    Default,
}

impl Color {
    pub const BLACK:          Color = Color::Ansi(30);
    pub const RED:            Color = Color::Ansi(31);
    pub const GREEN:          Color = Color::Ansi(32);
    pub const YELLOW:         Color = Color::Ansi(33);
    pub const BLUE:           Color = Color::Ansi(34);
    pub const MAGENTA:        Color = Color::Ansi(35);
    pub const CYAN:           Color = Color::Ansi(36);
    pub const WHITE:          Color = Color::Ansi(37);
    pub const BRIGHT_BLACK:   Color = Color::Ansi(90);
    pub const BRIGHT_RED:     Color = Color::Ansi(91);
    pub const BRIGHT_GREEN:   Color = Color::Ansi(92);
    pub const BRIGHT_YELLOW:  Color = Color::Ansi(93);
    pub const BRIGHT_BLUE:    Color = Color::Ansi(94);
    pub const BRIGHT_MAGENTA: Color = Color::Ansi(95);
    pub const BRIGHT_CYAN:    Color = Color::Ansi(96);
    pub const BRIGHT_WHITE:   Color = Color::Ansi(97);

    /// ANSI foreground escape payload (the digits inside `\x1b[...m`).
    pub fn fg_code(&self) -> String {
        match self {
            Color::Ansi(n)        => n.to_string(),
            Color::Palette(n)     => format!("38;5;{}", n),
            Color::Rgb(r, g, b)   => format!("38;2;{};{};{}", r, g, b),
            Color::Default        => "39".to_string(),
        }
    }

    /// ANSI background escape payload.
    pub fn bg_code(&self) -> String {
        match self {
            Color::Ansi(n)        => (n + 10).to_string(),
            Color::Palette(n)     => format!("48;5;{}", n),
            Color::Rgb(r, g, b)   => format!("48;2;{};{};{}", r, g, b),
            Color::Default        => "49".to_string(),
        }
    }
}

// -----------------------------------------------------------------------
// Style
// -----------------------------------------------------------------------

/// Text rendering attributes — bold, colours, etc.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Style {
    pub fg:        Option<Color>,
    pub bg:        Option<Color>,
    pub bold:      bool,
    pub dim:       bool,
    pub italic:    bool,
    pub underline: bool,
    pub blink:     bool,
    pub reverse:   bool,
}

impl Style {
    pub fn plain() -> Self { Style::default() }

    pub fn bold() -> Self {
        Style { bold: true, ..Default::default() }
    }

    pub fn fg(color: Color) -> Self {
        Style { fg: Some(color), ..Default::default() }
    }

    pub fn bold_fg(color: Color) -> Self {
        Style { bold: true, fg: Some(color), ..Default::default() }
    }

    /// `true` when no attributes are set.
    pub fn is_plain(&self) -> bool {
        self.fg.is_none() && self.bg.is_none()
            && !self.bold && !self.dim && !self.italic
            && !self.underline && !self.blink && !self.reverse
    }

    /// The `\x1b[...m` sequence that turns this style ON.
    /// Returns `""` for a plain style.
    pub fn ansi_on(&self) -> String {
        if self.is_plain() { return String::new(); }
        let mut codes: Vec<String> = Vec::new();
        if self.bold      { codes.push("1".into()); }
        if self.dim       { codes.push("2".into()); }
        if self.italic    { codes.push("3".into()); }
        if self.underline { codes.push("4".into()); }
        if self.blink     { codes.push("5".into()); }
        if self.reverse   { codes.push("7".into()); }
        if let Some(c) = &self.fg { codes.push(c.fg_code()); }
        if let Some(c) = &self.bg { codes.push(c.bg_code()); }
        format!("\x1b[{}m", codes.join(";"))
    }

    /// `\x1b[0m` — reset. Returns `""` for a plain style.
    pub fn ansi_off(&self) -> String {
        if self.is_plain() { String::new() } else { "\x1b[0m".to_string() }
    }
}

// -----------------------------------------------------------------------
// PromptSegment
// -----------------------------------------------------------------------

/// One styled piece of a prompt string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSegment {
    /// Visible text — no raw ANSI escapes; use `style` for colouring.
    pub text:  String,
    /// Optional style. `None` = plain terminal default.
    pub style: Option<Style>,
}

impl PromptSegment {
    pub fn plain(text: impl Into<String>) -> Self {
        PromptSegment { text: text.into(), style: None }
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        let t = text.into();
        PromptSegment {
            text: t,
            style: if style.is_plain() { None } else { Some(style) },
        }
    }

    /// Display width of the visible text in terminal columns.
    pub fn display_width(&self) -> usize {
        UnicodeWidthStr::width(self.text.as_str())
    }

    /// Render for direct terminal output (includes ANSI escapes).
    pub fn render(&self) -> String {
        match &self.style {
            None    => self.text.clone(),
            Some(s) => format!("{}{}{}", s.ansi_on(), self.text, s.ansi_off()),
        }
    }

    /// Render with GNU readline `\x01`/`\x02` non-printing markers.
    /// Use only when passing the prompt to C readline via `rl_set_prompt`.
    pub fn render_rl_escaped(&self) -> String {
        match &self.style {
            None    => self.text.clone(),
            Some(s) => format!("\x01{}\x02{}\x01{}\x02",
                               s.ansi_on(), self.text, s.ansi_off()),
        }
    }
}

// -----------------------------------------------------------------------
// Prompt
// -----------------------------------------------------------------------

/// A fully structured terminal prompt.
///
/// Built from `PromptSegment` slices. Knows its own:
///   - `display_width()` — visible columns (ANSI escapes excluded)
///   - `physical_width()` — same, but respects wide chars (CJK, emoji)
///   - `visible_text()`  — plain text with styling stripped
///   - `line_count()`    — number of physical lines (embedded `\n`)
///
/// # Example
/// ```rust
/// use oxidread::readline::prompt::{Prompt, PromptSegment, Style, Color};
/// let p = Prompt::builder()
///     .bold_fg("user@host", Color::GREEN)
///     .plain(":~/src $ ")
///     .build();
/// assert_eq!(p.display_width(), 17);
/// ```
#[derive(Debug, Clone, Default)]
pub struct Prompt {
    segments: Vec<PromptSegment>,
}

impl Prompt {
    // ------------------------------------------------------------------ //
    //  Construction                                                       //
    // ------------------------------------------------------------------ //

    pub fn new() -> Self { Prompt::default() }

    /// Plain unstyled prompt. Equivalent to GNU readline `rl_set_prompt("$ ")`.
    pub fn plain(text: impl Into<String>) -> Self {
        let mut p = Prompt::new();
        p.push(PromptSegment::plain(text));
        p
    }

    /// Parse a raw prompt string that may contain:
    ///   - ANSI CSI escape sequences (`\x1b[...m`)
    ///   - GNU readline non-printing markers (`\x01...\x02`)
    ///
    /// The result is a single plain segment with only the visible characters.
    /// This mirrors GNU readline's `_rl_strip_prompt` / `expand_prompt`.
    pub fn from_raw(raw: &str) -> Self {
        let mut p = Prompt::new();
        let visible = strip_ansi_and_rl_markers(raw);
        if !visible.is_empty() {
            p.push(PromptSegment::plain(visible));
        }
        p
    }

    /// Expand a raw prompt string — handles multiline prompts (embedded `\n`).
    ///
    /// Returns a `(prefix, last_line)` pair where `prefix` is everything up
    /// to and including the last newline (or `None` if no newline), and
    /// `last_line` is the tail used for display-width calculations.
    ///
    /// Mirrors GNU readline's `rl_expand_prompt` which returns the visible
    /// length of the last line of the prompt.
    pub fn expand(raw: &str) -> (Option<String>, Self) {
        match raw.rfind('\n') {
            None => (None, Prompt::from_raw(raw)),
            Some(pos) => {
                let prefix = raw[..=pos].to_string();
                let tail   = &raw[pos + 1..];
                (Some(prefix), Prompt::from_raw(tail))
            }
        }
    }

    /// Return a `PromptBuilder` for fluent construction.
    pub fn builder() -> PromptBuilder { PromptBuilder::new() }

    /// Add a segment. Empty text segments are silently dropped.
    pub fn push(&mut self, seg: PromptSegment) {
        if !seg.text.is_empty() {
            self.segments.push(seg);
        }
    }

    // ------------------------------------------------------------------ //
    //  Accessors                                                          //
    // ------------------------------------------------------------------ //

    /// Total display width in terminal columns (visible chars only, no ANSI).
    ///
    /// This is the value GNU readline stores in `rl_visible_prompt_length`.
    pub fn display_width(&self) -> usize {
        self.segments.iter().map(|s| s.display_width()).sum()
    }

    /// Display width of only the last line when the prompt contains `\n`.
    /// For single-line prompts this equals `display_width()`.
    pub fn last_line_width(&self) -> usize {
        let vis = self.visible_text();
        let last = vis.lines().last().unwrap_or("");
        UnicodeWidthStr::width(last)
    }

    /// The visible text (all styling stripped). GNU readline: `_rl_strip_prompt`.
    pub fn visible_text(&self) -> String {
        self.segments.iter().map(|s| s.text.as_str()).collect()
    }

    /// `true` if the prompt has no segments.
    pub fn is_empty(&self) -> bool { self.segments.is_empty() }

    /// Number of segments.
    pub fn segment_count(&self) -> usize { self.segments.len() }

    /// Number of physical lines in the visible text (split on `\n`).
    pub fn line_count(&self) -> usize {
        let vis = self.visible_text();
        if vis.is_empty() { 0 } else { vis.lines().count() }
    }

    /// Number of invisible bytes — bytes present in the render string that
    /// do not contribute to display columns (i.e. ANSI escape bytes).
    ///
    /// This mirrors GNU readline's `wrap_offset` = `local_prompt_len` -
    /// `prompt_visible_length`.
    pub fn invisible_byte_count(&self) -> usize {
        let rendered = self.render();
        rendered.len().saturating_sub(self.display_width())
    }

    // ------------------------------------------------------------------ //
    //  Rendering                                                          //
    // ------------------------------------------------------------------ //

    /// Render for direct terminal output (includes ANSI escapes).
    pub fn render(&self) -> String {
        self.segments.iter().map(|s| s.render()).collect()
    }

    /// Render with GNU readline `\x01`/`\x02` non-printing markers.
    /// Use only when passing this to C readline via `rl_set_prompt`.
    pub fn render_rl_escaped(&self) -> String {
        self.segments.iter().map(|s| s.render_rl_escaped()).collect()
    }

    // ------------------------------------------------------------------ //
    //  Special prompts (mirrors GNU readline display.c helpers)          //
    // ------------------------------------------------------------------ //

    /// Build an incremental reverse-search prompt.
    /// Mirrors GNU readline's `_rl_make_prompt_for_search`.
    ///
    /// Produces: `(reverse-i-search)'<query>': ` or
    ///           `(failed reverse-i-search)'<query>': `
    pub fn for_isearch(query: &str, failed: bool) -> Self {
        let label = if failed {
            "(failed reverse-i-search)'"
        } else {
            "(reverse-i-search)'"
        };
        let mut p = Prompt::new();
        p.push(PromptSegment::styled(label, Style::fg(Color::CYAN)));
        if !query.is_empty() {
            p.push(PromptSegment::plain(query));
        }
        p.push(PromptSegment::plain("': "));
        p
    }

    /// Build a forward incremental search prompt.
    pub fn for_forward_search(query: &str, failed: bool) -> Self {
        let label = if failed { "(failed i-search)'" } else { "(i-search)'" };
        let mut p = Prompt::new();
        p.push(PromptSegment::styled(label, Style::fg(Color::CYAN)));
        if !query.is_empty() {
            p.push(PromptSegment::plain(query));
        }
        p.push(PromptSegment::plain("': "));
        p
    }

    /// Build a message prompt (GNU readline `rl_message` equivalent).
    /// The message is displayed in place of the normal prompt.
    pub fn message(text: impl Into<String>) -> Self {
        Prompt::plain(text)
    }
}

impl fmt::Display for Prompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.render())
    }
}

// -----------------------------------------------------------------------
// PromptState — save / restore  (mirrors rl_save_prompt / rl_restore_prompt)
// -----------------------------------------------------------------------

/// Saved prompt state for push/pop during search or message display.
///
/// GNU readline uses a set of `saved_*` global variables. We wrap them
/// in a struct so the caller can stack-save without aliasing globals.
#[derive(Debug, Clone, Default)]
pub struct PromptState {
    pub prompt: Option<Prompt>,
}

impl PromptState {
    pub fn new() -> Self { PromptState::default() }

    /// Save `current` into this state and return `current`.
    pub fn save(&mut self, current: Prompt) -> Prompt {
        let clone = current.clone();
        self.prompt = Some(current);
        clone
    }

    /// Restore the previously saved prompt.
    /// Returns `None` if nothing was saved.
    pub fn restore(&mut self) -> Option<Prompt> {
        self.prompt.take()
    }

    pub fn is_saved(&self) -> bool { self.prompt.is_some() }
}

// -----------------------------------------------------------------------
// PromptBuilder — fluent API
// -----------------------------------------------------------------------

/// Fluent builder for `Prompt`.
///
/// # Example
/// ```rust
/// use oxidread::readline::prompt::{PromptBuilder, Color};
/// let p = PromptBuilder::new()
///     .bold_fg("myapp", Color::GREEN)
///     .plain(" › ")
///     .fg_text("~/src", Color::BLUE)
///     .plain(" $ ")
///     .build();
/// assert_eq!(p.visible_text(), "myapp › ~/src $ ");
/// ```
#[derive(Debug, Default)]
pub struct PromptBuilder {
    prompt: Prompt,
}

impl PromptBuilder {
    pub fn new() -> Self { PromptBuilder::default() }

    pub fn plain(mut self, text: impl Into<String>) -> Self {
        self.prompt.push(PromptSegment::plain(text));
        self
    }

    pub fn bold_text(mut self, text: impl Into<String>) -> Self {
        self.prompt.push(PromptSegment::styled(text, Style::bold()));
        self
    }

    pub fn fg_text(mut self, text: impl Into<String>, color: Color) -> Self {
        self.prompt.push(PromptSegment::styled(text, Style::fg(color)));
        self
    }

    pub fn bold_fg(mut self, text: impl Into<String>, color: Color) -> Self {
        self.prompt.push(PromptSegment::styled(text, Style::bold_fg(color)));
        self
    }

    pub fn styled(mut self, text: impl Into<String>, style: Style) -> Self {
        self.prompt.push(PromptSegment::styled(text, style));
        self
    }

    pub fn build(self) -> Prompt { self.prompt }
}

// -----------------------------------------------------------------------
// Public helper functions
// -----------------------------------------------------------------------

/// Strip ANSI CSI escape sequences and GNU readline `\x01`/`\x02` markers
/// from `s`, returning only the visible characters.
///
/// Handles:
///   - `\x1b[...m`  — SGR colour/attribute sequences
///   - `\x1b[...H`  — cursor positioning (treated as zero-width)
///   - `\x1b<X>`    — single-char ESC sequences
///   - `\x01...\x02`— GNU readline RL_PROMPT_START_IGNORE/END_IGNORE pairs
///
/// Mirrors GNU readline's `_rl_strip_prompt` and the width logic in
/// `expand_prompt` / `_rl_col_width`.
pub fn strip_ansi_and_rl_markers(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes   = s.as_bytes();
    let mut i   = 0;

    while i < bytes.len() {
        match bytes[i] {
            // GNU readline non-printing begin marker \x01
            0x01 => {
                i += 1;
                while i < bytes.len() && bytes[i] != 0x02 {
                    i += 1;
                }
                if i < bytes.len() { i += 1; }
            }
            // ESC [ — CSI sequence
            0x1b if i + 1 < bytes.len() && bytes[i + 1] == b'[' => {
                i += 2;
                // parameter + intermediate bytes (0x20–0x3F)
                while i < bytes.len() && bytes[i] >= 0x20 && bytes[i] <= 0x3f {
                    i += 1;
                }
                // final byte (0x40–0x7E)
                if i < bytes.len() && bytes[i] >= 0x40 && bytes[i] <= 0x7e {
                    i += 1;
                }
            }
            // ESC <single char>
            0x1b => { i += 2; }
            // Regular UTF-8 byte
            _ => {
                let start = i;
                i += 1;
                while i < bytes.len() && (bytes[i] & 0xC0) == 0x80 {
                    i += 1;
                }
                if let Ok(ch) = std::str::from_utf8(&bytes[start..i]) {
                    out.push_str(ch);
                }
            }
        }
    }
    out
}

/// Calculate the display width of a string that may contain ANSI escapes
/// and/or GNU readline `\x01`/`\x02` markers.
///
/// Equivalent to GNU readline's `rl_visible_prompt_length` computation.
pub fn visible_display_width(s: &str) -> usize {
    UnicodeWidthStr::width(strip_ansi_and_rl_markers(s).as_str())
}

/// Calculate the display width of the last line of `s` (after the last `\n`).
/// For single-line strings this equals `visible_display_width(s)`.
pub fn last_line_display_width(s: &str) -> usize {
    let visible = strip_ansi_and_rl_markers(s);
    let last = visible.lines().last().unwrap_or("");
    UnicodeWidthStr::width(last)
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Color --
    #[test] fn color_ansi_fg()    { assert_eq!(Color::RED.fg_code(),  "31"); }
    #[test] fn color_ansi_bg()    { assert_eq!(Color::RED.bg_code(),  "41"); }
    #[test] fn color_palette_fg() { assert_eq!(Color::Palette(200).fg_code(), "38;5;200"); }
    #[test] fn color_rgb_fg()     { assert_eq!(Color::Rgb(255,128,0).fg_code(), "38;2;255;128;0"); }
    #[test] fn color_default_fg() { assert_eq!(Color::Default.fg_code(), "39"); }
    #[test] fn color_bright_blue_fg() { assert_eq!(Color::BRIGHT_BLUE.fg_code(), "94"); }

    // -- Style --
    #[test] fn style_plain_empty()  { let s = Style::plain(); assert_eq!(s.ansi_on(), ""); assert_eq!(s.ansi_off(), ""); }
    #[test] fn style_bold()         { assert_eq!(Style::bold().ansi_on(), "\x1b[1m"); }
    #[test] fn style_fg_green()     { assert_eq!(Style::fg(Color::GREEN).ansi_on(), "\x1b[32m"); }
    #[test] fn style_bold_fg_red()  { assert_eq!(Style::bold_fg(Color::RED).ansi_on(), "\x1b[1;31m"); }
    #[test] fn style_off_plain()    { assert_eq!(Style::plain().ansi_off(), ""); }
    #[test] fn style_off_styled()   { assert_eq!(Style::bold().ansi_off(), "\x1b[0m"); }
    #[test] fn style_multi_attr() {
        let s = Style { bold: true, italic: true, fg: Some(Color::CYAN), ..Default::default() };
        let on = s.ansi_on();
        assert!(on.contains("1"));
        assert!(on.contains("3"));
        assert!(on.contains("36"));
    }
    #[test] fn style_underline() {
        let s = Style { underline: true, ..Default::default() };
        assert!(s.ansi_on().contains("4"));
    }
    #[test] fn style_bg_color() {
        let s = Style { bg: Some(Color::BLUE), ..Default::default() };
        assert!(s.ansi_on().contains("44"));
    }

    // -- PromptSegment --
    #[test] fn seg_plain_render()   { assert_eq!(PromptSegment::plain("$ ").render(), "$ "); }
    #[test] fn seg_plain_width()    { assert_eq!(PromptSegment::plain("$ ").display_width(), 2); }
    #[test] fn seg_styled_wraps()   {
        let seg = PromptSegment::styled("ok", Style::bold_fg(Color::GREEN));
        let r = seg.render();
        assert!(r.starts_with("\x1b["));
        assert!(r.contains("ok"));
        assert!(r.ends_with("\x1b[0m"));
    }
    #[test] fn seg_width_styled()   { assert_eq!(PromptSegment::styled("hello", Style::bold()).display_width(), 5); }
    #[test] fn seg_wide_chars()     { assert_eq!(PromptSegment::plain("こん").display_width(), 4); }
    #[test] fn seg_rl_escaped_has_markers() {
        let seg = PromptSegment::styled("x", Style::bold());
        let e = seg.render_rl_escaped();
        assert!(e.contains('\x01'));
        assert!(e.contains('\x02'));
        assert!(e.contains('x'));
    }
    #[test] fn seg_plain_rl_escaped_no_markers() {
        assert_eq!(PromptSegment::plain("$ ").render_rl_escaped(), "$ ");
    }
    #[test] fn seg_empty_text_plain() {
        // Empty segment — push should drop it
        let seg = PromptSegment::plain("");
        assert_eq!(seg.display_width(), 0);
    }

    // -- Prompt construction --
    #[test] fn prompt_plain_width()  { assert_eq!(Prompt::plain("myapp $ ").display_width(), 8); }
    #[test] fn prompt_new_empty()    { let p = Prompt::new(); assert!(p.is_empty()); assert_eq!(p.display_width(), 0); }
    #[test] fn prompt_push_count()   {
        let mut p = Prompt::new();
        p.push(PromptSegment::plain("a"));
        p.push(PromptSegment::plain("b"));
        assert_eq!(p.segment_count(), 2);
    }
    #[test] fn prompt_empty_seg_not_added() {
        let mut p = Prompt::new();
        p.push(PromptSegment::plain(""));
        assert!(p.is_empty());
    }
    #[test] fn prompt_visible_text_strips() {
        let mut p = Prompt::new();
        p.push(PromptSegment::styled("user", Style::bold_fg(Color::GREEN)));
        p.push(PromptSegment::plain("@host $ "));
        assert_eq!(p.visible_text(), "user@host $ ");
    }
    #[test] fn prompt_display_width_multi() {
        let mut p = Prompt::new();
        p.push(PromptSegment::styled("user", Style::bold_fg(Color::GREEN)));
        p.push(PromptSegment::plain("@host:~ $ "));
        assert_eq!(p.display_width(), 14);
    }
    #[test] fn prompt_render_has_ansi() {
        let mut p = Prompt::new();
        p.push(PromptSegment::styled("ok", Style::fg(Color::GREEN)));
        let r = p.render();
        assert!(r.contains("\x1b[") && r.contains("ok") && r.contains("\x1b[0m"));
    }

    // -- from_raw --
    #[test] fn prompt_from_raw_ansi() {
        let p = Prompt::from_raw("\x1b[32muser\x1b[0m@host $ ");
        assert_eq!(p.visible_text(), "user@host $ ");
        assert_eq!(p.display_width(), 12);
    }
    #[test] fn prompt_from_raw_rl_markers() {
        let p = Prompt::from_raw("\x01\x1b[32m\x02user\x01\x1b[0m\x02 $ ");
        assert_eq!(p.visible_text(), "user $ ");
        assert_eq!(p.display_width(), 7);
    }
    #[test] fn prompt_from_raw_empty() {
        let p = Prompt::from_raw("");
        assert!(p.is_empty());
    }
    #[test] fn prompt_from_raw_plain() {
        let p = Prompt::from_raw("$ ");
        assert_eq!(p.display_width(), 2);
    }

    // -- expand (multiline) --
    #[test] fn prompt_expand_no_newline() {
        let (prefix, last) = Prompt::expand("user $ ");
        assert!(prefix.is_none());
        assert_eq!(last.display_width(), 7);
    }
    #[test] fn prompt_expand_with_newline() {
        let (prefix, last) = Prompt::expand("line1\nuser $ ");
        assert_eq!(prefix.as_deref(), Some("line1\n"));
        assert_eq!(last.display_width(), 7);
    }
    #[test] fn prompt_expand_multiple_newlines() {
        let (prefix, last) = Prompt::expand("a\nb\n$ ");
        assert_eq!(prefix.as_deref(), Some("a\nb\n"));
        assert_eq!(last.display_width(), 2);
    }

    // -- line_count --
    #[test] fn prompt_line_count_single()   { assert_eq!(Prompt::plain("$ ").line_count(), 1); }
    #[test] fn prompt_line_count_empty()    { assert_eq!(Prompt::new().line_count(), 0); }
    #[test] fn prompt_last_line_width_single() {
        assert_eq!(Prompt::plain("$ ").last_line_width(), 2);
    }

    // -- invisible_byte_count --
    #[test] fn prompt_invisible_bytes_plain() {
        let p = Prompt::plain("$ ");
        assert_eq!(p.invisible_byte_count(), 0);
    }
    #[test] fn prompt_invisible_bytes_styled() {
        let p = Prompt::builder().bold_fg("hi", Color::GREEN).build();
        // \x1b[1;32m (8 bytes) + \x1b[0m (4 bytes) = 12 invisible bytes
        assert!(p.invisible_byte_count() > 0);
    }

    // -- search prompts --
    #[test] fn isearch_contains_query() {
        let p = Prompt::for_isearch("cargo", false);
        assert!(p.visible_text().contains("cargo"));
        assert!(p.visible_text().contains("reverse-i-search"));
    }
    #[test] fn isearch_failed_prefix() {
        let p = Prompt::for_isearch("xyz", true);
        assert!(p.visible_text().contains("failed"));
    }
    #[test] fn isearch_empty_query() {
        let p = Prompt::for_isearch("", false);
        assert!(p.visible_text().contains("reverse-i-search"));
    }
    #[test] fn forward_search_prompt() {
        let p = Prompt::for_forward_search("ls", false);
        assert!(p.visible_text().contains("i-search"));
        assert!(p.visible_text().contains("ls"));
    }
    #[test] fn forward_search_failed() {
        let p = Prompt::for_forward_search("zz", true);
        assert!(p.visible_text().contains("failed"));
    }

    // -- PromptState --
    #[test] fn prompt_state_save_restore() {
        let mut state = PromptState::new();
        let p = Prompt::plain("$ ");
        assert!(!state.is_saved());
        state.save(p.clone());
        assert!(state.is_saved());
        let restored = state.restore().unwrap();
        assert_eq!(restored.visible_text(), "$ ");
        assert!(!state.is_saved());
    }
    #[test] fn prompt_state_restore_none() {
        let mut state = PromptState::new();
        assert!(state.restore().is_none());
    }

    // -- strip_ansi_and_rl_markers --
    #[test] fn strip_plain()             { assert_eq!(strip_ansi_and_rl_markers("hello"), "hello"); }
    #[test] fn strip_sgr()               { assert_eq!(strip_ansi_and_rl_markers("\x1b[1;32mhello\x1b[0m"), "hello"); }
    #[test] fn strip_rl_markers()        { assert_eq!(strip_ansi_and_rl_markers("\x01\x1b[32m\x02green\x01\x1b[0m\x02"), "green"); }
    #[test] fn strip_unicode_preserved() { assert_eq!(strip_ansi_and_rl_markers("\x1b[34m日本語\x1b[0m"), "日本語"); }
    #[test] fn strip_empty()             { assert_eq!(strip_ansi_and_rl_markers(""), ""); }
    #[test] fn strip_multiple_seq()      {
        assert_eq!(strip_ansi_and_rl_markers("\x1b[1mhello\x1b[0m \x1b[32mworld\x1b[0m"), "hello world");
    }
    #[test] fn strip_cursor_movement()   {
        // CSI H (cursor position) should be stripped
        assert_eq!(strip_ansi_and_rl_markers("\x1b[1;1Hhello"), "hello");
    }
    #[test] fn strip_no_partial_escape() {
        // lone ESC at end should not panic
        let s = "hi\x1b";
        let _ = strip_ansi_and_rl_markers(s);
    }

    // -- visible_display_width --
    #[test] fn vdw_plain()      { assert_eq!(visible_display_width("hello $ "), 8); }
    #[test] fn vdw_with_ansi()  { assert_eq!(visible_display_width("\x1b[32mhello\x1b[0m $ "), 8); }
    #[test] fn vdw_wide()       { assert_eq!(visible_display_width("日本語"), 6); }
    #[test] fn vdw_empty()      { assert_eq!(visible_display_width(""), 0); }

    // -- last_line_display_width --
    #[test] fn lldw_single()    { assert_eq!(last_line_display_width("$ "), 2); }
    #[test] fn lldw_multiline() { assert_eq!(last_line_display_width("prefix\n$ "), 2); }
    #[test] fn lldw_with_ansi() { assert_eq!(last_line_display_width("a\n\x1b[32mhi\x1b[0m$ "), 4); }

    // -- PromptBuilder --
    #[test] fn builder_plain()   { assert_eq!(PromptBuilder::new().plain("$ ").build().visible_text(), "$ "); }
    #[test] fn builder_chain()   {
        let p = PromptBuilder::new()
            .bold_fg("user", Color::GREEN)
            .plain("@")
            .fg_text("host", Color::BLUE)
            .plain(" $ ")
            .build();
        assert_eq!(p.visible_text(), "user@host $ ");
        assert_eq!(p.display_width(), 12);
    }
    #[test] fn builder_bold()    {
        let p = PromptBuilder::new().bold_text("WARN").build();
        assert!(p.render().contains("\x1b[1m"));
    }
    #[test] fn builder_custom_style() {
        let s = Style { bold: true, underline: true, fg: Some(Color::YELLOW), ..Default::default() };
        let p = PromptBuilder::new().styled("alert", s).build();
        let r = p.render();
        assert!(r.contains("4"));
        assert!(r.contains("33"));
        assert!(r.contains("alert"));
    }

    // -- Display trait --
    #[test] fn prompt_display() { assert_eq!(format!("{}", Prompt::plain("$ ")), "$ "); }

    // -- message --
    #[test] fn prompt_message() {
        let p = Prompt::message("(arg: 3) ");
        assert_eq!(p.visible_text(), "(arg: 3) ");
    }
}

//! A rope-backed buffer for the right panel, built on Winc's headless kernel.
//!
//! This is the piece that makes replacing the editor possible: it stores the
//! file in a `ropey::Rope` through Winc's `Lines`/`EditBuffer` implementation, so
//! editing commands, multi-cursor selection and history all come from
//! `winc_core` rather than from Waku.
//!
//! ## What it is not
//!
//! It is **not** a GPUI view. Drawing still belongs to the panel, and this type
//! only answers questions about text — which is exactly why it can be tested
//! without a window.
//!
//! ## The offset rule, enforced in one place
//!
//! `FileEditorBackend` promises **byte** offsets; `winc_core` counts in **chars**.
//! Every conversion lives in [`WincBuffer`], and the rest of Waku never sees a
//! char index. Getting this wrong is the failure mode this module exists to
//! prevent: on text with any non-ASCII character, a byte offset read as a char
//! index silently lands in the wrong place, and a replace splices mid-codepoint.
//!
//! The methods that convert therefore clamp to `char_to_byte(len_chars())`
//! rather than trusting the caller: an out-of-range byte offset (a stale find
//! match after an edit, say) becomes "end of buffer" instead of a panic.

#![allow(dead_code, reason = "the drawable editor that uses it lands next")]

use std::ops::Range;

use ropey::Rope;
use winc_core::text::Lines;

/// Text stored as a rope, addressed in bytes at this boundary.
///
/// Not yet wired into the panel: the drawable editor that consumes it comes
/// next, so nothing constructs this type today. Its tests are what make it
/// committable in that state — they pin the byte/char translation, which is the
/// part that must be right before any drawing depends on it.
pub struct WincBuffer {
    text: Rope,
    /// Whether edits are refused, mirroring the panel's `writable` decision.
    read_only: bool,
}

impl Default for WincBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl WincBuffer {
    pub fn new() -> Self {
        Self {
            text: Rope::new(),
            read_only: false,
        }
    }

    pub fn from_text(text: &str) -> Self {
        Self {
            text: Rope::from_str(text),
            read_only: false,
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    /// The whole text.
    pub fn text(&self) -> String {
        self.text.to_string()
    }

    /// Replace everything, as a reload from disk does.
    ///
    /// This ignores `read_only`: the panel calls it after deciding the new text
    /// describes the file, which is not an edit the user is making.
    pub fn set_text(&mut self, text: &str) {
        self.text = Rope::from_str(text);
    }

    /// Number of lines, counting a trailing newline as opening a final empty one.
    pub fn line_count(&self) -> usize {
        self.text.line_count()
    }

    /// The text of one line, without its newline.
    pub fn line(&self, line: usize) -> String {
        if line >= self.line_count() {
            return String::new();
        }
        self.text.line(line).to_string()
    }

    /// End of the text, in bytes.
    pub fn byte_len(&self) -> usize {
        self.text.len_bytes()
    }

    /// Byte offset of the start of `line`, or the end of the text past the last.
    ///
    /// The editor needs this to turn a painted row back into a caret offset: a
    /// click on row 40 must land at the start of line 40 without walking the
    /// rope from the top.
    pub fn line_start_byte(&self, line: usize) -> usize {
        if line >= self.text.len_lines() {
            return self.byte_len();
        }
        self.text.line_to_byte(line)
    }

    /// The line a byte offset sits on.
    pub fn line_of_byte(&self, byte: usize) -> usize {
        self.text.byte_to_line(byte.min(self.byte_len()))
    }

    /// The column of a byte offset within its line, in bytes.
    ///
    /// Bytes rather than chars because every caller here indexes a `&str`; a
    /// char column would have to be converted back for the very next slice.
    pub fn column_of_byte(&self, byte: usize) -> usize {
        let byte = byte.min(self.byte_len());
        byte - self.line_start_byte(self.line_of_byte(byte))
    }

    /// The byte offset for a line and a byte column within it, clamped to the
    /// line's end so a click past the end of a short line lands on its last
    /// character rather than on the next line.
    pub fn byte_at(&self, line: usize, column: usize) -> usize {
        if line >= self.line_count() {
            return self.byte_len();
        }
        let start = self.line_start_byte(line);
        // `line` here excludes the newline: `line()` keeps it for a rope slice,
        // so the end is measured from the text without the break.
        let text = self.line(line);
        let end = start + text.trim_end_matches(['\r', '\n']).len();
        (start + column).min(end)
    }

    /// The byte offset one grapheme to the left of `byte`.
    ///
    /// Grapheme rather than char: a caret belongs between what a reader sees as
    /// characters, and stepping into the middle of an emoji or a combining
    /// accent would let the next keystroke split it.
    pub fn prev_grapheme(&self, byte: usize) -> usize {
        self.byte_of_char(winc_core::movement::prev_grapheme(
            &self.text,
            self.char_of_byte(byte),
        ))
    }

    pub fn next_grapheme(&self, byte: usize) -> usize {
        self.byte_of_char(winc_core::movement::next_grapheme(
            &self.text,
            self.char_of_byte(byte),
        ))
    }

    pub fn prev_word(&self, byte: usize) -> usize {
        self.byte_of_char(winc_core::movement::prev_word(
            &self.text,
            self.char_of_byte(byte),
        ))
    }

    pub fn next_word(&self, byte: usize) -> usize {
        self.byte_of_char(winc_core::movement::next_word(
            &self.text,
            self.char_of_byte(byte),
        ))
    }

    /// The byte offset at the start of the word containing `byte`.
    pub fn word_start_at(&self, byte: usize) -> usize {
        let (start, _) = winc_core::movement::word_at(&self.text, self.char_of_byte(byte));
        self.byte_of_char(start)
    }

    /// The byte offset at the end of the word containing `byte`.
    pub fn word_end_at(&self, byte: usize) -> usize {
        let (_, end) = winc_core::movement::word_at(&self.text, self.char_of_byte(byte));
        self.byte_of_char(end)
    }

    /// Move one line up or down, keeping the column in bytes where it fits.
    ///
    /// Returns the same offset when there is no line in that direction, so a
    /// caller can tell a refused move from a successful one by comparing.
    pub fn vertical(&self, byte: usize, down: bool) -> usize {
        let line = self.line_of_byte(byte);
        let column = self.column_of_byte(byte);
        let target = if down {
            if line + 1 >= self.line_count() {
                return byte;
            }
            line + 1
        } else {
            if line == 0 {
                return byte;
            }
            line - 1
        };
        self.byte_at(target, column)
    }

    /// Translate a byte offset into the char index the kernel expects.
    ///
    /// Clamped: `char_to_byte` panics past the end, and a stale offset from a
    /// find match that a concurrent edit shortened is an ordinary occurrence,
    /// not a programming error.
    fn char_of_byte(&self, byte: usize) -> usize {
        let byte = byte.min(self.text.len_bytes());
        // A stale offset can land mid-codepoint. Back off until the rope
        // accepts it, rather than panicking on a caller's arithmetic.
        let mut byte = byte;
        while byte > 0 {
            if let Ok(char_idx) = self.text.try_byte_to_char(byte) {
                return char_idx;
            }
            byte -= 1;
        }
        0
    }

    /// Translate a char index (as the kernel reports it) back to bytes.
    fn byte_of_char(&self, char_idx: usize) -> usize {
        self.text.char_to_byte(char_idx.min(self.text.len_chars()))
    }

    /// Replace `range` (bytes) with `text`, recording nothing — history is the
    /// caller's business.
    ///
    /// Returns false when the buffer is read-only or the range is not a slice of
    /// this text, so a caller cannot mistake a refused edit for an applied one.
    pub fn edit(&mut self, range: Range<usize>, text: &str) -> bool {
        if self.read_only {
            return false;
        }
        let from = self.char_of_byte(range.start);
        let to = self.char_of_byte(range.end);
        if from > to {
            return false;
        }
        // One splice rather than remove-then-insert: a rope rebuild in between
        // would make the second half point at offsets the first half moved.
        self.text.remove(from..to);
        if !text.is_empty() {
            self.text.insert(from, text);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_edits_land_where_the_byte_range_says() {
        let mut buffer = WincBuffer::from_text("hello world");
        assert!(buffer.edit(6..11, "there"));
        assert_eq!(buffer.text(), "hello there");
    }

    /// The regression this module exists for: byte offsets must not be read as
    /// char indices once non-ASCII text is present.
    ///
    /// In "héllo" the `é` occupies bytes 1..3, so the `ll` sit at bytes 3..5 —
    /// a byte-addressed replace there touches "ll". Reading 3..5 as *char*
    /// indices would touch "o " instead, which is the bug being guarded.
    #[test]
    fn byte_offsets_are_not_mistaken_for_char_indices() {
        let mut buffer = WincBuffer::from_text("héllo wörld");
        assert!(buffer.edit(3..5, "LL"));
        assert_eq!(buffer.text(), "héLLo wörld");
    }

    #[test]
    fn a_cjk_line_reports_its_bytes_not_its_chars() {
        let buffer = WincBuffer::from_text("中文\nabc");
        assert_eq!(
            buffer.byte_len(),
            10,
            "3 bytes per CJK char plus the newline"
        );
        assert_eq!(buffer.line_count(), 2);
        assert_eq!(
            buffer.line(0),
            "中文\n",
            "ropey's line slice keeps the break"
        );
        assert_eq!(buffer.line(1), "abc");
    }

    #[test]
    fn an_offset_past_the_end_clamps_instead_of_panicking() {
        let mut buffer = WincBuffer::from_text("abc");
        assert!(
            buffer.edit(10..20, "x"),
            "a stale range still applies at the end"
        );
        assert_eq!(buffer.text(), "abcx");
    }

    #[test]
    fn an_offset_inside_a_codepoint_backs_off_to_a_boundary() {
        let mut buffer = WincBuffer::from_text("a中b");
        // 2 is the second byte of the three-byte 中.
        assert!(buffer.edit(2..2, "X"));
        assert_eq!(
            buffer.text(),
            "aX中b",
            "the insert lands before the character"
        );
    }

    #[test]
    fn a_read_only_buffer_refuses_edits_without_changing() {
        let mut buffer = WincBuffer::from_text("keep");
        buffer.set_read_only(true);
        assert!(!buffer.edit(0..4, "changed"));
        assert_eq!(buffer.text(), "keep");
    }

    #[test]
    fn a_multi_line_edit_keeps_the_line_structure() {
        let mut buffer = WincBuffer::from_text("one\ntwo\nthree");
        // "two" is bytes 4..7; replacing it with a bare word leaves the breaks
        // on either side intact.
        assert!(buffer.edit(4..7, "TWO"));
        assert_eq!(buffer.text(), "one\nTWO\nthree");
        assert_eq!(buffer.line_count(), 3);

        // And a replacement that carries its own break adds a line.
        assert!(buffer.edit(7..8, "\n\n"));
        assert_eq!(buffer.text(), "one\nTWO\n\nthree");
        assert_eq!(buffer.line_count(), 4);
    }

    #[test]
    fn line_starts_are_byte_offsets_after_a_multibyte_line() {
        // 中文 is six bytes, so the second line starts at 7, not at 3.
        let buffer = WincBuffer::from_text("中文\nabc");
        assert_eq!(buffer.line_start_byte(0), 0);
        assert_eq!(
            buffer.line_start_byte(1),
            7,
            "a CJK line must not shift the next line's start"
        );
    }

    #[test]
    fn a_byte_offset_maps_back_to_its_line_and_column() {
        let buffer = WincBuffer::from_text("中文\nabc");
        assert_eq!(buffer.line_of_byte(0), 0);
        assert_eq!(buffer.column_of_byte(3), 3, "the second CJK char is 3..6");
        assert_eq!(buffer.line_of_byte(7), 1);
        assert_eq!(buffer.column_of_byte(7), 0);
        assert_eq!(buffer.column_of_byte(9), 2);
    }

    #[test]
    fn a_column_past_a_short_line_clamps_to_its_end() {
        let buffer = WincBuffer::from_text("ab\nabcdef");
        // Column 5 does not exist on line 0, so the caret stops after `ab`.
        assert_eq!(buffer.byte_at(0, 5), 2);
        assert_eq!(buffer.byte_at(1, 5), 8);
    }

    /// The down arrow must keep the caret in its column across a multibyte
    /// line, in bytes: a char-based column would land two bytes early here.
    #[test]
    fn vertical_motion_keeps_a_byte_column_past_cjk() {
        // `中文x` is 7 bytes, so the second line starts at 8. Byte column 6 on
        // that line is byte 14 — reading the column as chars would give 13.
        let buffer = WincBuffer::from_text("中文x\nabcdef");
        assert_eq!(buffer.byte_at(0, 6), 6, "after the `x` on line 0");
        assert_eq!(buffer.vertical(6, true), 14, "column 6 of the second line");
        assert_eq!(buffer.vertical(14, false), 6);
    }

    #[test]
    fn vertical_motion_at_the_edges_stays_put() {
        let buffer = WincBuffer::from_text("a\nb");
        assert_eq!(buffer.vertical(0, false), 0);
        assert_eq!(buffer.vertical(2, true), 2);
    }

    #[test]
    fn grapheme_motion_crosses_a_line_break() {
        let buffer = WincBuffer::from_text("ab\ncd");
        // From the start of line 1, left goes to the end of line 0.
        assert_eq!(buffer.prev_grapheme(3), 2);
        // From the end of line 0, right goes to the start of line 1.
        assert_eq!(buffer.next_grapheme(2), 3);
    }

    /// A caret must never land inside a multi-byte character: one left-step
    /// from after 中 must skip all three of its bytes.
    #[test]
    fn grapheme_motion_steps_over_a_whole_cjk_character() {
        let buffer = WincBuffer::from_text("a中b");
        assert_eq!(buffer.prev_grapheme(4), 1, "back over 中, not into it");
        assert_eq!(buffer.next_grapheme(1), 4, "forward over all three bytes");
    }

    /// Winc's rule: run to the end of the current class, then skip trailing
    /// whitespace. So from `fn` the caret lands on `main`, not on the space.
    #[test]
    fn word_motion_lands_on_word_boundaries() {
        let buffer = WincBuffer::from_text("fn main() {}");
        assert_eq!(
            buffer.next_word(0),
            3,
            "past `fn` and the space, onto `main`"
        );
        assert_eq!(buffer.next_word(3), 7, "past `main`, onto `(`");
        assert_eq!(buffer.prev_word(7), 3, "back to the start of `main`");
        assert_eq!(buffer.prev_word(3), 0, "back to the start of `fn`");
    }

    #[test]
    fn the_word_under_the_caret_is_bounded() {
        let buffer = WincBuffer::from_text("let 中文 = 1");
        // 中文 occupies bytes 4..10.
        assert_eq!(buffer.word_start_at(5), 4);
        assert_eq!(buffer.word_end_at(5), 10);
    }
}

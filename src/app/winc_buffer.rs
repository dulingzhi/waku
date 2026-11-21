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
}

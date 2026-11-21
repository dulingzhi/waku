//! The right panel's file buffer: Winc's own `Document`, addressed in bytes.
//!
//! This is a thin adapter, not an editor. Everything that makes an editor —
//! selection, undo history, folding, multi-cursor, auto-indent, bracket
//! matching, line ending and encoding detection — lives in
//! [`winc_editor::Document`], and Waku drives it rather than reimplementing it.
//!
//! ## Why an adapter at all
//!
//! `Document` counts positions in **chars**; [`super::file_editor`] promises
//! **bytes**, because the panel and its find bar slice `&str`. Both units are
//! legitimate and neither converts cleanly, so the translation happens here and
//! nowhere else: the rest of Waku never sees a char index.
//!
//! This is the failure mode the module exists to prevent. On text with any
//! non-ASCII character, a byte offset read as a char index silently lands in the
//! wrong place, and a replace splices into the middle of a code point — which
//! corrupts a Chinese file without any error.
//!
//! ## What it deliberately does not do
//!
//! It never touches the filesystem. The panel decides when a read is stale and
//! when a save may overwrite, so a buffer that also read the disk would race
//! those decisions from underneath.

use std::ops::Range;

use ropey::Rope;
use winc_core::Selection;
use winc_core::text::Lines;
use winc_editor::Command;
use winc_editor::Document;

/// Winc's document, addressed in bytes at this boundary.
pub struct WincBuffer {
    document: Document,
    /// The tab width Winc's commands indent and outdent by.
    tab_width: usize,
    /// The file's name, kept so a rebuild can restore the language Winc picked.
    file_name: String,
    /// Whether the file this buffer holds uses CRLF breaks.
    crlf: bool,
}

impl Default for WincBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl WincBuffer {
    pub fn new() -> Self {
        Self {
            document: Document::from_text(next_document_id(), ""),
            tab_width: 4,
            file_name: String::new(),
            crlf: false,
        }
    }

    /// Build a buffer from text, as an in-memory source does.
    ///
    /// The name is what picks the language, so `"a.rs"` and `"a.txt"` with the
    /// same text are different buffers: one can toggle a comment, the other
    /// cannot.
    #[allow(
        dead_code,
        reason = "the constructor tests build buffers with; the panel sets text after a read"
    )]
    pub fn from_text(file_name: &str, text: &str) -> Self {
        let mut buffer = Self::new();
        buffer.set_file_name(file_name);
        buffer.set_text(text);
        buffer
    }

    /// The whole text.
    pub fn text(&self) -> String {
        let content = self.document.text();
        match content.rope() {
            Some(rope) => rope.to_string(),
            // A large document keeps its text on disk and only its edits in
            // memory; this materialises it, which is why the panel asks for it
            // only when saving or comparing against the disk baseline.
            None => content.substring(0, content.end_pos()),
        }
    }

    /// Replace everything, as a reload from disk does.
    ///
    /// Winc's document keeps an undo history, so replacing the text is not just
    /// a store: the history behind it no longer describes anything, and a
    /// Ctrl+Z after a reload would step into a file that is no longer there.
    /// The document is therefore rebuilt rather than mutated.
    ///
    /// The file name is what tells Winc which language it is looking at, and
    /// that decides the comment prefix, the indentation rules and the lexer. A
    /// document told nothing highlights as plain text and toggles comments to
    /// nothing, so the name is kept here and reapplied on every rebuild.
    ///
    /// Breaks are normalised to LF on the way in. Winc's `Newline` and its
    /// line arithmetic assume `\n`, so a stray `\r` would be counted as a column
    /// and painted as a cell; the file's own convention is remembered and put
    /// back by [`Self::text_for_save`].
    pub fn set_text(&mut self, text: &str) {
        self.crlf = text.contains("\r\n");
        let normalised = if self.crlf {
            text.replace("\r\n", "\n")
        } else {
            text.to_owned()
        };
        let file_name = self.file_name.clone();
        self.document = Document::from_text(next_document_id(), &normalised);
        self.document.set_language(&file_name);
    }

    /// Tell the buffer which file it holds, so Winc can pick a language.
    ///
    /// Called before the text arrives: the panel knows the path from its tab
    /// long before the read completes.
    pub fn set_file_name(&mut self, file_name: &str) {
        self.file_name = file_name.to_owned();
        self.document.set_language(file_name);
    }

    /// The text with the file's own line endings, for saving.
    ///
    /// Writing a CRLF file back as LF would rewrite every line of it. The editor
    /// normalises in memory, which is what the lexer and the caret arithmetic
    /// expect, so the conversion back happens here and only here.
    pub fn text_for_save(&self) -> String {
        let text = self.text();
        if self.crlf {
            // Collapse first so a mixed file cannot become `\r\r\n`.
            text.replace("\r\n", "\n").replace('\n', "\r\n")
        } else {
            text
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.document.is_locked()
    }

    pub fn set_read_only(&mut self, read_only: bool) {
        self.document.set_locked(read_only);
    }

    // ---- Lines, in bytes -------------------------------------------------

    /// Number of lines, counting a trailing newline as opening a final empty one.
    pub fn line_count(&self) -> usize {
        self.document.text().line_count()
    }

    /// The text of one line, exactly as the rope slices it.
    ///
    /// Winc's `Lines` keeps the line break in a rope slice, which is what the
    /// panel's callers already expect: `winc_paint` trims it before painting.
    pub fn line(&self, line: usize) -> String {
        if line >= self.line_count() {
            return String::new();
        }
        self.document.text().line_content(line).slice().to_string()
    }

    /// End of the text, in bytes.
    pub fn byte_len(&self) -> usize {
        match self.document.text().rope() {
            Some(rope) => rope.len_bytes(),
            // A large document reads its text on demand; measuring it costs one
            // pass, and the panel only asks when sizing its scroll content.
            None => self.text().len(),
        }
    }

    /// Byte offset of the start of `line`, or the end of the text past the last.
    pub fn line_start_byte(&self, line: usize) -> usize {
        if line >= self.line_count() {
            return self.byte_len();
        }
        let char_start = self.document.text().line_start(line);
        self.byte_of_char(char_start)
    }

    /// The line a byte offset sits on.
    pub fn line_of_byte(&self, byte: usize) -> usize {
        self.document.text().line_of(self.char_of_byte(byte))
    }

    /// The column of a byte offset within its line, in bytes.
    ///
    /// Not called today: the renderer asks the shaped line for a byte's x, so
    /// it never needs the column back. Kept because it is the inverse of
    /// `byte_at` and the two are what make a caret round-trip across a click.
    #[allow(
        dead_code,
        reason = "the inverse of `byte_at`; the renderer solves this by shaping"
    )]
    pub fn column_of_byte(&self, byte: usize) -> usize {
        let byte = byte.min(self.byte_len());
        byte - self.line_start_byte(self.line_of_byte(byte))
    }

    /// The byte offset for a line and a byte column within it, clamped to the
    /// line's end so a click past a short line lands on its last character.
    #[allow(dead_code, reason = "see `column_of_byte`")]
    pub fn byte_at(&self, line: usize, column: usize) -> usize {
        if line >= self.line_count() {
            return self.byte_len();
        }
        let start = self.line_start_byte(line);
        let text = self.line(line);
        let end = start + text.trim_end_matches(['\r', '\n']).len();
        (start + column).min(end)
    }

    // ---- Unit conversion, the only place chars appear ---------------------

    /// Translate a byte offset into the char index the kernel expects.
    ///
    /// Clamped, and backed off to a character boundary: a stale offset from a
    /// find match that a concurrent edit shortened is an ordinary occurrence,
    /// not a programming error.
    pub fn char_of_byte(&self, byte: usize) -> usize {
        match self.document.text().rope() {
            Some(rope) => {
                let byte = byte.min(rope.len_bytes());
                let mut byte = byte;
                while byte > 0 {
                    if let Ok(char_idx) = rope.try_byte_to_char(byte) {
                        return char_idx;
                    }
                    byte -= 1;
                }
                0
            }
            // A large document has no rope to convert against; its char index
            // is derived by walking the text, which is what `pos_to_line_col`
            // would do anyway.
            None => {
                let text = self.text();
                let byte = byte.min(text.len());
                let mut boundary = byte;
                while boundary > 0 && !text.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                text[..boundary].chars().count()
            }
        }
    }

    /// Translate a char index back to bytes.
    pub fn byte_of_char(&self, char_idx: usize) -> usize {
        match self.document.text().rope() {
            Some(rope) => rope.char_to_byte(char_idx.min(rope.len_chars())),
            None => {
                let text = self.text();
                text.char_indices()
                    .nth(char_idx)
                    .map_or(text.len(), |(byte, _)| byte)
            }
        }
    }

    // ---- Editing, through Winc's own commands -----------------------------

    /// Run one of Winc's commands.
    ///
    /// This is the whole editing surface: typing is `Command::Insert`, Enter is
    /// `Command::Newline`, indent, comment toggling, line moves and undo all go
    /// through here, so their behaviour is exactly Winc's.
    pub fn execute(&mut self, command: Command, page_lines: usize) {
        self.document
            .execute(command, page_lines.max(1), self.tab_width);
    }

    /// Replace `range` (bytes) with `text`, as a find-bar replace does.
    ///
    /// Returns false when the buffer refuses the edit, so a caller cannot
    /// mistake a refused edit for an applied one.
    ///
    /// The splice goes through Winc's `Insert`, which replaces a selection, so
    /// it lands as one undo step in the document's own history. Deleting is
    /// `Insert("")` rather than Winc's `Delete`, because `Delete` is
    /// forward-delete-one-grapheme and would remove a character instead of the
    /// range.
    pub fn edit(&mut self, range: Range<usize>, text: &str) -> bool {
        if self.is_read_only() {
            return false;
        }
        if range.start > range.end {
            return false;
        }
        let len = self.byte_len();
        let range = range.start.min(len)..range.end.min(len);
        let from = self.char_of_byte(range.start);
        let to = self.char_of_byte(range.end);
        if from > to {
            return false;
        }
        self.document.selection = Selection::new(from, to);
        self.execute(Command::Insert(text.to_owned()), 1);
        true
    }

    // ---- Selection, in bytes ---------------------------------------------

    /// The selection as a byte range, ordered low to high.
    pub fn selected_range(&self) -> Range<usize> {
        let selection = self.document.selection;
        self.byte_of_char(selection.from())..self.byte_of_char(selection.to())
    }

    /// The caret's byte offset.
    pub fn cursor(&self) -> usize {
        self.byte_of_char(self.document.selection.head)
    }

    /// Whether the selection was made backwards, so the caret is at its start.
    pub fn selection_reversed(&self) -> bool {
        self.document.selection.head < self.document.selection.anchor
    }

    /// Select a byte range. Refused unless both ends sit on character
    /// boundaries, so a selection computed against stale text cannot split a
    /// code point.
    pub fn select_bytes(&mut self, range: Range<usize>, cx_reversed: bool) -> bool {
        let len = self.byte_len();
        if range.start > range.end || range.end > len {
            return false;
        }
        let text = self.text();
        if !text.is_char_boundary(range.start) || !text.is_char_boundary(range.end) {
            return false;
        }
        let from = self.char_of_byte(range.start);
        let to = self.char_of_byte(range.end);
        self.document.selection = if cx_reversed {
            Selection::new(to, from)
        } else {
            Selection::new(from, to)
        };
        true
    }

    /// A line and a char column, as a byte offset.
    pub fn byte_at_line_col(&self, line: usize, col: usize) -> usize {
        if line >= self.line_count() {
            return self.byte_len();
        }
        let char_start = self.document.text().line_start(line);
        self.byte_of_char(char_start + col)
    }

    /// The language Winc would highlight this document with.
    pub fn language(&self) -> &'static winc_syntax::Language {
        self.document.highlighter.language()
    }

    /// The byte range of the word at `byte`, as a double click or Ctrl+arrow
    /// would select it.
    ///
    /// Winc's `word_at` defines what a word is, so this agrees with the word
    /// motion its commands use. A position just past the end of a word selects
    /// that word rather than the punctuation after it, which is what a user
    /// double-clicking the tail of a name expects.
    pub fn word_at(&self, byte: usize) -> Range<usize> {
        let content = self.document.text();
        let (start, end) = winc_core::movement::word_at(content, self.char_of_byte(byte));
        self.byte_of_char(start)..self.byte_of_char(end)
    }

    /// The identifier the caret sits on or just after, as a byte range.
    ///
    /// Navigation is asked about "the name under the cursor", so the caret at
    /// the end of `foo` must still yield `foo`: that is how every editor's
    /// go-to-definition behaves, and `word_at` already has the rule.
    pub fn name_under_caret(&self) -> Option<(String, Range<usize>)> {
        let range = self.word_at(self.cursor());
        let text = self.text();
        let name = text.get(range.clone())?.to_owned();
        // A word of only punctuation is not a name to look up.
        if !name.chars().any(|c| c.is_alphanumeric() || c == '_') {
            return None;
        }
        Some((name, range))
    }

    /// The text of the whole document as a rope, for Winc's navigation scans.
    ///
    /// Those take a `Rope` rather than the `Lines` interface, and a large
    /// document has no rope — so it is materialised here. A large file is
    /// exactly where a linear scan is least wanted, which is why the callers
    /// bound the work rather than relying on the rope being cheap.
    pub fn rope(&self) -> Option<Rope> {
        self.document.text().rope().cloned()
    }
}

/// Document ids are Winc's; Waku opens one document per file.
///
/// `Document` keys its caches by `(id, revision)`, so two live documents must
/// never share an id: doing so would let one file's cached folds or syntax state
/// answer for another and paint the wrong colours. The counter guarantees that
/// for the life of the process.
fn next_document_id() -> winc_editor::DocumentId {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    winc_editor::DocumentId(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use winc_editor::Command;

    fn rust(text: &str) -> WincBuffer {
        WincBuffer::from_text("a.rs", text)
    }

    /// A plain buffer: no language, no comment prefix.
    fn plain(text: &str) -> WincBuffer {
        WincBuffer::from_text("a.txt", text)
    }

    #[test]
    fn ascii_edits_land_where_the_byte_range_says() {
        let mut buffer = plain("hello world");
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
        let mut buffer = plain("héllo wörld");
        assert!(buffer.edit(3..5, "LL"));
        assert_eq!(buffer.text(), "héLLo wörld");
    }

    #[test]
    fn a_cjk_line_reports_its_bytes_not_its_chars() {
        let buffer = plain("中文\nabc");
        assert_eq!(
            buffer.byte_len(),
            10,
            "3 bytes per CJK char plus the newline"
        );
        assert_eq!(buffer.line_count(), 2);
        assert_eq!(buffer.line(1), "abc");
    }

    #[test]
    fn an_offset_past_the_end_clamps_instead_of_panicking() {
        let mut buffer = plain("abc");
        assert!(
            buffer.edit(10..20, "x"),
            "a stale range still applies at the end"
        );
        assert_eq!(buffer.text(), "abcx");
    }

    #[test]
    fn an_offset_inside_a_codepoint_backs_off_to_a_boundary() {
        let mut buffer = plain("a中b");
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
        let mut buffer = plain("keep");
        buffer.set_read_only(true);
        assert!(!buffer.edit(0..4, "changed"));
        assert_eq!(buffer.text(), "keep");
    }

    #[test]
    fn a_multi_line_edit_keeps_the_line_structure() {
        let mut buffer = plain("one\ntwo\nthree");
        // "two" is bytes 4..7.
        assert!(buffer.edit(4..7, "TWO"));
        assert_eq!(buffer.text(), "one\nTWO\nthree");
        assert_eq!(buffer.line_count(), 3);

        // A replacement that carries its own break adds a line.
        assert!(buffer.edit(7..8, "\n\n"));
        assert_eq!(buffer.line_count(), 4);
    }

    #[test]
    fn line_starts_are_byte_offsets_after_a_multibyte_line() {
        // 中文 is six bytes, so the second line starts at 7, not at 3.
        let buffer = plain("中文\nabc");
        assert_eq!(buffer.line_start_byte(0), 0);
        assert_eq!(
            buffer.line_start_byte(1),
            7,
            "a CJK line must not shift the next line's start"
        );
    }

    #[test]
    fn a_byte_offset_maps_back_to_its_line_and_column() {
        let buffer = plain("中文\nabc");
        assert_eq!(buffer.line_of_byte(0), 0);
        assert_eq!(buffer.column_of_byte(3), 3, "the second CJK char is 3..6");
        assert_eq!(buffer.line_of_byte(7), 1);
        assert_eq!(buffer.column_of_byte(7), 0);
    }

    #[test]
    fn a_column_past_a_short_line_clamps_to_its_end() {
        let buffer = plain("ab\nabcdef");
        assert_eq!(buffer.byte_at(0, 5), 2, "column 5 does not exist on line 0");
        assert_eq!(buffer.byte_at(1, 5), 8);
    }

    /// A double click and Ctrl+arrow must agree about what a word is, because
    /// both ask Winc. This pins that the byte range quotes the whole CJK word
    /// rather than a slice of it.
    #[test]
    fn the_word_under_the_caret_is_bounded() {
        let buffer = plain("let 中文 = 1");
        let range = buffer.word_at(5);
        assert_eq!(
            &buffer.text()[range],
            "中文",
            "the word spans all six of its bytes"
        );
    }

    #[test]
    fn a_punctuation_only_caret_has_no_name() {
        let mut buffer = plain("a + b");
        buffer.select_bytes(2..2, false);
        assert_eq!(
            buffer.name_under_caret(),
            None,
            "`+` is not something to look up"
        );
    }

    /// The file name decides the language, and the language decides everything
    /// from the lexer to the comment prefix. A buffer that forgot its name would
    /// silently toggle comments to nothing.
    #[test]
    fn the_file_name_decides_the_language() {
        assert_eq!(rust("fn main() {}").language().id, "rust");
        assert_ne!(
            plain("fn main() {}").language().id,
            "rust",
            "a .txt file is not Rust"
        );
    }

    /// Winc's toggle-comment needs a language that has a comment prefix, which
    /// is why it is the strongest check that the document knows what it is.
    #[test]
    fn toggle_comment_uses_the_files_own_prefix() {
        let mut buffer = rust("fn main() {}\n");
        buffer.execute(Command::ToggleComment, 1);
        assert!(
            buffer.text().trim_start().starts_with("//"),
            "got {:?}",
            buffer.text()
        );
    }

    /// A CRLF file must be written back with its own breaks: converting it to
    /// LF would mark every line as changed in git.
    ///
    /// The text is normalised on the way in, so reading it back as CRLF is
    /// itself a check that the normalisation happened — a buffer that kept the
    /// `\r` in place would pass the save round-trip while carrying a stray byte
    /// into every offset it reports.
    #[test]
    fn a_crlf_file_is_normalised_in_memory_and_restored_on_save() {
        let buffer = plain("one\r\ntwo\r\n");
        assert_eq!(
            buffer.text(),
            "one\ntwo\n",
            "the editor works on LF so no `\r` becomes a stray column"
        );
        assert_eq!(
            buffer.text_for_save(),
            "one\r\ntwo\r\n",
            "saving puts the file's own breaks back"
        );
    }

    #[test]
    fn an_lf_file_is_not_converted_on_save() {
        let buffer = plain("one\ntwo\n");
        assert_eq!(buffer.text(), "one\ntwo\n");
        assert_eq!(buffer.text_for_save(), "one\ntwo\n");
    }

    /// A byte offset reported against a CRLF file must not count the carriage
    /// return that the file has but the editor does not.
    #[test]
    fn a_crlf_file_reports_offsets_without_its_carriage_returns() {
        let buffer = plain("a\r\nb\r\n");
        assert_eq!(buffer.byte_len(), 4, "four bytes after normalisation");
        assert_eq!(buffer.line_start_byte(1), 2);
    }

    /// Replacing the text rebuilds the document, so the undo stack that
    /// described the old text must not survive into the new one.
    #[test]
    fn replacing_the_text_forgets_the_old_undo_history() {
        let mut buffer = plain("abc");
        buffer.execute(Command::Insert("X".to_owned()), 1);
        assert_eq!(buffer.text(), "Xabc");

        buffer.set_text("fresh");
        buffer.execute(Command::Undo, 1);
        assert_eq!(
            buffer.text(),
            "fresh",
            "an undo after a reload has nothing to step back into"
        );
    }
}

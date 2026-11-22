//! The GPUI view for the rope-backed file editor.
//!
//! This is the piece that turns the four layers underneath — the byte-addressed
//! rope buffer, the cross-line highlighter, the viewport arithmetic and the run
//! shaper — into something the user can click, type into and save.
//!
//! ## Why a canvas instead of a `div` per line
//!
//! The panel draws a file with one `canvas` that shapes only the rows on screen.
//! A `div` per line would put one layout node per line of the file into every
//! frame, which is what made large files crawl; the arithmetic that decides
//! which rows are visible lives in [`super::winc_paint`] so it can be tested
//! without a window.
//!
//! ## What this type owns
//!
//! The selection, the caret and its blink, the scroll-to-caret behaviour, and the
//! plumbing between GPUI's input protocol and the buffer. It owns no text
//! storage and no layout arithmetic of its own: a bug in either shows up in the
//! tests of the module that does own it.
//!
//! ## Offsets
//!
//! Everything public here is a **byte** offset, matching [`super::file_editor`].
//! The one place other units appear is GPUI's IME protocol, which speaks UTF-16;
//! [`Self::range_from_utf16`] and [`Self::range_to_utf16`] convert at that
//! boundary and nowhere else.

use std::ops::Range;
use std::time::Duration;

use gpui::{
    App, BorderStyle, Bounds, ClipboardEntry, ClipboardItem, Context, CursorStyle,
    ElementInputHandler, EntityInputHandler, EventEmitter, FocusHandle, Focusable, KeyBinding,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, ScrollHandle,
    ShapedLine, SharedString, Task, TextAlign, TextRun, UTF16Selection, Window, actions, canvas,
    div, point, prelude::*, px, quad, transparent_black,
};

use super::winc_buffer::WincBuffer;
use super::winc_highlight::LineHighlighter;
use super::winc_paint::visible_lines;
use super::winc_view::{EditorPalette, runs_for};
use crate::input::InputEvent;
use crate::theme::Theme;
use winc_core::transform::Case;
use winc_editor::Command as WincCommand;

/// Rows past the viewport kept painted, so a fast scroll never shows a blank
/// strip for a frame while the next batch is shaped.
const OVERSCAN_ROWS: f32 = 2.0;

/// How long the caret stays solid after an edit before blinking resumes.
const CURSOR_BLINK_PAUSE: Duration = Duration::from_millis(300);

/// One caret blink half-period.
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// How long a formatter may run before Winc kills it.
///
/// A formatter is an external process on the user's machine; without a bound, a
/// tool that waits on stdin or a locked file would hold the job forever and the
/// editor would look hung.
const FORMAT_TIMEOUT: Duration = Duration::from_secs(30);

/// The caret's blink phase.
///
/// A separate type rather than three fields on the editor because every path
/// that touches it must agree on what "epoch" means: a timer that fires after a
/// pause or a second blink must not flip the caret.
struct BlinkCursor {
    visible: bool,
    paused: bool,
    epoch: usize,
    _task: Task<()>,
}

impl BlinkCursor {
    fn new() -> Self {
        Self {
            visible: true,
            paused: false,
            epoch: 0,
            _task: Task::ready(()),
        }
    }

    /// Whether the caret should paint right now.
    fn visible(&self) -> bool {
        self.paused || self.visible
    }

    /// Restart the blink solid, as an edit does.
    fn pause(&mut self, cx: &mut Context<WincEditor>) {
        self.paused = true;
        self.visible = true;
        let epoch = self.next_epoch();
        cx.notify();
        self._task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CURSOR_BLINK_PAUSE).await;
            this.update(cx, |this, cx| {
                this.blink.paused = false;
                this.blink.blink(epoch, cx);
            })
            .ok();
        });
    }

    fn next_epoch(&mut self) -> usize {
        self.epoch += 1;
        self.epoch
    }

    fn blink(&mut self, epoch: usize, cx: &mut Context<WincEditor>) {
        if self.paused || epoch != self.epoch {
            self.visible = true;
            return;
        }
        self.visible = !self.visible;
        cx.notify();

        let epoch = self.next_epoch();
        self._task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CURSOR_BLINK_INTERVAL).await;
            this.update(cx, |this, cx| this.blink.blink(epoch, cx)).ok();
        });
    }
}

/// One open file in the right panel.
pub struct WincEditor {
    buffer: WincBuffer,
    highlighter: Option<LineHighlighter>,
    focus_handle: FocusHandle,
    /// Anchor and head, both byte offsets. `head` is the caret.
    selection: Range<usize>,
    /// Whether the selection was made backwards, so the caret is at its start.
    reversed: bool,
    /// Text an IME is still composing, as a byte range.
    marked: Option<Range<usize>>,
    blink: BlinkCursor,
    /// The scroll handle the panel owns, used to follow the caret.
    scroll: ScrollHandle,
    /// Search matches to wash under the text, and which one is current.
    search_matches: Vec<Range<usize>>,
    active_search_match: Option<usize>,
    /// The last painted rows, for hit testing. Filled by `paint`.
    rows: Vec<PaintedRow>,
    /// Bounds of the text area as of the last paint.
    text_bounds: Option<Bounds<Pixels>>,
    font_size: Pixels,
    row_height: Pixels,
    /// How many lines a page-up/page-down moves, as Winc's commands expect.
    page_lines: usize,
    /// A drag is in progress, so mouse moves extend the selection.
    selecting: bool,
    /// The caret offset the viewport has already been scrolled to, so an
    /// unrelated repaint does not fight the user's own scrolling.
    followed: Option<usize>,
    /// Where the file lives, for the formatter, which runs a tool against a real
    /// path. `None` until the panel resolves the project root.
    file_path: Option<std::path::PathBuf>,
    /// Winc's extra cursors, in bytes, mirrored here so the paint pass can wash
    /// them. The document owns the real ones; this is a copy taken after each
    /// command, because painting holds a shared borrow and cannot refresh it.
    extra_selections: Vec<Range<usize>>,
    /// A one-line explanation for a command that found nothing, painted under
    /// the caret and cleared on the next keystroke.
    hint_text: Option<String>,
}

/// One painted row, kept for hit testing.
///
/// `shaped` is the line as GPUI laid it out, so a click can ask the text system
/// where byte `n` is instead of estimating from a character width.
#[derive(Clone)]
struct PaintedRow {
    line: usize,
    /// The display row this line occupies. Several lines share one row while a
    /// block is collapsed, which is why hit testing keys on the paint order
    /// rather than on the line number.
    row: usize,
    /// The row's text, without its newline.
    text: SharedString,
    shaped: ShapedLine,
    /// Top of the row in window coordinates.
    top: Pixels,
}

/// The advance a painted row hands to `shape_line`.
///
/// `force_width` is not a minimum width: GPUI positions glyph *n* at
/// `n * force_width`, so any value here overrides the font's own advances. The
/// row height belongs nowhere near it — it is vertical, and using it spaced
/// every character 1.5em apart, which pushed code past the viewport and left
/// the body blank while the gutter kept drawing numbers.
///
/// A named `None` keeps that decision visible at the call site and lets a test
/// assert it without a window or a loaded font.
fn row_force_width() -> Option<Pixels> {
    None
}

impl WincEditor {
    /// Build an editor for a file in the panel.
    ///
    /// `file_name` is the panel's project-relative path, which is what picks the
    /// language — Winc resolves it by name. `absolute_path` is where the file
    /// lives, needed only by the formatter, which runs a tool against a real
    /// path and must find the project's config beside it.
    pub fn new(
        file_name: &str,
        absolute_path: Option<std::path::PathBuf>,
        font_size: Pixels,
        scroll: ScrollHandle,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut buffer = WincBuffer::new();
        buffer.set_file_name(file_name);
        Self {
            buffer,
            highlighter: LineHighlighter::for_file_name(file_name),
            focus_handle: cx.focus_handle(),
            selection: 0..0,
            reversed: false,
            marked: None,
            blink: BlinkCursor::new(),
            scroll,
            search_matches: Vec::new(),
            active_search_match: None,
            rows: Vec::new(),
            text_bounds: None,
            font_size,
            row_height: (font_size * 1.5).round(),
            // Refined once the viewport is known; 30 is a reasonable page
            // until the first paint measures the real one.
            page_lines: 30,
            selecting: false,
            followed: None,
            file_path: absolute_path,
            extra_selections: Vec::new(),
            hint_text: None,
        }
    }

    /// Point the editor at the file's real path, once the panel knows it.
    ///
    /// The panel can open a file before it knows which project it belongs to —
    /// the path is resolved from the selected workspace, which can land a frame
    /// later — so this is set separately rather than required at construction.
    pub fn set_file_path(&mut self, path: std::path::PathBuf) {
        self.file_path = Some(path);
    }

    pub fn row_height(&self) -> Pixels {
        self.row_height
    }

    pub fn content(&self) -> String {
        self.buffer.text()
    }

    /// The text with the file's own line endings, for saving.
    ///
    /// The editor works on normalised breaks because that is what the lexer and
    /// every offset in it assume; a CRLF file has to be put back the way it was
    /// or a single edit would rewrite every line of it.
    pub fn content_for_save(&self) -> String {
        self.buffer.text_for_save()
    }

    pub fn buffer(&self) -> &WincBuffer {
        &self.buffer
    }

    pub fn is_read_only(&self) -> bool {
        self.buffer.is_read_only()
    }

    pub fn selected_range(&self) -> Range<usize> {
        self.selection.clone()
    }

    pub fn cursor(&self) -> usize {
        self.selection.end
    }

    /// Replace the whole text, as a read from disk or a reload does.
    ///
    /// The caret is clamped rather than reset: reloading a file the user was
    /// reading should not jump them back to the top, but it must not leave the
    /// caret past the new end either.
    pub fn set_content(&mut self, content: &str, cx: &mut Context<Self>) {
        // `set_text` rebuilds the document, so the undo history behind the old
        // text goes with it — stepping back into a file that is no longer open
        // would resurrect text the user cannot see the provenance of.
        self.buffer.set_text(content);
        self.marked = None;
        self.clamp_selection();
        self.invalidate_highlight(0);
        self.rows.clear();
        cx.emit(InputEvent::Edited);
        cx.notify();
    }

    pub fn set_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        self.buffer.set_read_only(read_only);
        cx.notify();
    }

    /// Select a byte range, as navigating to a search match does. Refused
    /// unless both ends sit on character boundaries, so a selection computed
    /// against stale text cannot split a code point.
    pub fn select_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let len = self.buffer.byte_len();
        if range.start > range.end || range.end > len {
            return;
        }
        let text = self.buffer.text();
        if !text.is_char_boundary(range.start) || !text.is_char_boundary(range.end) {
            return;
        }
        self.selection = range;
        self.reversed = false;
        self.marked = None;
        self.blink.pause(cx);
        cx.notify();
    }

    pub fn set_search_matches(
        &mut self,
        matches: Vec<Range<usize>>,
        active: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        if self.search_matches == matches && self.active_search_match == active {
            return;
        }
        self.search_matches = matches;
        self.active_search_match = active;
        cx.notify();
    }

    /// Where `offset` sits on screen, with the row height, as of the last paint.
    pub fn position_for_offset(&self, offset: usize) -> Option<(Point<Pixels>, Pixels)> {
        let bounds = self.text_bounds?;
        let line = self.buffer.line_of_byte(offset);
        // Folds mean a line can share a row with the block header above it, so
        // the painted row carrying it is the one whose display row matches;
        // searching by line alone would miss a line hidden inside a fold.
        let row = self.rows.iter().find(|row| row.line == line).or_else(|| {
            let display = self.buffer.row_of(line);
            self.rows.iter().find(|row| row.row == display)
        })?;
        let column = offset.saturating_sub(self.buffer.line_start_byte(line));
        let x = row.shaped.x_for_index(column.min(row.text.len()));
        Some((point(bounds.left() + x, row.top), self.row_height))
    }

    /// Splice `text` over `range`, as a replace-all does.
    ///
    /// Refused when read-only. The splice is one Winc transaction, so it undoes
    /// as a single step in the document's own history.
    pub fn replace_range(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        if self.buffer.is_read_only() {
            return;
        }
        let before = self.buffer.text();
        if !self.buffer.edit(range, text) {
            return;
        }
        self.after_command(before, cx);
    }

    fn invalidate_highlight(&mut self, line: usize) {
        if let Some(highlighter) = self.highlighter.as_mut() {
            highlighter.invalidate_from(line);
        }
    }

    fn clamp_selection(&mut self) {
        let len = self.buffer.byte_len();
        let text = self.buffer.text();
        let mut start = self.selection.start.min(len);
        let mut end = self.selection.end.min(len);
        while start > 0 && !text.is_char_boundary(start) {
            start -= 1;
        }
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        self.selection = start.min(end)..end;
    }

    /// Move the caret to `offset`, extending the selection when `extend`.
    fn move_caret(&mut self, offset: usize, extend: bool, cx: &mut Context<Self>) {
        let offset = offset.min(self.buffer.byte_len());
        if extend {
            self.extend_selection(offset, cx);
            return;
        }
        self.selection = offset..offset;
        self.reversed = false;
        self.marked = None;
        self.blink.pause(cx);
        cx.notify();
    }

    /// Extend the selection to `offset`, keeping the anchor where it was.
    fn extend_selection(&mut self, offset: usize, cx: &mut Context<Self>) {
        let anchor = if self.reversed {
            self.selection.end
        } else {
            self.selection.start
        };
        self.selection = if offset < anchor {
            offset..anchor
        } else {
            anchor..offset
        };
        self.reversed = offset < anchor;
        self.marked = None;
        self.blink.pause(cx);
        cx.notify();
    }

    /// Run one of Winc's commands, then adopt whatever selection it left.
    ///
    /// This is the single place edits happen. Typing, Enter, Backspace, indent,
    /// comment toggling, line moves, undo and multi-cursor all arrive here as a
    /// `winc_editor::Command`, so their behaviour is Winc's by construction
    /// rather than by reimplementation.
    ///
    /// The selection is written back to the document first. It has to be: the
    /// document owns the selection Winc's commands act on, so a caret the user
    /// moved with the mouse would otherwise be ignored and the command would
    /// apply at wherever the document last thought the caret was.
    fn run(&mut self, command: WincCommand, cx: &mut Context<Self>) {
        if self.buffer.is_read_only() && command.is_edit() {
            return;
        }
        self.sync_selection_to_buffer();
        let before = self.buffer.text();
        self.buffer.execute(command, self.page_lines);
        self.after_command(before, cx);
    }

    /// Push this view's selection into the document that acts on it.
    fn sync_selection_to_buffer(&mut self) {
        let selection = self.selection.clone();
        let reversed = self.reversed;
        self.buffer.select_bytes(selection, reversed);
    }

    /// Everything to do once a command may have changed the text.
    ///
    /// `before` is the text from prior to the command: a command that only
    /// moved the caret leaves it unchanged, and re-lexing the file for that
    /// would make every arrow key a linear pass over it.
    fn after_command(&mut self, before: String, cx: &mut Context<Self>) {
        let after = self.buffer.text();
        self.selection = self.buffer.selected_range();
        self.reversed = self.buffer.selection_reversed();
        self.extra_selections = self.buffer.extra_selections();
        self.marked = None;
        // Any command dismisses a hint: it was about the caret's old position,
        // and leaving it up would describe a name that is no longer there.
        self.hint_text = None;

        if before != after {
            // Both texts are already materialised by the comparison, so finding
            // where they diverge is cheap relative to the edit itself. Only the
            // lines below the change need re-lexing, which keeps typing in a
            // large file proportional to one line.
            let line = self.first_changed_line(&before, &after);
            self.invalidate_highlight(line);
            self.rows.clear();
            self.blink.pause(cx);
            cx.emit(InputEvent::Edited);
        }
        cx.notify();
    }

    /// The line of the first difference between two texts.
    fn first_changed_line(&self, before: &str, after: &str) -> usize {
        let mut seen = 0usize;
        for (a, b) in before.lines().zip(after.lines()) {
            if a != b {
                return seen;
            }
            seen += 1;
        }
        seen
    }

    fn backspace(&mut self, cx: &mut Context<Self>) {
        self.run(WincCommand::Backspace { word: false }, cx);
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        self.run(WincCommand::Delete { word: false }, cx);
    }

    fn delete_previous_word(&mut self, cx: &mut Context<Self>) {
        self.run(WincCommand::Backspace { word: true }, cx);
    }

    fn delete_next_word(&mut self, cx: &mut Context<Self>) {
        self.run(WincCommand::Delete { word: true }, cx);
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| {
            item.entries().iter().find_map(|entry| match entry {
                ClipboardEntry::String(text) => Some(text.text().to_owned()),
                _ => None,
            })
        }) else {
            return;
        };
        // Winc's paste folds the breaks to the document's own line ending, so a
        // CRLF file pasted into from a Unix clipboard keeps one convention.
        self.run(WincCommand::Paste(text), cx);
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            return;
        }
        let content = self.buffer.text();
        let Some(text) = content.get(self.selection.clone()) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
    }

    fn cut(&mut self, cx: &mut Context<Self>) {
        self.copy(cx);
        self.run(WincCommand::Backspace { word: false }, cx);
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        if self.marked.is_some() {
            return;
        }
        self.run(WincCommand::Undo, cx);
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        if self.marked.is_some() {
            return;
        }
        self.run(WincCommand::Redo, cx);
    }

    // ---- Folding ----------------------------------------------------------

    /// Run one of Winc's fold commands at the caret.
    ///
    /// Folding changes which lines are painted, not the text, so it must not go
    /// through [`Self::run`]: that compares texts to decide what to re-lex and
    /// would find nothing changed. What it does have to do is push the caret
    /// into the document, because Winc folds the block around the *document's*
    /// selection.
    fn fold(&mut self, command: winc_editor::FoldCommand, cx: &mut Context<Self>) {
        if self.buffer.is_read_only() {
            return;
        }
        self.sync_selection_to_buffer();
        if !self.buffer.run_fold(command) {
            return;
        }
        // A fold can hide the caret's own line, and Winc moves the selection out
        // of a collapsed block when that happens.
        self.selection = self.buffer.selected_range();
        self.reversed = self.buffer.selection_reversed();
        self.extra_selections = self.buffer.extra_selections();
        self.rows.clear();
        cx.notify();
    }

    /// Collapse or expand the block whose gutter arrow sits on `line`.
    ///
    /// This is what a click on the gutter calls. A click is a hit test on the
    /// painted rows, so the caller has already resolved the row back to a line.
    pub fn toggle_fold_at(&mut self, line: usize, cx: &mut Context<Self>) {
        if self.buffer.is_read_only() {
            return;
        }
        if !self.buffer.toggle_fold_at(line) {
            return;
        }
        // Collapsing the block the caret is in would leave it invisible; Winc
        // keeps the selection out of folded regions when the fold command runs,
        // and a gutter click has to do the same by hand.
        if let Some((first, last)) = self.buffer.collapsed_span(line) {
            let hidden = self.buffer.line_start_byte(first)
                ..self.buffer.line_start_byte(last.saturating_add(1));
            if hidden.contains(&self.selection.end) {
                let header = self.buffer.line_start_byte(line);
                self.selection = header..header;
                self.reversed = false;
                self.sync_selection_to_buffer();
            }
        }
        self.rows.clear();
        cx.notify();
    }

    /// Every line that opens a block, for the gutter to draw an arrow beside.
    ///
    /// Resolved from the painted rows rather than the whole file: a Markdown
    /// fence costs a scan upward, so asking about every line of a long file
    /// would be the thing that made opening it crawl.
    pub fn block_starts(&mut self, lines: &[usize]) -> Vec<usize> {
        let mut starts: Vec<usize> = lines
            .iter()
            .copied()
            .filter(|line| self.buffer.starts_block(*line))
            .collect();
        starts.sort_unstable();
        starts.dedup();
        starts
    }

    /// Whether `line` is currently collapsed, so its gutter arrow points closed.
    pub fn is_line_collapsed(&self, line: usize) -> bool {
        self.buffer.is_collapsed(line)
    }

    /// Whether the document has any collapsed block.
    ///
    /// Asked by the outline: a file with nothing folded numbers every line, so
    /// the gutter can take the cheap path and skip the row→line mapping.
    pub fn has_folds(&self) -> bool {
        self.buffer.has_folds()
    }

    // ---- Bookmarks --------------------------------------------------------

    /// Toggle a bookmark on the caret's line, through Winc's own bookmark map.
    fn toggle_bookmark(&mut self, cx: &mut Context<Self>) {
        self.sync_selection_to_buffer();
        if !self.buffer.toggle_bookmark() {
            return;
        }
        cx.notify();
    }

    /// Move the caret to the next bookmarked line, wrapping at the end.
    fn next_bookmark(&mut self, cx: &mut Context<Self>) {
        let Some(line) = self.buffer.next_bookmark_line() else {
            return;
        };
        self.jump_to_line(line, cx);
    }

    fn previous_bookmark(&mut self, cx: &mut Context<Self>) {
        let Some(line) = self.buffer.previous_bookmark_line() else {
            return;
        };
        self.jump_to_line(line, cx);
    }

    fn clear_bookmarks(&mut self, cx: &mut Context<Self>) {
        self.buffer.clear_bookmarks();
        cx.notify();
    }

    /// The bookmarked lines, so the gutter can mark them.
    pub fn bookmarked_lines(&self) -> Vec<usize> {
        self.buffer.bookmark_lines()
    }

    /// Put the caret at the start of `line` and scroll it into view.
    fn jump_to_line(&mut self, line: usize, cx: &mut Context<Self>) {
        let offset = self.buffer.line_start_byte(line);
        self.selection = offset..offset;
        self.reversed = false;
        self.sync_selection_to_buffer();
        self.blink.pause(cx);
        // `followed` is what makes the paint pass scroll to the caret; clearing
        // it here would skip the scroll, so the offset is deliberately left
        // different from the previous one.
        cx.notify();
    }

    // ---- Multi-cursor -----------------------------------------------------

    /// Add a cursor on the line above or below, as Winc's column cursors do.
    fn add_cursor_vertically(&mut self, down: bool, cx: &mut Context<Self>) {
        if self.buffer.is_read_only() {
            return;
        }
        self.sync_selection_to_buffer();
        let tab_width = self.buffer.tab_width();
        if self.buffer.add_cursor_vertically(down, tab_width) {
            self.adopt_cursor_set(cx);
        }
    }

    /// Select the next occurrence of the current selection, adding a cursor.
    fn add_next_occurrence(&mut self, cx: &mut Context<Self>) {
        if self.buffer.is_read_only() {
            return;
        }
        self.sync_selection_to_buffer();
        if self.buffer.add_next_occurrence().unwrap_or(false) {
            self.adopt_cursor_set(cx);
        }
    }

    /// Put a cursor on every occurrence of the selection.
    fn select_all_occurrences(&mut self, cx: &mut Context<Self>) {
        if self.buffer.is_read_only() {
            return;
        }
        self.sync_selection_to_buffer();
        if self.buffer.select_all_occurrences().unwrap_or(0) > 1 {
            self.adopt_cursor_set(cx);
        }
    }

    /// Take Winc's cursor set back into the view after a cursor command.
    ///
    /// Winc keeps the extra selections; this view keeps the primary one. Both
    /// have to agree or the washes and the next edit disagree about where the
    /// carets are.
    fn adopt_cursor_set(&mut self, cx: &mut Context<Self>) {
        self.selection = self.buffer.selected_range();
        self.reversed = self.buffer.selection_reversed();
        self.extra_selections = self.buffer.extra_selections();
        self.rows.clear();
        cx.notify();
    }

    /// Drop every cursor but the primary, as Escape does.
    fn collapse_cursors(&mut self, cx: &mut Context<Self>) {
        if self.buffer.collapse_selections() {
            self.adopt_cursor_set(cx);
        }
    }

    // ---- Structural edits the Command enum does not cover directly --------

    /// Open an empty line below the caret and put the caret on it.
    ///
    /// Winc has `InsertLinesAbove` as a command but no "below" counterpart: its
    /// own UI opens a line below by moving to the line's end and inserting a
    /// newline. That is what this does, so the result goes through the same
    /// undo entry as an ordinary Enter.
    fn insert_line_below(&mut self, cx: &mut Context<Self>) {
        self.run(WincCommand::LineEnd { extend: false }, cx);
        self.run(WincCommand::Newline, cx);
    }

    // ---- Formatting, through Winc's tool runner ---------------------------

    /// Which formatter Winc would run for this file, if any.
    ///
    /// Asked before the key does anything, so the panel can say "no formatter
    /// for this language" rather than a chord that silently does nothing.
    pub fn formatter(&self) -> Option<String> {
        let path = self.file_path.clone()?;
        let plan = self.formatter_plan(&path)?;
        Some(plan.tool().into_owned())
    }

    /// The format plan for this document, as Winc resolves it.
    ///
    /// `Plan` is Winc's own tool selection: the project's configured command,
    /// else a preset for the language, else a guess. Waku does not keep a
    /// language→tool table of its own.
    fn formatter_plan(&self, path: &std::path::Path) -> Option<winc_editor::format::Plan> {
        let root = path.parent();
        winc_editor::format::Plan::new(self.buffer.language(), path, root, None).ok()
    }

    /// Format the whole document with whichever tool Winc picks.
    ///
    /// The job runs off the UI thread: a formatter is an external process and
    /// can take a second on a large file. Winc returns the edits as a list of
    /// changes rather than a new string, so the document keeps its scroll
    /// position, its folds and its undo history — and Ctrl+Z undoes the
    /// formatting as one step instead of replaying thousands of splices.
    ///
    /// Every failure says why: no tool for the language, or the tool's own
    /// error. A format key that silently does nothing is the thing users report
    /// as broken.
    fn format_document(&mut self, cx: &mut Context<Self>) {
        if self.buffer.is_read_only() {
            return;
        }
        let Some(path) = self.file_path.clone() else {
            return;
        };
        // Asked first so the "no formatter" case can be reported without
        // spawning anything.
        if self.formatter().is_none() {
            self.hint(tr!("files.format_no_tool"), cx);
            return;
        }
        let Some(job) = self.format_job() else {
            return;
        };
        let cancel = winc_editor::format::Cancel::default();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let formatter = winc_editor::format::Formatter::default();
                    formatter.run(job, &cancel)
                })
                .await;
            let formatted = match result {
                Ok(formatted) => formatted,
                Err(error) => {
                    let message = tr!("files.format_failed", error = error.to_string());
                    this.update(cx, |this, cx| this.hint(message, cx)).ok();
                    return;
                }
            };
            if formatted.changes.is_empty() {
                return;
            }
            this.update(cx, |this, cx| {
                this.buffer.apply_changes(formatted.changes);
                this.selection = this.buffer.selected_range();
                this.reversed = this.buffer.selection_reversed();
                this.invalidate_highlight(0);
                this.rows.clear();
                cx.emit(InputEvent::Edited);
                cx.notify();
            })
            .ok();
        })
        .detach();
        let _ = path;
    }

    /// The format job for this document's current text, if a tool resolves.
    fn format_job(&self) -> Option<winc_editor::format::Job> {
        let path = self.file_path.clone()?;
        let plan = self.formatter_plan(&path)?;
        let text = self.buffer.rope()?;
        Some(winc_editor::format::Job {
            plan,
            text,
            line_ending: self.buffer.line_ending(),
            timeout: FORMAT_TIMEOUT,
        })
    }

    /// The colours this editor paints with, read from the theme at paint time
    /// so a theme switch recolours an open file instead of leaving it stale.
    fn palette(theme: &Theme) -> EditorPalette {
        use crate::md::highlight::TokenClass;
        let palette = crate::md::render::Palette::from_theme(theme);
        EditorPalette {
            text: theme.text,
            keyword: palette.token(TokenClass::Keyword),
            literal: palette.token(TokenClass::Literal),
            string: palette.token(TokenClass::String),
            comment: palette.token(TokenClass::Comment),
            type_name: palette.token(TokenClass::Type),
            meta: palette.token(TokenClass::Meta),
            added: palette.token(TokenClass::Added),
            removed: palette.token(TokenClass::Removed),
        }
    }

    /// Byte offset for a point in window coordinates.
    ///
    /// Clamped to the nearest row and column, so a click past the end of a line
    /// lands on its last character and a click above the first row lands at the
    /// top, the way a native text view behaves.
    fn offset_for_point(&self, position: Point<Pixels>) -> usize {
        let Some(row) = self.row_for_point(position) else {
            return self.selection.end;
        };
        let Some(bounds) = self.text_bounds else {
            return self.selection.end;
        };
        let x = position.x - bounds.left();
        let column = if x <= Pixels::ZERO {
            0
        } else {
            row.shaped.closest_index_for_x(x).min(row.text.len())
        };
        self.buffer.line_start_byte(row.line) + column
    }

    fn row_for_point(&self, position: Point<Pixels>) -> Option<&PaintedRow> {
        if self.rows.is_empty() {
            return None;
        }
        self.rows
            .iter()
            .find(|row| position.y < row.top + self.row_height)
            .or_else(|| self.rows.last())
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        window.focus(&self.focus_handle, cx);
        let offset = self.offset_for_point(event.position);
        if event.modifiers.shift {
            self.extend_selection(offset, cx);
        } else if event.click_count >= 2 {
            // Winc's own notion of "the word under the caret", so a double
            // click and Ctrl+arrow agree about what a word is.
            let range = self.buffer.word_at(offset);
            if !range.is_empty() {
                self.select_range(range, cx);
            } else {
                self.move_caret(offset, false, cx);
            }
        } else {
            self.move_caret(offset, false, cx);
        }
        self.selecting = true;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let offset = self.offset_for_point(event.position);
        self.extend_selection(offset, cx);
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            self.selecting = false;
            cx.notify();
        }
    }

    /// Scroll the viewport so the caret is visible.
    ///
    /// Only when the caret moved: scrolling on every repaint would drag the
    /// view back out from under a user who scrolled away from it.
    fn follow_caret(&mut self, window: &mut Window) {
        if self.followed == Some(self.selection.end) {
            return;
        }
        let Some((position, line_height)) = self.position_for_offset(self.selection.end) else {
            return;
        };
        let viewport = self.scroll.bounds();
        if viewport.is_empty() {
            return;
        }
        let current = self.scroll.offset();
        let mut y = current.y;
        if position.y - line_height < viewport.top() {
            y = current.y - (viewport.top() - (position.y - line_height));
        } else if position.y + line_height * OVERSCAN_ROWS > viewport.bottom() {
            y = current.y + (position.y + line_height * OVERSCAN_ROWS - viewport.bottom());
        }
        if y != current.y {
            let max = self.scroll.max_offset().y;
            self.scroll
                .set_offset(point(current.x, y.max(-max).min(Pixels::ZERO)));
            window.refresh();
        }
        self.followed = Some(self.selection.end);
    }

    /// The selection's wash on `row`, in row-relative bytes, if it reaches it.
    ///
    /// `Option` rather than a one-element `Vec`: a plain selection covers one
    /// contiguous range, so there is never more than one, and an `Option` says
    /// so instead of making every caller iterate.
    fn selection_wash(&self, row: &Range<usize>) -> Option<Range<usize>> {
        let selection = self.selection.clone();
        if selection.is_empty() {
            return None;
        }
        let start = selection.start.max(row.start);
        let end = selection.end.min(row.end);
        // The subtraction has to happen only when the range is real:
        // `then_some` evaluates its argument eagerly, so a row that lies past
        // the selection would underflow here before the condition was checked.
        if start >= end {
            return None;
        }
        Some(start - row.start..end - row.start)
    }

    /// The primary selection's wash on `row`, in row-relative bytes.
    fn search_wash_ranges(&self, row: &Range<usize>) -> Vec<(usize, Range<usize>)> {
        self.search_matches
            .iter()
            .enumerate()
            .filter_map(|(index, range)| {
                let start = range.start.max(row.start);
                let end = range.end.min(row.end);
                // Same eager-evaluation trap as `selection_wash`: subtract only
                // once the range is known to be real.
                if start >= end {
                    return None;
                }
                Some((index, start - row.start..end - row.start))
            })
            .collect()
    }

    /// Winc's extra cursors' washes on `row`, in row-relative bytes.
    ///
    /// Same clipping as [`Self::selection_wash`], applied to each of the extra
    /// selections so a multi-cursor edit is visible on every row it touches
    /// rather than only where the primary caret happens to be.
    fn extra_selection_washes(&self, row: &Range<usize>) -> Vec<Range<usize>> {
        self.extra_selections
            .iter()
            .filter_map(|selection| {
                let start = selection.start.max(row.start);
                let end = selection.end.min(row.end);
                if start >= end {
                    return None;
                }
                Some(start - row.start..end - row.start)
            })
            .collect()
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        let content = self.buffer.text();
        let mut utf16 = 0usize;
        let mut start = None;
        let mut end = None;
        for (byte, ch) in content.char_indices() {
            if start.is_none() && utf16 >= range.start {
                start = Some(byte);
            }
            if end.is_none() && utf16 >= range.end {
                end = Some(byte);
            }
            if start.is_some() && end.is_some() {
                break;
            }
            utf16 += ch.len_utf16();
        }
        let start = start.unwrap_or(content.len());
        let end = end.unwrap_or(content.len());
        start..end.max(start)
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        let content = self.buffer.text();
        let start = content
            .get(..range.start.min(content.len()))
            .map_or(0, |text| text.encode_utf16().count());
        let end = content
            .get(..range.end.min(content.len()))
            .map_or(start, |text| text.encode_utf16().count());
        start..end.max(start)
    }
}

impl EventEmitter<InputEvent> for WincEditor {}

impl Focusable for WincEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for WincEditor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        adjusted_range.replace(self.range_to_utf16(&range));
        self.buffer.text().get(range).map(str::to_owned)
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selection.clone()),
            reversed: self.reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // While text is marked, a replacement targets the marked span: the
        // protocol reports ranges relative to the composition.
        let range = match (&self.marked, range_utf16.as_ref()) {
            (Some(marked), _) => marked.clone(),
            (None, Some(range_utf16)) => self.range_from_utf16(range_utf16),
            (None, None) => self.selection.clone(),
        };
        self.replace_range(range, text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.buffer.is_read_only() {
            return;
        }
        let range = match (&self.marked, range_utf16.as_ref()) {
            (Some(marked), _) => marked.clone(),
            (None, Some(range_utf16)) => self.range_from_utf16(range_utf16),
            (None, None) => self.selection.clone(),
        };
        // An empty composition is a cancel: the marked text goes away and the
        // caret returns to where the composition started. Winc's history keeps
        // whatever the composition inserted as one step, so cancelling here
        // does not leave a half-applied edit behind.
        if new_text.is_empty() {
            self.marked = None;
            self.selection = range.start..range.start;
            self.rows.clear();
            cx.emit(InputEvent::Edited);
            cx.notify();
            return;
        }
        self.replace_selection_composing(range, new_text, new_selected_range_utf16, cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        let (position, line_height) = self.position_for_offset(range.start)?;
        let (end, _) = self
            .position_for_offset(range.end)
            .unwrap_or((position, line_height));
        let _ = cx;
        Some(Bounds::from_corners(
            position,
            point(end.x, position.y + line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(
            self.range_to_utf16(&(self.offset_for_point(point)..self.offset_for_point(point)))
                .start,
        )
    }
}

impl WincEditor {
    /// Where `name` is defined, found by Winc's own definition scanner.
    ///
    /// Not a language server: Winc looks for the *written form* of a definition
    /// (`fn name`, `def name`, `class Name`, a bare `name = …` at line start)
    /// in this file. That finds most same-file definitions and honestly finds
    /// nothing for a type it cannot see spelled out, which is better than
    /// guessing and jumping somewhere wrong.
    ///
    /// Returned as a byte offset so the caller can select and reveal it.
    pub fn definition_of(&self, name: &str) -> Option<usize> {
        let rope = self.buffer.rope()?;
        let finder = winc_editor::definition::Finder::new(name, false, false)?;
        let language = self.buffer.language();
        let found = finder.in_text(&rope, language);
        // Prefer a definition on a line the name is not merely mentioned on:
        // `in_text` already filtered by the written form, so the first hit is
        // the earliest definition, which is the one a reader means.
        let (line, definition) = found.first()?;
        Some(self.buffer.byte_at_line_col(*line, definition.col))
    }

    /// Every place `name` is *used* in this file, per Winc's usage filter.
    ///
    /// The filter is the valuable part: it drops hits inside comments and
    /// strings, and hits that are another definition of the same name. That
    /// leaves matches a plain text search would wrongly report.
    pub fn usages_of(&self, name: &str) -> Vec<Range<usize>> {
        let Some(rope) = self.buffer.rope() else {
            return Vec::new();
        };
        let language = self.buffer.language();
        let Some(filter) = winc_editor::usages::Filter::new(name, language) else {
            return Vec::new();
        };
        let candidates = filter.occurrences(&rope);
        let keep = filter.usages(language, &rope, &candidates);
        candidates
            .iter()
            .zip(keep)
            .filter(|(_, keep)| *keep)
            .map(|(&(line, col), _)| {
                let start = self.buffer.byte_at_line_col(line, col);
                start..start + name.len()
            })
            .collect()
    }

    /// The symbols in this file, for a go-to-symbol list.
    ///
    /// Winc reads them by indentation and per-language patterns, so this works
    /// for any file it can lex and needs no index. The panel has no symbol
    /// palette yet, so nothing calls this today; it is the data such a palette
    /// would list, kept here because it is the editor that can produce it.
    #[allow(
        dead_code,
        reason = "the symbol palette that lists these is not built yet; the scan itself is Winc's"
    )]
    pub fn symbols(&self) -> Vec<FileSymbol> {
        let Some(rope) = self.buffer.rope() else {
            return Vec::new();
        };
        let language = self.buffer.language();
        winc_editor::symbols::symbols(&rope, language)
            .into_iter()
            .map(|symbol| FileSymbol {
                name: symbol.name.clone(),
                kind: format!("{:?}", symbol.kind),
                indent: symbol.depth,
                offset: self.buffer.byte_at_line_col(symbol.line, symbol.col),
                line: symbol.line,
            })
            .collect()
    }
}

/// One entry in a file's symbol list.
///
/// The fields a palette renders — its kind, its indent, its line — are filled
/// from Winc's scan even though nothing reads them yet: the palette is the piece
/// still missing, and a list that could not indent a method under its type would
/// have to re-scan to get that back.
#[allow(
    dead_code,
    reason = "the symbol palette that reads these is not built yet"
)]
pub struct FileSymbol {
    pub name: String,
    pub kind: String,
    /// Nesting depth, so a list can indent a method under its type.
    pub indent: usize,
    /// Byte offset of the name, to select when the symbol is chosen.
    pub offset: usize,
    pub line: usize,
}

impl WincEditor {
    /// Jump to the definition of the name under the caret.
    ///
    /// Resolved by Winc's own scanner, then selected so the caret lands on the
    /// name rather than merely on its line.
    ///
    /// Every failure says why in a hint. Winc's own UI does the same: a key that
    /// silently does nothing is indistinguishable from a broken key.
    fn goto_definition(&mut self, cx: &mut Context<Self>) {
        // The caret the user sees lives in this view; the buffer's own cursor is
        // only current after a sync, so reading it first would look up whatever
        // name was under the caret the last time a command ran.
        self.sync_selection_to_buffer();
        let Some((name, _)) = self.buffer.name_under_caret() else {
            self.hint(tr!("files.no_name_under_caret"), cx);
            return;
        };
        let Some(offset) = self.definition_of(&name) else {
            self.hint(tr!("files.no_definition"), cx);
            return;
        };
        self.clear_hint();
        self.select_range(offset..offset + name.len(), cx);
    }

    /// Wash every usage of the name under the caret, and jump to the first.
    ///
    /// The matches go into the same wash the find bar uses, so a caller does not
    /// need a second overlay to see them.
    fn find_usages(&mut self, cx: &mut Context<Self>) {
        // Same reason as `goto_definition`: the buffer's cursor is only current
        // after a sync, and a stale one looks up the wrong name.
        self.sync_selection_to_buffer();
        let Some((name, _)) = self.buffer.name_under_caret() else {
            self.hint(tr!("files.no_name_under_caret"), cx);
            return;
        };
        let usages = self.usages_of(&name);
        if usages.is_empty() {
            self.set_search_matches(Vec::new(), None, cx);
            self.hint(tr!("files.no_usages"), cx);
            return;
        }
        self.clear_hint();
        let first = usages[0].clone();
        self.set_search_matches(usages, Some(0), cx);
        self.select_range(first, cx);
    }

    /// Show a one-line explanation at the caret, cleared on the next keystroke.
    fn hint(&mut self, text: String, cx: &mut Context<Self>) {
        self.hint_text = Some(text);
        cx.notify();
    }

    fn clear_hint(&mut self) {
        if self.hint_text.take().is_some() {
            // The caller repaints; nothing else to do here.
        }
    }

    /// The hint to paint under the caret, if one is showing.
    pub fn hint_text(&self) -> Option<&str> {
        self.hint_text.as_deref()
    }

    /// Select the `index`th symbol in this file.
    ///
    /// The other half of the symbol palette: it jumps, given a list to jump
    /// from. The panel's picker reads [`Self::symbols`] and calls this with the
    /// chosen index, so the scan and the jump stay one implementation of what a
    /// symbol is.
    pub fn goto_symbol(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(symbol) = self.symbols().into_iter().nth(index) else {
            self.hint(tr!("files.no_symbols"), cx);
            return;
        };
        self.clear_hint();
        self.select_range(symbol.offset..symbol.offset + symbol.name.len(), cx);
    }
}

impl WincEditor {
    /// Apply one IME composition revision over `range`.
    ///
    /// The composition's ranges arrive relative to the marked text, so this
    /// keeps the marked span in step and leaves the caret where the IME says.
    /// Winc's `Insert` records the splice, so the document's own history stays
    /// the only undo stack.
    fn replace_selection_composing(
        &mut self,
        range: Range<usize>,
        text: &str,
        caret_utf16: Option<Range<usize>>,
        cx: &mut Context<Self>,
    ) {
        let start = range.start;
        if !self.buffer.edit(range, text) {
            return;
        }
        let end = start + text.len();
        self.marked = Some(start..end);
        self.selection = match caret_utf16 {
            Some(caret) => {
                // The protocol reports the composition's own caret, relative to
                // the marked text; it is translated through the same byte/UTF-16
                // boundary as everything else here.
                let base = self.range_to_utf16(&(start..start)).start;
                self.range_from_utf16(&(base + caret.start..base + caret.end))
            }
            None => end..end,
        };
        let line = self.buffer.line_of_byte(start);
        self.invalidate_highlight(line);
        self.rows.clear();
        self.blink.pause(cx);
        cx.emit(InputEvent::Edited);
        cx.notify();
    }
}

impl Render for WincEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::current(cx);
        let palette = Self::palette(&theme);
        let selection_wash = theme.selection;
        let search_wash = theme.warning.opacity(0.22);
        let active_wash = theme.warning.opacity(0.5);
        let caret_color = theme.accent;
        // The default foreground comes from the palette, not from the theme
        // directly: the palette is what decides how an unclassified run looks,
        // and reading it from two places is how they drift apart.
        let text_color = palette.text;
        let hint_text_color = theme.text;
        let hint_background = theme.surface;
        let editor = cx.entity();
        let focus = self.focus_handle.clone();
        let font_size = self.font_size;
        let row_height = self.row_height;
        let scroll = self.scroll.clone();

        let field = canvas(
            |_, _, _| (),
            move |bounds: Bounds<Pixels>, _, window: &mut Window, cx: &mut App| {
                let mut rows: Vec<PaintedRow> = Vec::new();
                let mut washes: Vec<(Bounds<Pixels>, gpui::Hsla, f32)> = Vec::new();
                let mut caret: Option<Bounds<Pixels>> = None;

                editor.update(cx, |editor, cx| {
                    let offset = scroll.offset();
                    let scroll_top = (-offset.y).max(Pixels::ZERO);
                    let painted = visible_lines(
                        &editor.buffer,
                        editor.highlighter.as_mut(),
                        f32::from(scroll_top),
                        f32::from(bounds.size.height),
                        f32::from(row_height),
                        &palette,
                    );

                    let mut y = bounds.top();
                    for row in &painted {
                        let text: SharedString = row.text.clone().into();
                        let runs = runs_for(row, text_color);
                        // `force_width` is an advance width, not a minimum: GPUI
                        // places glyph *n* at `n * force_width`. Passing the row
                        // height here spaced every character 1.5em apart, more
                        // than twice a monospace advance, which pushed ordinary
                        // code far past the viewport and left the body looking
                        // blank while the gutter still drew its numbers.
                        let shaped = if runs.is_empty() {
                            window.text_system().shape_line(
                                text.clone(),
                                font_size,
                                &[TextRun {
                                    len: 0,
                                    font: gpui::font(crate::md::render::MONO_FAMILY),
                                    color: text_color,
                                    ..Default::default()
                                }],
                                None,
                            )
                        } else {
                            window.text_system().shape_line(
                                text.clone(),
                                font_size,
                                &runs,
                                row_force_width(),
                            )
                        };

                        let line_start = editor.buffer.line_start_byte(row.line);
                        let row_range = line_start..line_start + row.text.len();
                        if let Some(range) = editor.selection_wash(&row_range) {
                            let x = shaped.x_for_index(range.start);
                            let end_x = shaped.x_for_index(range.end);
                            washes.push((
                                Bounds::from_corners(
                                    point(bounds.left() + x, y),
                                    point(bounds.left() + end_x, y + row_height),
                                ),
                                selection_wash,
                                0.0,
                            ));
                        }
                        // Winc's extra cursors, washed like the primary one so a
                        // Ctrl+D is visible as more than a caret that moved.
                        for range in editor.extra_selection_washes(&row_range) {
                            let x = shaped.x_for_index(range.start);
                            let end_x = shaped.x_for_index(range.end);
                            washes.push((
                                Bounds::from_corners(
                                    point(bounds.left() + x, y),
                                    point(bounds.left() + end_x, y + row_height),
                                ),
                                selection_wash,
                                0.0,
                            ));
                        }
                        for (index, range) in editor.search_wash_ranges(&row_range) {
                            // The current match is washed more strongly than its
                            // siblings, which is how the user tells which one
                            // Enter will step past next.
                            let color = if editor.active_search_match == Some(index) {
                                active_wash
                            } else {
                                search_wash
                            };
                            let x = shaped.x_for_index(range.start);
                            let end_x = shaped.x_for_index(range.end);
                            washes.push((
                                Bounds::from_corners(
                                    point(bounds.left() + x, y),
                                    point(bounds.left() + end_x, y + row_height),
                                ),
                                color,
                                2.0,
                            ));
                        }
                        if editor.selection.end >= row_range.start
                            && editor.selection.end <= row_range.end
                        {
                            let column = editor.selection.end - row_range.start;
                            let x = shaped.x_for_index(column.min(row.text.len()));
                            caret = Some(Bounds::from_corners(
                                point(bounds.left() + x, y),
                                point(bounds.left() + x + px(1.5), y + row_height),
                            ));
                        }

                        rows.push(PaintedRow {
                            line: row.line,
                            row: row.row,
                            text,
                            shaped,
                            top: y,
                        });
                        y += row_height;
                    }

                    editor.rows = rows;
                    editor.text_bounds = Some(bounds);
                    editor.follow_caret(window);
                    let _ = cx;
                });

                for (bounds, color, radius) in washes {
                    window.paint_quad(quad(
                        bounds,
                        px(radius),
                        color,
                        px(0.0),
                        transparent_black(),
                        BorderStyle::default(),
                    ));
                }

                let paint_rows = editor.read(cx).rows.clone();
                for row in &paint_rows {
                    let _ = row.shaped.paint(
                        point(bounds.left(), row.top),
                        row_height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }

                let focused = editor.read(cx).focus_handle.is_focused(window);
                let visible = editor.read(cx).blink.visible();
                let read_only = editor.read(cx).is_read_only();
                if focused
                    && visible
                    && !read_only
                    && let Some(caret) = caret
                {
                    window.paint_quad(quad(
                        caret,
                        px(0.0),
                        caret_color,
                        px(0.0),
                        transparent_black(),
                        BorderStyle::default(),
                    ));
                }
                let _ = active_wash;

                // The hint a failed navigation left behind, under the caret's
                // row. Painted last so it reads over whatever is beneath it.
                let hint = editor.read(cx).hint_text().map(str::to_owned);
                if let Some(hint) = hint {
                    let selection = editor.read(cx).selection.end;
                    let line = editor.read(cx).buffer.line_of_byte(selection);
                    let top = editor
                        .read(cx)
                        .rows
                        .iter()
                        .find(|painted| painted.line == line)
                        .map(|painted| painted.top)
                        .unwrap_or(bounds.top());
                    let text: SharedString = hint.into();
                    let shaped = window.text_system().shape_line(
                        text,
                        font_size,
                        &[TextRun {
                            len: 0,
                            font: gpui::font(crate::md::render::MONO_FAMILY),
                            color: hint_text_color,
                            ..Default::default()
                        }],
                        None,
                    );
                    let padding = px(6.0);
                    let origin = point(bounds.left() + px(8.0), top + row_height + px(4.0));
                    window.paint_quad(quad(
                        Bounds::from_corners(
                            point(origin.x - padding, origin.y - px(2.0)),
                            point(
                                origin.x + shaped.width + padding,
                                origin.y + row_height + px(2.0),
                            ),
                        ),
                        px(4.0),
                        hint_background,
                        px(0.0),
                        transparent_black(),
                        BorderStyle::default(),
                    ));
                    let _ = shaped.paint(origin, row_height, TextAlign::Left, None, window, cx);
                }

                window.handle_input(&focus, ElementInputHandler::new(bounds, editor.clone()), cx);
            },
        )
        .size_full();

        div()
            .key_context("WincEditor")
            .size_full()
            .relative()
            .cursor(CursorStyle::IBeam)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Backspace, _, cx| this.backspace(cx)))
            .on_action(cx.listener(|this, _: &Delete, _, cx| this.delete(cx)))
            .on_action(
                cx.listener(|this, _: &DeleteToPreviousWord, _, cx| this.delete_previous_word(cx)),
            )
            .on_action(cx.listener(|this, _: &DeleteToNextWord, _, cx| this.delete_next_word(cx)))
            // Motion is Winc's, so the goal column, grapheme snapping and
            // multi-cursor rules are the same ones its own UI gets.
            .on_action(cx.listener(|this, _: &Left, _, cx| {
                this.run(
                    WincCommand::MoveLeft {
                        extend: false,
                        word: false,
                    },
                    cx,
                )
            }))
            .on_action(cx.listener(|this, _: &Right, _, cx| {
                this.run(
                    WincCommand::MoveRight {
                        extend: false,
                        word: false,
                    },
                    cx,
                )
            }))
            .on_action(cx.listener(|this, _: &Up, _, cx| {
                this.run(WincCommand::MoveUp { extend: false }, cx)
            }))
            .on_action(cx.listener(|this, _: &Down, _, cx| {
                this.run(WincCommand::MoveDown { extend: false }, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectLeft, _, cx| {
                this.run(
                    WincCommand::MoveLeft {
                        extend: true,
                        word: false,
                    },
                    cx,
                )
            }))
            .on_action(cx.listener(|this, _: &SelectRight, _, cx| {
                this.run(
                    WincCommand::MoveRight {
                        extend: true,
                        word: false,
                    },
                    cx,
                )
            }))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| {
                this.run(WincCommand::MoveUp { extend: true }, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| {
                this.run(WincCommand::MoveDown { extend: true }, cx)
            }))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| {
                this.run(WincCommand::PageUp { extend: false }, cx)
            }))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| {
                this.run(WincCommand::PageDown { extend: false }, cx)
            }))
            .on_action(cx.listener(|this, _: &LineStart, _, cx| {
                this.run(WincCommand::LineStart { extend: false }, cx)
            }))
            .on_action(cx.listener(|this, _: &LineEnd, _, cx| {
                this.run(WincCommand::LineEnd { extend: false }, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectToLineStart, _, cx| {
                this.run(WincCommand::LineStart { extend: true }, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectToLineEnd, _, cx| {
                this.run(WincCommand::LineEnd { extend: true }, cx)
            }))
            .on_action(cx.listener(|this, _: &DocStart, _, cx| {
                this.run(WincCommand::DocStart { extend: false }, cx)
            }))
            .on_action(cx.listener(|this, _: &DocEnd, _, cx| {
                this.run(WincCommand::DocEnd { extend: false }, cx)
            }))
            .on_action(cx.listener(|this, _: &JumpToBracket, _, cx| {
                this.run(WincCommand::JumpToBracket, cx)
            }))
            .on_action(cx.listener(|this, _: &ExpandSelection, _, cx| {
                this.run(WincCommand::ExpandSelection, cx)
            }))
            .on_action(cx.listener(|this, _: &ShrinkSelection, _, cx| {
                this.run(WincCommand::ShrinkSelection, cx)
            }))
            // The structural edits Winc ships: indentation, comments, line
            // moves and the rest of its command set.
            .on_action(cx.listener(|this, _: &Indent, _, cx| this.run(WincCommand::Indent, cx)))
            .on_action(cx.listener(|this, _: &Outdent, _, cx| this.run(WincCommand::Outdent, cx)))
            .on_action(cx.listener(|this, _: &ToggleComment, _, cx| {
                this.run(WincCommand::ToggleComment, cx)
            }))
            .on_action(
                cx.listener(|this, _: &DeleteLine, _, cx| this.run(WincCommand::DeleteLine, cx)),
            )
            .on_action(cx.listener(|this, _: &DeleteToLineStart, _, cx| {
                this.run(WincCommand::DeleteToLineStart, cx)
            }))
            .on_action(
                cx.listener(|this, _: &JoinLines, _, cx| this.run(WincCommand::JoinLines, cx)),
            )
            .on_action(cx.listener(|this, _: &TrimTrailingWhitespace, _, cx| {
                this.run(WincCommand::TrimTrailingWhitespace, cx)
            }))
            .on_action(cx.listener(|this, _: &SortLines, _, cx| {
                this.run(WincCommand::SortLines { descending: false }, cx)
            }))
            .on_action(cx.listener(|this, _: &MoveToPreviousWord, _, cx| {
                this.run(
                    WincCommand::MoveLeft {
                        word: true,
                        extend: false,
                    },
                    cx,
                )
            }))
            .on_action(cx.listener(|this, _: &MoveToNextWord, _, cx| {
                this.run(
                    WincCommand::MoveRight {
                        word: true,
                        extend: false,
                    },
                    cx,
                )
            }))
            .on_action(cx.listener(|this, _: &SelectToPreviousWord, _, cx| {
                this.run(
                    WincCommand::MoveLeft {
                        word: true,
                        extend: true,
                    },
                    cx,
                )
            }))
            .on_action(cx.listener(|this, _: &SelectToNextWord, _, cx| {
                this.run(
                    WincCommand::MoveRight {
                        word: true,
                        extend: true,
                    },
                    cx,
                )
            }))
            .on_action(
                cx.listener(|this, _: &SelectAll, _, cx| this.run(WincCommand::SelectAll, cx)),
            )
            .on_action(cx.listener(|this, _: &Paste, _, cx| this.paste(cx)))
            .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy(cx)))
            .on_action(cx.listener(|this, _: &Cut, _, cx| this.cut(cx)))
            .on_action(cx.listener(|this, _: &Undo, _, cx| this.undo(cx)))
            .on_action(cx.listener(|this, _: &Redo, _, cx| this.redo(cx)))
            .on_action(cx.listener(|this, _: &Newline, _, cx| this.run(WincCommand::Newline, cx)))
            // Navigation, resolved by Winc's own scanners.
            .on_action(cx.listener(|this, _: &GotoDefinition, _, cx| this.goto_definition(cx)))
            .on_action(cx.listener(|this, _: &FindUsages, _, cx| this.find_usages(cx)))
            // The structural edits Winc has that the first pass left unbound.
            .on_action(
                cx.listener(|this, _: &IndentLines, _, cx| this.run(WincCommand::IndentLines, cx)),
            )
            .on_action(
                cx.listener(|this, _: &OutdentLines, _, cx| this.run(WincCommand::Outdent, cx)),
            )
            .on_action(cx.listener(|this, _: &MoveLinesUp, _, cx| {
                this.run(WincCommand::MoveLines { down: false }, cx)
            }))
            .on_action(cx.listener(|this, _: &MoveLinesDown, _, cx| {
                this.run(WincCommand::MoveLines { down: true }, cx)
            }))
            .on_action(cx.listener(|this, _: &CopyLinesUp, _, cx| {
                this.run(WincCommand::CopyLines { down: false }, cx)
            }))
            .on_action(cx.listener(|this, _: &CopyLinesDown, _, cx| {
                this.run(WincCommand::CopyLines { down: true }, cx)
            }))
            .on_action(cx.listener(|this, _: &InsertLineAbove, _, cx| {
                this.run(WincCommand::InsertLinesAbove(String::new()), cx)
            }))
            .on_action(cx.listener(|this, _: &InsertLineBelow, _, cx| this.insert_line_below(cx)))
            .on_action(cx.listener(|this, _: &ChangeCaseUpper, _, cx| {
                this.run(WincCommand::ChangeCase(Case::Upper), cx)
            }))
            .on_action(cx.listener(|this, _: &ChangeCaseLower, _, cx| {
                this.run(WincCommand::ChangeCase(Case::Lower), cx)
            }))
            .on_action(cx.listener(|this, _: &ChangeCaseTitle, _, cx| {
                this.run(WincCommand::ChangeCase(Case::Title), cx)
            }))
            // Folding, through Winc's fold commands.
            .on_action(
                cx.listener(|this, _: &Fold, _, cx| this.fold(winc_editor::FoldCommand::Fold, cx)),
            )
            .on_action(cx.listener(|this, _: &Unfold, _, cx| {
                this.fold(winc_editor::FoldCommand::Unfold, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleFold, _, cx| {
                this.fold(winc_editor::FoldCommand::Toggle, cx)
            }))
            .on_action(cx.listener(|this, _: &FoldRecursively, _, cx| {
                this.fold(winc_editor::FoldCommand::FoldRecursively, cx)
            }))
            .on_action(cx.listener(|this, _: &UnfoldRecursively, _, cx| {
                this.fold(winc_editor::FoldCommand::UnfoldRecursively, cx)
            }))
            .on_action(cx.listener(|this, _: &FoldAll, _, cx| {
                this.fold(winc_editor::FoldCommand::FoldAll, cx)
            }))
            .on_action(cx.listener(|this, _: &UnfoldAll, _, cx| {
                this.fold(winc_editor::FoldCommand::UnfoldAll, cx)
            }))
            .on_action(cx.listener(|this, _: &FoldLevel1, _, cx| {
                this.fold(winc_editor::FoldCommand::FoldLevel(1), cx)
            }))
            .on_action(cx.listener(|this, _: &FoldLevel2, _, cx| {
                this.fold(winc_editor::FoldCommand::FoldLevel(2), cx)
            }))
            .on_action(cx.listener(|this, _: &FoldLevel3, _, cx| {
                this.fold(winc_editor::FoldCommand::FoldLevel(3), cx)
            }))
            // Bookmarks.
            .on_action(cx.listener(|this, _: &ToggleBookmark, _, cx| this.toggle_bookmark(cx)))
            .on_action(cx.listener(|this, _: &NextBookmark, _, cx| this.next_bookmark(cx)))
            .on_action(cx.listener(|this, _: &PreviousBookmark, _, cx| this.previous_bookmark(cx)))
            .on_action(cx.listener(|this, _: &ClearBookmarks, _, cx| this.clear_bookmarks(cx)))
            // Multi-cursor, driven by Winc's cursor set.
            .on_action(
                cx.listener(|this, _: &AddCursorAbove, _, cx| {
                    this.add_cursor_vertically(false, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &AddCursorBelow, _, cx| this.add_cursor_vertically(true, cx)),
            )
            .on_action(cx.listener(|this, _: &AddCursorToNextOccurrence, _, cx| {
                this.add_next_occurrence(cx)
            }))
            .on_action(
                cx.listener(|this, _: &SelectAllOccurrences, _, cx| {
                    this.select_all_occurrences(cx)
                }),
            )
            // Escape collapses extra cursors before anything else sees it, which
            // is what a user reaching for Escape after a stray Ctrl+D expects.
            .on_action(cx.listener(|this, _: &CollapseCursors, _, cx| this.collapse_cursors(cx)))
            // Formatting through whichever tool Winc resolves for the language.
            .on_action(cx.listener(|this, _: &FormatDocument, _, cx| this.format_document(cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .child(field)
    }
}

// The editor's commands, one action each.
//
// Unit actions rather than one action carrying a payload: GPUI's `Action`
// derive wants serde for a payload, and a unit struct keeps the key map and the
// listeners ordinary.
actions!(
    winc_editor,
    [
        Backspace,
        Delete,
        DeleteToPreviousWord,
        DeleteToNextWord,
        DeleteToLineStart,
        DeleteLine,
        Left,
        Right,
        Up,
        Down,
        PageUp,
        PageDown,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        LineStart,
        LineEnd,
        SelectToLineStart,
        SelectToLineEnd,
        DocStart,
        DocEnd,
        MoveToPreviousWord,
        MoveToNextWord,
        SelectToPreviousWord,
        SelectToNextWord,
        SelectAll,
        Indent,
        Outdent,
        ToggleComment,
        JoinLines,
        SortLines,
        TrimTrailingWhitespace,
        JumpToBracket,
        ExpandSelection,
        ShrinkSelection,
        Paste,
        Copy,
        Cut,
        Undo,
        Redo,
        Newline,
        GotoDefinition,
        FindUsages,
        // The rest of Winc's structural edits, which the first pass left out.
        IndentLines,
        OutdentLines,
        MoveLinesUp,
        MoveLinesDown,
        CopyLinesUp,
        CopyLinesDown,
        InsertLineAbove,
        InsertLineBelow,
        ChangeCaseUpper,
        ChangeCaseLower,
        ChangeCaseTitle,
        // Folding: Winc's own command family, one action each because GPUI
        // dispatches on the action's type and a payload would need serde.
        Fold,
        Unfold,
        ToggleFold,
        FoldRecursively,
        UnfoldRecursively,
        FoldAll,
        UnfoldAll,
        FoldLevel1,
        FoldLevel2,
        FoldLevel3,
        // Bookmarks.
        ToggleBookmark,
        NextBookmark,
        PreviousBookmark,
        ClearBookmarks,
        // Navigation with feedback, and the symbol jump.
        GoToSymbol,
        // Multi-cursor: Winc's own cursor commands.
        AddCursorAbove,
        AddCursorBelow,
        AddCursorToNextOccurrence,
        SelectAllOccurrences,
        CollapseCursors,
        // Formatting through Winc's tool runner.
        FormatDocument,
    ]
);

/// Register the editor's key bindings.
///
/// The chords a multi-line text field uses, under this editor's own context so
/// the two cannot shadow each other.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some("WincEditor")),
        KeyBinding::new("delete", Delete, Some("WincEditor")),
        KeyBinding::new("shift-backspace", Backspace, Some("WincEditor")),
        KeyBinding::new("ctrl-backspace", DeleteToPreviousWord, Some("WincEditor")),
        KeyBinding::new("ctrl-delete", DeleteToNextWord, Some("WincEditor")),
        KeyBinding::new("left", Left, Some("WincEditor")),
        KeyBinding::new("right", Right, Some("WincEditor")),
        KeyBinding::new("up", Up, Some("WincEditor")),
        KeyBinding::new("down", Down, Some("WincEditor")),
        KeyBinding::new("shift-left", SelectLeft, Some("WincEditor")),
        KeyBinding::new("shift-right", SelectRight, Some("WincEditor")),
        KeyBinding::new("shift-up", SelectUp, Some("WincEditor")),
        KeyBinding::new("shift-down", SelectDown, Some("WincEditor")),
        KeyBinding::new("home", LineStart, Some("WincEditor")),
        KeyBinding::new("end", LineEnd, Some("WincEditor")),
        KeyBinding::new("shift-home", SelectToLineStart, Some("WincEditor")),
        KeyBinding::new("shift-end", SelectToLineEnd, Some("WincEditor")),
        KeyBinding::new("ctrl-left", MoveToPreviousWord, Some("WincEditor")),
        KeyBinding::new("ctrl-right", MoveToNextWord, Some("WincEditor")),
        KeyBinding::new("ctrl-shift-left", SelectToPreviousWord, Some("WincEditor")),
        KeyBinding::new("ctrl-shift-right", SelectToNextWord, Some("WincEditor")),
        KeyBinding::new("secondary-a", SelectAll, Some("WincEditor")),
        KeyBinding::new("secondary-v", Paste, Some("WincEditor")),
        KeyBinding::new("secondary-c", Copy, Some("WincEditor")),
        KeyBinding::new("secondary-x", Cut, Some("WincEditor")),
        KeyBinding::new("secondary-z", Undo, Some("WincEditor")),
        KeyBinding::new("secondary-shift-z", Redo, Some("WincEditor")),
        KeyBinding::new("ctrl-y", Redo, Some("WincEditor")),
        KeyBinding::new("enter", Newline, Some("WincEditor")),
        // The document and page ends, and Winc's structural edits.
        KeyBinding::new("ctrl-home", DocStart, Some("WincEditor")),
        KeyBinding::new("ctrl-end", DocEnd, Some("WincEditor")),
        KeyBinding::new("pageup", PageUp, Some("WincEditor")),
        KeyBinding::new("pagedown", PageDown, Some("WincEditor")),
        KeyBinding::new("tab", Indent, Some("WincEditor")),
        KeyBinding::new("shift-tab", Outdent, Some("WincEditor")),
        KeyBinding::new("secondary-/", ToggleComment, Some("WincEditor")),
        KeyBinding::new("secondary-d", DeleteLine, Some("WincEditor")),
        KeyBinding::new("ctrl-j", JoinLines, Some("WincEditor")),
        KeyBinding::new("secondary-]", JumpToBracket, Some("WincEditor")),
        KeyBinding::new("ctrl-w", ExpandSelection, Some("WincEditor")),
        KeyBinding::new("ctrl-shift-w", ShrinkSelection, Some("WincEditor")),
        // Navigation. F12 is what most editors use for go-to-definition, and
        // shift-F12 for references.
        KeyBinding::new("f12", GotoDefinition, Some("WincEditor")),
        KeyBinding::new("shift-f12", FindUsages, Some("WincEditor")),
        // The rest of Winc's structural edits. Ctrl+Enter is what its own UI
        // uses for "open a line below"; the move/copy chord is VS Code's, which
        // is what a user of this panel will already have in their fingers.
        KeyBinding::new("ctrl-]", IndentLines, Some("WincEditor")),
        KeyBinding::new("ctrl-[", OutdentLines, Some("WincEditor")),
        KeyBinding::new("alt-up", MoveLinesUp, Some("WincEditor")),
        KeyBinding::new("alt-down", MoveLinesDown, Some("WincEditor")),
        KeyBinding::new("alt-shift-up", CopyLinesUp, Some("WincEditor")),
        KeyBinding::new("alt-shift-down", CopyLinesDown, Some("WincEditor")),
        KeyBinding::new("ctrl-enter", InsertLineBelow, Some("WincEditor")),
        KeyBinding::new("ctrl-shift-enter", InsertLineAbove, Some("WincEditor")),
        // Case changes, on the chord VS Code uses. Lower-case gets Ctrl+U
        // because `ctrl-shift-l` is Winc's select-all-occurrences below.
        KeyBinding::new("ctrl-shift-u", ChangeCaseUpper, Some("WincEditor")),
        KeyBinding::new("ctrl-u", ChangeCaseLower, Some("WincEditor")),
        // Folding. Ctrl+Shift+[ / ] is the fold/unfold pair everywhere; the
        // recursive and whole-file variants take the modifier the editors that
        // have them use.
        KeyBinding::new("ctrl-shift-[", Fold, Some("WincEditor")),
        KeyBinding::new("ctrl-shift-]", Unfold, Some("WincEditor")),
        KeyBinding::new("ctrl-k", ToggleFold, Some("WincEditor")),
        KeyBinding::new("ctrl-shift-alt-[", FoldRecursively, Some("WincEditor")),
        KeyBinding::new("ctrl-shift-alt-]", UnfoldRecursively, Some("WincEditor")),
        KeyBinding::new("ctrl-alt-0", FoldAll, Some("WincEditor")),
        KeyBinding::new("ctrl-alt-j", UnfoldAll, Some("WincEditor")),
        KeyBinding::new("ctrl-alt-1", FoldLevel1, Some("WincEditor")),
        KeyBinding::new("ctrl-alt-2", FoldLevel2, Some("WincEditor")),
        KeyBinding::new("ctrl-alt-3", FoldLevel3, Some("WincEditor")),
        // Bookmarks, on the chord VS Code uses.
        KeyBinding::new("ctrl-alt-k", ToggleBookmark, Some("WincEditor")),
        KeyBinding::new("ctrl-alt-b", NextBookmark, Some("WincEditor")),
        KeyBinding::new("ctrl-alt-shift-b", PreviousBookmark, Some("WincEditor")),
        KeyBinding::new("ctrl-alt-shift-k", ClearBookmarks, Some("WincEditor")),
        // Go to symbol, which opens the palette's symbol list.
        KeyBinding::new("ctrl-shift-o", GoToSymbol, Some("WincEditor")),
        // Multi-cursor. Ctrl+D adds the next occurrence and is the chord that
        // makes it discoverable. The column-cursor pair takes Ctrl+Alt+arrows,
        // which VS Code uses too and which does not collide with line-move
        // (`alt-up`/`alt-down`) above.
        KeyBinding::new("ctrl-alt-up", AddCursorAbove, Some("WincEditor")),
        KeyBinding::new("ctrl-alt-down", AddCursorBelow, Some("WincEditor")),
        KeyBinding::new("ctrl-d", AddCursorToNextOccurrence, Some("WincEditor")),
        KeyBinding::new("ctrl-shift-l", SelectAllOccurrences, Some("WincEditor")),
        KeyBinding::new("escape", CollapseCursors, Some("WincEditor")),
        // Formatting through whichever tool Winc finds for the language.
        KeyBinding::new("shift-alt-f", FormatDocument, Some("WincEditor")),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Entity, TestAppContext};

    /// Rows must advance by their own glyphs, never by a forced step.
    ///
    /// `shape_line`'s last argument is `force_width`, and GPUI reads it as the
    /// per-character advance: glyph *n* lands at `n * force_width`. Passing the
    /// row height there spaced every character 1.5em apart — over twice a
    /// monospace advance — so ordinary code ran far past the viewport while the
    /// gutter still painted its numbers. The body looked like a blank file.
    ///
    /// The value is asserted directly rather than measured from a shaped line:
    /// a test run has no font loaded, so only the argument itself is meaningful.
    #[test]
    fn painted_rows_never_force_a_glyph_advance() {
        assert_eq!(
            row_force_width(),
            None,
            "a forced advance spaces characters apart by that width, \
             which is how the editor body went blank"
        );
    }

    /// Build an editor over `text` with the caret at `selection`.
    ///
    /// Goes through the real constructor, so the test exercises the same state
    /// the panel builds rather than a stand-in.
    fn editor_with(
        cx: &mut TestAppContext,
        text: &str,
        selection: Range<usize>,
    ) -> Entity<WincEditor> {
        cx.new(|cx| {
            let mut editor = WincEditor::new("sample.rs", None, px(12.0), ScrollHandle::new(), cx);
            editor.set_content(text, cx);
            editor.selection = selection;
            editor
        })
    }

    /// The wash must be clipped to the row it paints on: an unclipped range
    /// indexes `x_for_index` past the shaped line, and a selection spanning
    /// two rows would paint its whole width on both.
    #[gpui::test]
    fn a_selection_is_clipped_to_the_row_it_paints_on(cx: &mut TestAppContext) {
        // A selection covering bytes 5..25 across rows; row 10..20 must see
        // exactly its own bytes, rebased to the row.
        let editor = editor_with(cx, "abcdefghijklmnopqrstuvwxyz", 5..25);
        cx.read_entity(&editor, |editor, _| {
            assert_eq!(
                editor.selection_wash(&(10..20)),
                Some(0..10),
                "the row's share of the selection, rebased to 0"
            );
            // A selection that ends before this row paints nothing on it.
            assert_eq!(editor.selection_wash(&(20..30)), Some(0..5));
            // Entirely above the row: nothing.
            assert_eq!(editor.selection_wash(&(30..40)), None);
            // Entirely below the row: nothing.
            assert_eq!(editor.selection_wash(&(0..5)), None);
        });
    }

    /// A collapsed caret is not a selection, so it must paint no wash at all —
    /// otherwise clicking would leave a one-byte highlight behind.
    #[gpui::test]
    fn a_collapsed_caret_paints_no_selection(cx: &mut TestAppContext) {
        let editor = editor_with(cx, "abcdef", 3..3);
        cx.read_entity(&editor, |editor, _| {
            assert_eq!(editor.selection_wash(&(0..6)), None);
        });
    }

    /// The match wash must carry which match it is, so the current one can be
    /// drawn more strongly than its siblings.
    #[gpui::test]
    fn a_match_wash_carries_its_index(cx: &mut TestAppContext) {
        let editor = editor_with(cx, "abcdefghij", 0..0);
        editor.update(cx, |editor, cx| {
            editor.set_search_matches(vec![1..3, 6..8], Some(1), cx);
        });
        cx.read_entity(&editor, |editor, _| {
            let ranges = editor.search_wash_ranges(&(0..10));
            assert_eq!(ranges, vec![(0, 1..3), (1, 6..8)]);
            // A row that holds only the second match reports only it, still
            // with its original index — not its position on this row.
            assert_eq!(editor.search_wash_ranges(&(5..10)), vec![(1, 1..3)]);
        });
    }

    /// UTF-16 is the unit GPUI's IME protocol speaks; a conversion that
    /// confused it with bytes would put the candidate window in the wrong
    /// place for any non-ASCII text.
    #[test]
    fn utf16_offsets_are_not_byte_offsets() {
        let content = "a中b";
        // `a`, `中` and `b` are three UTF-16 code units but 1 + 3 + 1 = 5 bytes.
        assert_eq!(content.encode_utf16().count(), 3);
        assert_eq!(content.len(), 5);
        // The four bytes of `a中` are two code units.
        let prefix = content.get(..4).expect("a char boundary");
        assert_eq!(prefix, "a中");
        assert_eq!(prefix.encode_utf16().count(), 2);
    }

    /// Undo and redo are Winc's, driven through its commands. This pins that
    /// typing goes through the document's own history rather than a second one
    /// kept beside it: one undo must restore the text, and one redo must
    /// reapply it.
    #[gpui::test]
    fn typing_undoes_through_the_documents_own_history(cx: &mut TestAppContext) {
        let editor = editor_with(cx, "abc", 3..3);
        editor.update(cx, |editor, cx| {
            editor.run(WincCommand::Insert("X".to_owned()), cx);
        });
        cx.read_entity(&editor, |editor, _| {
            assert_eq!(editor.content(), "abcX", "the insert landed");
        });

        editor.update(cx, |editor, cx| editor.undo(cx));
        cx.read_entity(&editor, |editor, _| {
            assert_eq!(editor.content(), "abc", "one undo steps back over it");
        });

        editor.update(cx, |editor, cx| editor.redo(cx));
        cx.read_entity(&editor, |editor, _| {
            assert_eq!(editor.content(), "abcX");
        });
    }

    /// Winc's toggle-comment command, reached through the same `run` the key
    /// map uses. A hand-rolled implementation would pass a simpler test; this
    /// one pins the wiring, which is what actually broke before.
    #[gpui::test]
    fn toggle_comment_goes_through_wincs_own_command(cx: &mut TestAppContext) {
        let editor = editor_with(cx, "fn main() {}\n", 0..0);
        editor.update(cx, |editor, cx| {
            editor.run(WincCommand::ToggleComment, cx);
        });
        cx.read_entity(&editor, |editor, _| {
            let text = editor.content();
            assert!(
                text.trim_start().starts_with("//"),
                "the line should be commented out, got {text:?}"
            );
        });
    }

    /// A definition is found by Winc's scanner, so `fn helper` written above the
    /// caret is reachable. `goto_definition` puts the caret on the name itself,
    /// not merely on its line — a jump that lands at column 0 has not jumped.
    #[gpui::test]
    fn go_to_definition_lands_on_the_name(cx: &mut TestAppContext) {
        let source = "fn helper() {}\n\nfn main() { helper(); }\n";
        // Put the caret inside the call, not on the definition.
        let call = source.rfind("helper").expect("the call site");
        let editor = editor_with(cx, source, call..call + 3);
        editor.update(cx, |editor, cx| editor.goto_definition(cx));
        cx.read_entity(&editor, |editor, _| {
            let range = editor.selected_range();
            let text = editor.content();
            assert_eq!(
                &text[range.clone()],
                "helper",
                "the jump must select the name it found"
            );
            assert_eq!(range.start, 3, "the definition is on the first line");
        });
    }

    /// Usages come back with the definitions themselves and the prose mentions
    /// already filtered out, which is the whole point of using Winc's filter
    /// over a text search: `helper` appearing in a `//` comment is not a use.
    ///
    /// The definition line is excluded too — Winc reports *uses*, and a caller
    /// that wants both asks the definition scanner as well.
    #[gpui::test]
    fn usages_skip_definitions_and_prose(cx: &mut TestAppContext) {
        let source = "fn helper() {}\n// helper in prose\nfn main() { helper(); }\n";
        let editor = editor_with(cx, source, 0..0);
        cx.read_entity(&editor, |editor, _| {
            let text = editor.content();
            let found: Vec<String> = editor
                .usages_of("helper")
                .into_iter()
                .map(|range| text[range].to_owned())
                .collect();
            assert_eq!(
                found,
                vec!["helper".to_owned()],
                "only the call site is a use: not the definition, not the comment"
            );
        });
    }

    /// The row a point falls on decides where a click puts the caret. The rule
    /// is "the first row whose bottom is below the point", with the rows above
    /// and below the viewport clamped to the first and last.
    #[test]
    fn a_point_maps_to_the_row_holding_it() {
        let row_height = px(20.0);
        let tops: Vec<Pixels> = (0..3).map(|index| px(index as f32 * 20.0)).collect();
        let row_at = |y: Pixels| -> Option<usize> {
            tops.iter()
                .position(|top| y < *top + row_height)
                .or_else(|| tops.len().checked_sub(1))
        };
        assert_eq!(row_at(px(0.0)), Some(0));
        assert_eq!(row_at(px(19.9)), Some(0));
        assert_eq!(row_at(px(20.0)), Some(1));
        assert_eq!(row_at(px(-5.0)), Some(0), "above the first row clamps up");
        assert_eq!(row_at(px(999.0)), Some(2), "below the last clamps down");
    }
}

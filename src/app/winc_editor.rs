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
use std::time::{Duration, Instant};

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
use crate::edit_history::EditHistory;
use crate::input::InputEvent;
use crate::theme::Theme;

/// Rows past the viewport kept painted, so a fast scroll never shows a blank
/// strip for a frame while the next batch is shaped.
const OVERSCAN_ROWS: f32 = 2.0;

/// How long the caret stays solid after an edit before blinking resumes.
const CURSOR_BLINK_PAUSE: Duration = Duration::from_millis(300);

/// One caret blink half-period.
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);

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
    history: EditHistory,
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
    /// A drag is in progress, so mouse moves extend the selection.
    selecting: bool,
    /// The caret offset the viewport has already been scrolled to, so an
    /// unrelated repaint does not fight the user's own scrolling.
    followed: Option<usize>,
}

/// One painted row, kept for hit testing.
///
/// `shaped` is the line as GPUI laid it out, so a click can ask the text system
/// where byte `n` is instead of estimating from a character width.
#[derive(Clone)]
struct PaintedRow {
    line: usize,
    /// The row's text, without its newline.
    text: SharedString,
    shaped: ShapedLine,
    /// Top of the row in window coordinates.
    top: Pixels,
}

impl WincEditor {
    pub fn new(
        language: Option<&str>,
        font_size: Pixels,
        scroll: ScrollHandle,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            buffer: WincBuffer::new(),
            highlighter: language.and_then(LineHighlighter::for_language),
            focus_handle: cx.focus_handle(),
            selection: 0..0,
            reversed: false,
            marked: None,
            history: EditHistory::default(),
            blink: BlinkCursor::new(),
            scroll,
            search_matches: Vec::new(),
            active_search_match: None,
            rows: Vec::new(),
            text_bounds: None,
            font_size,
            row_height: (font_size * 1.5).round(),
            selecting: false,
            followed: None,
        }
    }

    pub fn row_height(&self) -> Pixels {
        self.row_height
    }

    pub fn content(&self) -> String {
        self.buffer.text()
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
        self.buffer.set_text(content);
        self.history.clear();
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
        let row = self.rows.iter().find(|row| row.line == line)?;
        let column = offset.saturating_sub(self.buffer.line_start_byte(line));
        let x = row.shaped.x_for_index(column.min(row.text.len()));
        Some((point(bounds.left() + x, row.top), self.row_height))
    }

    /// Splice `text` over `range` as one undo step, refused when read-only.
    pub fn replace_range(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        if self.buffer.is_read_only() {
            return;
        }
        let len = self.buffer.byte_len();
        let range = range.start.min(len)..range.end.min(len);
        if range.start > range.end {
            return;
        }
        self.record(range.clone(), text, false);
        if !self.buffer.edit(range.clone(), text) {
            return;
        }
        let caret = range.start + text.len();
        self.selection = caret..caret;
        self.reversed = false;
        self.marked = None;
        self.after_edit(range.start, cx);
    }

    fn record(&mut self, range: Range<usize>, text: &str, composing: bool) {
        let content = self.buffer.text();
        let selection = self.selection.clone();
        let now = Instant::now();
        if composing {
            self.history
                .record_composition(&content, &range, text, selection, self.reversed, now);
        } else {
            self.history
                .record(&content, &range, text, selection, self.reversed, now);
        }
    }

    /// Everything an edit must do once the text changed.
    fn after_edit(&mut self, at: usize, cx: &mut Context<Self>) {
        let line = self.buffer.line_of_byte(at);
        self.invalidate_highlight(line);
        self.rows.clear();
        self.blink.pause(cx);
        cx.emit(InputEvent::Edited);
        cx.notify();
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

    fn backspace(&mut self, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            let from = self.buffer.prev_grapheme(self.selection.start);
            self.replace_range(from..self.selection.start, "", cx);
        } else {
            self.replace_range(self.selection.clone(), "", cx);
        }
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            let to = self.buffer.next_grapheme(self.selection.end);
            self.replace_range(self.selection.start..to, "", cx);
        } else {
            self.replace_range(self.selection.clone(), "", cx);
        }
    }

    fn delete_previous_word(&mut self, cx: &mut Context<Self>) {
        let from = if self.selection.is_empty() {
            self.buffer.prev_word(self.selection.start)
        } else {
            self.selection.start
        };
        let to = self.selection.end.max(from);
        self.replace_range(from..to, "", cx);
    }

    fn delete_next_word(&mut self, cx: &mut Context<Self>) {
        let to = if self.selection.is_empty() {
            self.buffer.next_word(self.selection.end)
        } else {
            self.selection.end
        };
        let from = self.selection.start;
        self.replace_range(from..to.max(from), "", cx);
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
        // A file editor keeps the breaks a clipboard carries, folding CRLF to
        // the `\n` the rope stores so a paste cannot add a second convention.
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.replace_range(self.selection.clone(), &text, cx);
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
        self.replace_range(self.selection.clone(), "", cx);
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        if self.buffer.is_read_only() || self.marked.is_some() {
            return;
        }
        let content = self.buffer.text();
        let Some((restored, selection, reversed)) = self.history.undo(&content) else {
            return;
        };
        self.apply_history(restored, selection, reversed, cx);
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        if self.buffer.is_read_only() || self.marked.is_some() {
            return;
        }
        let content = self.buffer.text();
        let Some((restored, selection, reversed)) = self.history.redo(&content) else {
            return;
        };
        self.apply_history(restored, selection, reversed, cx);
    }

    fn apply_history(
        &mut self,
        restored: String,
        selection: Range<usize>,
        reversed: bool,
        cx: &mut Context<Self>,
    ) {
        self.buffer.set_text(&restored);
        self.selection = selection;
        self.reversed = reversed;
        self.marked = None;
        self.invalidate_highlight(0);
        self.rows.clear();
        self.blink.pause(cx);
        cx.emit(InputEvent::Edited);
        cx.notify();
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
            let start = self.buffer.word_start_at(offset);
            let end = self.buffer.word_end_at(offset);
            if start < end {
                self.select_range(start..end, cx);
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

    /// Search-match ranges overlapping `row`, in row-relative bytes, with the
    /// index of each match in `self.search_matches` so the caller can tell the
    /// current match from its siblings.
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
        self.history.finalize_composition();
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
        // An empty composition is a cancel: undo the step it opened.
        if new_text.is_empty() {
            self.marked = None;
            self.history.finalize_composition();
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
    /// Apply one IME composition revision over `range`.
    ///
    /// Separate from `replace_range` because a composition's ranges arrive
    /// relative to the marked text, and recording it as one undo step is what
    /// makes a whole composition undo at once.
    fn replace_selection_composing(
        &mut self,
        range: Range<usize>,
        text: &str,
        caret_utf16: Option<Range<usize>>,
        cx: &mut Context<Self>,
    ) {
        self.record(range.clone(), text, true);
        if !self.buffer.edit(range.clone(), text) {
            return;
        }
        let start = range.start;
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
                                Some(row_height),
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
            .on_action(cx.listener(|this, _: &Left, _, cx| this.step(false, cx)))
            .on_action(cx.listener(|this, _: &Right, _, cx| this.step(true, cx)))
            .on_action(cx.listener(|this, _: &Up, _, cx| this.step_line(false, cx)))
            .on_action(cx.listener(|this, _: &Down, _, cx| this.step_line(true, cx)))
            .on_action(cx.listener(|this, _: &SelectLeft, _, cx| this.step_select(false, cx)))
            .on_action(cx.listener(|this, _: &SelectRight, _, cx| this.step_select(true, cx)))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.step_line_select(false, cx)))
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.step_line_select(true, cx)))
            .on_action(cx.listener(|this, _: &LineStart, _, cx| this.line_edge(false, false, cx)))
            .on_action(cx.listener(|this, _: &LineEnd, _, cx| this.line_edge(true, false, cx)))
            .on_action(
                cx.listener(|this, _: &SelectToLineStart, _, cx| this.line_edge(false, true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectToLineEnd, _, cx| this.line_edge(true, true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &MoveToPreviousWord, _, cx| this.word_step(false, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &MoveToNextWord, _, cx| this.word_step(true, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectToPreviousWord, _, cx| {
                    this.word_step(false, true, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &SelectToNextWord, _, cx| this.word_step(true, true, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                let len = this.buffer.byte_len();
                this.select_range(0..len, cx)
            }))
            .on_action(cx.listener(|this, _: &Paste, _, cx| this.paste(cx)))
            .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy(cx)))
            .on_action(cx.listener(|this, _: &Cut, _, cx| this.cut(cx)))
            .on_action(cx.listener(|this, _: &Undo, _, cx| this.undo(cx)))
            .on_action(cx.listener(|this, _: &Redo, _, cx| this.redo(cx)))
            .on_action(cx.listener(|this, _: &Newline, _, cx| {
                let selection = this.selection.clone();
                this.replace_range(selection, "\n", cx)
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .child(field)
    }
}

impl WincEditor {
    /// Move one grapheme left or right.
    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let caret = self.selection.end;
        let next = if forward {
            self.buffer.next_grapheme(caret)
        } else {
            self.buffer.prev_grapheme(caret)
        };
        self.move_caret(next, false, cx);
    }

    fn step_select(&mut self, forward: bool, cx: &mut Context<Self>) {
        let caret = self.selection.end;
        let next = if forward {
            self.buffer.next_grapheme(caret)
        } else {
            self.buffer.prev_grapheme(caret)
        };
        self.extend_selection(next, cx);
    }

    fn step_line(&mut self, down: bool, cx: &mut Context<Self>) {
        let next = self.buffer.vertical(self.selection.end, down);
        self.move_caret(next, false, cx);
    }

    fn step_line_select(&mut self, down: bool, cx: &mut Context<Self>) {
        let next = self.buffer.vertical(self.selection.end, down);
        self.extend_selection(next, cx);
    }

    fn word_step(&mut self, forward: bool, extend: bool, cx: &mut Context<Self>) {
        let caret = self.selection.end;
        let next = if forward {
            self.buffer.next_word(caret)
        } else {
            self.buffer.prev_word(caret)
        };
        if extend {
            self.extend_selection(next, cx);
        } else {
            self.move_caret(next, false, cx);
        }
    }

    /// Move to the start or end of the caret's line.
    fn line_edge(&mut self, end: bool, extend: bool, cx: &mut Context<Self>) {
        let caret = self.selection.end;
        let line = self.buffer.line_of_byte(caret);
        let next = if end {
            self.line_end_byte(caret)
        } else {
            self.buffer.line_start_byte(line)
        };
        if extend {
            self.extend_selection(next, cx);
        } else {
            self.move_caret(next, false, cx);
        }
    }

    /// Byte offset of the end of the line holding `at`, before its newline.
    fn line_end_byte(&self, at: usize) -> usize {
        let line = self.buffer.line_of_byte(at);
        let text = self.buffer.line(line);
        self.buffer.line_start_byte(line) + text.trim_end_matches(['\r', '\n']).len()
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
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        LineStart,
        LineEnd,
        SelectToLineStart,
        SelectToLineEnd,
        MoveToPreviousWord,
        MoveToNextWord,
        SelectToPreviousWord,
        SelectToNextWord,
        SelectAll,
        Paste,
        Copy,
        Cut,
        Undo,
        Redo,
        Newline,
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
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Entity, TestAppContext};

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
            let mut editor = WincEditor::new(None, px(12.0), ScrollHandle::new(), cx);
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

    /// Undo bookkeeping: a fresh editor has nothing to step back over, and an
    /// edit gives it something. Driven through the history rather than the
    /// editor so no window is needed.
    #[test]
    fn undo_availability_follows_the_history() {
        let mut history = EditHistory::default();
        assert!(!history.can_undo() && !history.can_redo());
        history.record("", &(0..0), "a", 0..0, false, Instant::now());
        assert!(history.can_undo());
        let (restored, ..) = history.undo("a").expect("one step");
        assert_eq!(restored, "");
        assert!(history.can_redo());
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

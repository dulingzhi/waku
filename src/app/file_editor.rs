//! What Waku's right panel needs from a file editor, independent of which
//! editor implements it.
//!
//! The panel owns everything around the text: when to read the file from disk,
//! when a read is stale, when a save may overwrite, and how the tab's dirty dot
//! is decided. None of that depends on how the text is stored, so it is stated
//! once here and the editor supplies only the operations below.
//!
//! ## Why a trait and not `Entity<TextInput>`
//!
//! The panel's current editor is `TextInput`, a plain multi-line field: one
//! `SharedString`, one cursor, and a token pass per line for colour. A real
//! editor stores lines in a rope, tracks many cursors, folds and a search index.
//! Both can serve the panel, but only once the panel stops naming one of them.
//!
//! The trait is deliberately narrow. Anything wider — selections, cursors,
//! scroll positions, undo — would bake today's implementation into tomorrow's
//! interface, and the panel genuinely does not need any of it, because the
//! editor draws itself.
//!
//! ## Why content is a `String` and not a `Rope`
//!
//! A rope-backed editor cannot honestly hand out a `&str` of the whole file, and
//! the panel's two callers both want an owned value anyway: the dirty check
//! compares against the last text read from disk, and a save sends that text to
//! the daemon. Returning `String` costs one copy per save and per dirty check,
//! which is the price of not knowing how the text is stored.
//!
//! ## Offsets are bytes
//!
//! Every offset and range in this interface is a **byte** offset into the
//! editor's text, matching `str` indexing. The panel and its find bar slice the
//! text with these values, so an editor that counts in anything else must
//! convert at its own boundary.
//!
//! This matters more than it looks. Winc's `winc_core` counts positions in
//! **chars**, and its `Lines`/`EditBuffer` traits are documented in those terms.
//! A backend built on it therefore has to translate both ways: a byte offset
//! from the find bar becomes a char index before it reaches the rope, and a
//! position the rope reports becomes a byte offset before it comes back. Skipping
//! that translation is not a cosmetic bug — on any text with non-ASCII characters
//! every match after the first multibyte character would point at the wrong
//! place, and a replace would splice into the middle of a code point.
//!
//! ## What an implementation must not do
//!
//! * Read or write the filesystem. The panel decides when a read is stale (its
//!   own epoch counter) and when a save is allowed; an editor that also touched
//!   the disk would race those decisions from underneath.
//! * Report itself dirty by comparing text to a file it read itself. Dirty is
//!   the panel's verdict, because only it knows what the last disk read was and
//!   whether the buffer is writable at all.

use std::ops::Range;

use gpui::{FocusHandle, SharedString};

use super::Waku;

/// The editor behind one open file in the right panel.
pub trait FileEditorBackend {
    /// Downcast hook for the render path, which has to know what it is drawing.
    ///
    /// Behaviour never uses this: it goes through the methods below. Only a
    /// view that must hand the concrete widget to GPUI does.
    fn as_any(&self) -> &dyn std::any::Any;

    /// The text the editor is showing right now, as a copy.
    fn content(&mut self, cx: &mut gpui::Context<Waku>) -> String;

    /// Replace the whole text, as when a file is first read or reloaded.
    ///
    /// The caller has already decided this is safe: no unsaved edit exists, and
    /// no newer read superseded this one.
    fn set_content(&mut self, content: SharedString, cx: &mut gpui::Context<Waku>);

    /// Make the editor refuse edits, or accept them again.
    ///
    /// Called when the file is not writable, and when a read is still in flight:
    /// an editor that accepted input before its file arrived would report text
    /// that never existed on disk.
    fn set_read_only(&mut self, read_only: bool, cx: &mut gpui::Context<Waku>);

    /// Whether the panel has decided this editor holds unsaved changes.
    fn is_dirty(&self) -> bool;

    /// Record the panel's dirty verdict.
    ///
    /// Separate from [`Self::is_dirty`] because only the panel can compute it:
    /// it needs the text the file had when it was read.
    fn set_dirty(&mut self, dirty: bool);

    /// The handle the panel focuses when it hands the keyboard back to the
    /// editor, as closing the find bar does.
    fn focus_handle(&mut self, cx: &mut gpui::Context<Waku>) -> FocusHandle;

    /// The selected byte range, so the find bar can seed itself from a
    /// single-line selection the way editors conventionally do.
    fn selected_range(&mut self, cx: &mut gpui::Context<Waku>) -> Range<usize>;

    /// Select a byte range, as navigating to a search match does.
    fn select_range(&mut self, range: Range<usize>, cx: &mut gpui::Context<Waku>);

    /// Where `offset` sits in the editor's last painted layout, with the height
    /// of the row carrying it.
    ///
    /// `None` before the first paint, or for an offset the editor cannot place —
    /// the find bar then leaves scrolling alone rather than guessing.
    fn position_for_offset(
        &mut self,
        offset: usize,
        cx: &mut gpui::Context<Waku>,
    ) -> Option<(gpui::Point<gpui::Pixels>, gpui::Pixels)>;

    /// The caret's byte offset.
    fn cursor(&mut self, cx: &mut gpui::Context<Waku>) -> usize;

    /// Replace a byte range with `text`, as a replace-all does.
    fn replace_range(&mut self, range: Range<usize>, text: &str, cx: &mut gpui::Context<Waku>);

    /// Paint match washes under the text, with `active` stronger than its
    /// siblings, and navigate to it.
    ///
    /// Owned by the find bar, which recomputes the ranges whenever the query or
    /// the text changes; the editor only paints them.
    fn set_search_matches(
        &mut self,
        matches: Vec<Range<usize>>,
        active: Option<usize>,
        cx: &mut gpui::Context<Waku>,
    );
}

/// The editor the panel used before the rope-backed one.
///
/// It is no longer what opening a file gets — the panel builds a
/// [`WincEditorBackend`] now — but it stays as the reference the trait was
/// written against, and as the shape a fallback would take. Its own tests below
/// keep that role honest, which is why constructing it is not dead code.
pub struct TextInputBackend {
    input: gpui::Entity<crate::input::TextInput>,
    dirty: bool,
}

impl TextInputBackend {
    /// Build one over an existing field.
    ///
    /// The panel builds a [`WincEditorBackend`] now, so nothing calls this on a
    /// render path; it stays for tests that hold the reference implementation
    /// to the same contract.
    #[allow(
        dead_code,
        reason = "constructs the reference backend the trait's tests use"
    )]
    pub fn new(input: gpui::Entity<crate::input::TextInput>) -> Self {
        Self {
            input,
            dirty: false,
        }
    }

    pub fn entity(&self) -> &gpui::Entity<crate::input::TextInput> {
        &self.input
    }
}

impl FileEditorBackend for TextInputBackend {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn content(&mut self, cx: &mut gpui::Context<Waku>) -> String {
        self.input.read(cx).content().to_owned()
    }

    fn set_content(&mut self, content: SharedString, cx: &mut gpui::Context<Waku>) {
        self.input
            .update(cx, |input, cx| input.set_content(content, cx));
    }

    fn set_read_only(&mut self, read_only: bool, cx: &mut gpui::Context<Waku>) {
        self.input
            .update(cx, |input, _| input.set_read_only(read_only));
    }

    fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn set_dirty(&mut self, dirty: bool) {
        self.dirty = dirty;
    }

    fn focus_handle(&mut self, cx: &mut gpui::Context<Waku>) -> FocusHandle {
        self.input.read(cx).focus()
    }

    fn selected_range(&mut self, cx: &mut gpui::Context<Waku>) -> Range<usize> {
        self.input.read(cx).selected_range()
    }

    fn select_range(&mut self, range: Range<usize>, cx: &mut gpui::Context<Waku>) {
        self.input
            .update(cx, |input, cx| input.select_range(range, cx));
    }

    fn position_for_offset(
        &mut self,
        offset: usize,
        cx: &mut gpui::Context<Waku>,
    ) -> Option<(gpui::Point<gpui::Pixels>, gpui::Pixels)> {
        self.input.read(cx).position_for_offset(offset)
    }

    fn cursor(&mut self, cx: &mut gpui::Context<Waku>) -> usize {
        self.input.read(cx).cursor()
    }

    fn replace_range(&mut self, range: Range<usize>, text: &str, cx: &mut gpui::Context<Waku>) {
        self.input
            .update(cx, |input, cx| input.replace_range(range, text, cx));
    }

    fn set_search_matches(
        &mut self,
        matches: Vec<Range<usize>>,
        active: Option<usize>,
        cx: &mut gpui::Context<Waku>,
    ) {
        self.input.update(cx, |input, cx| {
            input.set_search_matches(matches, active, cx)
        });
    }
}

/// The rope-backed editor: Winc's headless kernel, drawn by Waku.
///
/// This is the backend the integration adds. It keeps a [`WincEditor`] entity,
/// so the panel still talks to the trait above while the view draws itself.
pub struct WincEditorBackend {
    editor: gpui::Entity<super::winc_editor::WincEditor>,
    dirty: bool,
}

impl WincEditorBackend {
    pub fn new(editor: gpui::Entity<super::winc_editor::WincEditor>) -> Self {
        Self {
            editor,
            dirty: false,
        }
    }

    pub fn entity(&self) -> &gpui::Entity<super::winc_editor::WincEditor> {
        &self.editor
    }
}

impl FileEditorBackend for WincEditorBackend {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn content(&mut self, cx: &mut gpui::Context<Waku>) -> String {
        self.editor.read(cx).content()
    }

    fn set_content(&mut self, content: SharedString, cx: &mut gpui::Context<Waku>) {
        self.editor
            .update(cx, |editor, cx| editor.set_content(content.as_ref(), cx));
    }

    fn set_read_only(&mut self, read_only: bool, cx: &mut gpui::Context<Waku>) {
        self.editor
            .update(cx, |editor, cx| editor.set_read_only(read_only, cx));
    }

    fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn set_dirty(&mut self, dirty: bool) {
        self.dirty = dirty;
    }

    fn focus_handle(&mut self, cx: &mut gpui::Context<Waku>) -> FocusHandle {
        let editor = self.editor.read(cx);
        gpui::Focusable::focus_handle(editor, cx)
    }

    fn selected_range(&mut self, cx: &mut gpui::Context<Waku>) -> Range<usize> {
        self.editor.read(cx).selected_range()
    }

    fn select_range(&mut self, range: Range<usize>, cx: &mut gpui::Context<Waku>) {
        self.editor
            .update(cx, |editor, cx| editor.select_range(range, cx));
    }

    fn position_for_offset(
        &mut self,
        offset: usize,
        cx: &mut gpui::Context<Waku>,
    ) -> Option<(gpui::Point<gpui::Pixels>, gpui::Pixels)> {
        self.editor.read(cx).position_for_offset(offset)
    }

    fn cursor(&mut self, cx: &mut gpui::Context<Waku>) -> usize {
        self.editor.read(cx).cursor()
    }

    fn replace_range(&mut self, range: Range<usize>, text: &str, cx: &mut gpui::Context<Waku>) {
        self.editor
            .update(cx, |editor, cx| editor.replace_range(range, text, cx));
    }

    fn set_search_matches(
        &mut self,
        matches: Vec<Range<usize>>,
        active: Option<usize>,
        cx: &mut gpui::Context<Waku>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.set_search_matches(matches, active, cx)
        });
    }
}

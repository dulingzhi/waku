//! Go to symbol for the right panel's file editor.
//!
//! The entries come from Winc's own scan ([`winc_editor::symbols`]): a regex
//! pass that knows about twenty languages and infers nesting from indentation.
//! It is not semantic analysis — Winc has no language server — so what it can
//! find is a definition written the way its language usually writes one.
//!
//! ## Why the list is a popup rather than a palette
//!
//! Waku's command palette is scoped to a session: it lists canvas commands,
//! sessions and models, and searches through the daemon. A file's symbols are a
//! different namespace entirely, so they get their own list over the editor
//! instead of sharing that machinery.
//!
//! ## Why the scan runs on open
//!
//! A scan reads every line, so running it per frame would make a large file
//! crawl while the list is up. It runs once when the list opens and the result
//! is kept until the list closes or the file changes under it.

use super::*;

impl Waku {
    // Action handlers, registered on the root in `render`.

    pub(super) fn toggle_file_symbols_action(
        &mut self,
        _: &ToggleFileSymbols,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.file_hint = None;
        self.toggle_file_symbols(cx);
    }

    pub(super) fn file_symbol_next_action(
        &mut self,
        _: &FileSymbolNext,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.file_symbols.is_none() {
            cx.propagate();
            return;
        }
        self.file_symbols_step(1, cx);
    }

    pub(super) fn file_symbol_previous_action(
        &mut self,
        _: &FileSymbolPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.file_symbols.is_none() {
            cx.propagate();
            return;
        }
        self.file_symbols_step(-1, cx);
    }

    pub(super) fn accept_file_symbol_action(
        &mut self,
        _: &AcceptFileSymbol,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.file_symbols.is_none() {
            cx.propagate();
            return;
        }
        self.accept_file_symbol(cx);
    }

    pub(super) fn close_file_symbols_action(
        &mut self,
        _: &CloseFileSymbols,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.file_symbols.is_none() {
            // Unhandled, so escape falls through to whatever else is bound.
            cx.propagate();
            return;
        }
        self.close_file_symbols(cx);
    }
}

impl Waku {
    /// Open the symbol list for the visible file, or close it if it is open.
    pub(super) fn toggle_file_symbols(&mut self, cx: &mut Context<Self>) {
        if self.file_symbols.is_some() {
            self.close_file_symbols(cx);
            return;
        }
        let Some(path) = self.visible_right_panel_file_path() else {
            return;
        };
        let Some(editor) = self.right_panel_file_editors.get_mut(&path) else {
            return;
        };
        let EditorSurface::Winc(entity) = editor.surface() else {
            return;
        };
        let entries = entity.read(cx).symbols();
        if entries.is_empty() {
            // Nothing to list. Say so instead of opening an empty box, which
            // reads as a broken shortcut.
            self.show_file_hint(tr!("files.no_symbols"));
            cx.notify();
            return;
        }
        let entries: Vec<SymbolEntry> = entries
            .into_iter()
            .map(|symbol| SymbolEntry {
                name: symbol.name,
                kind: symbol.kind,
                indent: symbol.indent,
                line: symbol.line,
                offset: symbol.offset,
            })
            .collect();
        self.file_symbols = Some(FileSymbolPicker {
            relative_path: path,
            entries,
            selected: 0,
            scroll: ScrollHandle::new(),
            scrollbar: Rc::new(ScrollbarState::default()),
        });
        cx.notify();
    }

    pub(super) fn close_file_symbols(&mut self, cx: &mut Context<Self>) {
        if self.file_symbols.take().is_some() {
            cx.notify();
        }
    }

    /// Move the picker's selection by `delta`, clamped to the list.
    pub(super) fn file_symbols_step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(picker) = self.file_symbols.as_mut() else {
            return;
        };
        if picker.entries.is_empty() {
            return;
        }
        let last = picker.entries.len() - 1;
        let next = picker.selected as isize + delta;
        picker.selected = next.clamp(0, last as isize) as usize;
        cx.notify();
    }

    /// Jump to the selected symbol and close the list.
    pub(super) fn accept_file_symbol(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = self.file_symbols.as_ref() else {
            return;
        };
        let Some(symbol) = picker.entries.get(picker.selected) else {
            return;
        };
        let path = picker.relative_path.clone();
        let index = picker.selected;
        let offset = symbol.offset;
        self.file_symbols = None;
        let Some(editor) = self.right_panel_file_editors.get_mut(&path) else {
            return;
        };
        let EditorSurface::Winc(entity) = editor.surface() else {
            return;
        };
        // The jump itself is the editor's: it owns the selection, the caret and
        // the repaint, and `goto_symbol` is the one place that decides what
        // "landing on a symbol" means.
        entity.update(cx, |editor, cx| editor.goto_symbol(index, cx));
        let Some((position, line_height)) = editor.backend.position_for_offset(offset, cx) else {
            cx.notify();
            return;
        };
        let scroll = &self.right_panel_editor_scroll_handle;
        let viewport = scroll.bounds();
        let current = scroll.offset();
        if let Some(next) = file_search::revealed_scroll_offset(
            current.y,
            scroll.max_offset().y,
            position.y,
            line_height,
            viewport.top(),
            viewport.bottom(),
        ) {
            scroll.set_offset(point(current.x, next));
        }
        cx.notify();
    }

    /// A one-line notice shown in the panel, for a command that found nothing.
    pub(super) fn show_file_hint(&mut self, text: String) {
        self.file_hint = Some(text);
    }

    /// The symbol list, if one is open and its file is still the visible one.
    pub(super) fn visible_file_symbols(&mut self) -> Option<&mut FileSymbolPicker> {
        let visible = self.visible_right_panel_file_path();
        // Decided before the mutable borrow, so the stale case can clear the
        // field without borrowing it twice.
        let stale = self
            .file_symbols
            .as_ref()
            .is_some_and(|picker| visible.as_deref() != Some(picker.relative_path.as_str()));
        if stale {
            // The visible file changed under the list, so it would be listing
            // another file's symbols. It goes.
            self.file_symbols = None;
            return None;
        }
        self.file_symbols.as_mut()
    }

    /// Draw the symbol list over the editor.
    ///
    /// A list, not a search box: Winc's scan already produced every symbol, so
    /// typing filters nothing the user cannot see by scrolling. Arrow keys move
    /// the selection and Enter jumps.
    pub(super) fn render_file_symbols(
        &mut self,
        pane_width: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::current(cx);
        let text_size = self.state.code_font_size;
        let row_height = (text_size * 1.6).round();
        // Copied out before building anything: the rows carry `cx.listener`
        // closures, which borrow `cx`, so holding a borrow of `self` across
        // them would not compile.
        let (selected, scroll, scrollbar, entries) = {
            let picker = self.visible_file_symbols()?;
            let entries: Vec<(String, String, usize, usize)> = picker
                .entries
                .iter()
                .map(|entry| {
                    (
                        entry.name.clone(),
                        entry.kind.clone(),
                        entry.indent,
                        entry.line,
                    )
                })
                .collect();
            (
                picker.selected,
                picker.scroll.clone(),
                picker.scrollbar.clone(),
                entries,
            )
        };
        let total = entries.len();
        let rows = entries
            .into_iter()
            .enumerate()
            .map(|(index, (name, kind, indent, line))| {
                let is_selected = index == selected;
                div()
                    .id(SharedString::from(format!("file-symbol-{index}")))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(row_height))
                    .pl(px(8.0 + indent as f32 * 12.0))
                    .pr(px(8.0))
                    .when(is_selected, |row| row.bg(theme.selection))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(picker) = this.file_symbols.as_mut() {
                            picker.selected = index;
                        }
                        this.accept_file_symbol(cx);
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(if is_selected {
                                theme.text
                            } else {
                                theme.text_secondary
                            })
                            .child(SharedString::from(name)),
                    )
                    // The kind, then the line: enough to tell two same-named
                    // symbols apart without opening the file.
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.text_ghost)
                            .child(SharedString::from(kind)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(text_size * 2.6))
                            .text_right()
                            .text_color(theme.text_ghost)
                            .child(SharedString::from((line + 1).to_string())),
                    )
            });

        let width = (pane_width - 24.0).clamp(240.0, 520.0);
        Some(
            div()
                .key_context("FileSymbols")
                .absolute()
                .top(px(6.0))
                .right(px(16.0))
                .w(px(width))
                .max_h(px(row_height * 12.0))
                .flex()
                .flex_col()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.surface)
                .shadow_lg()
                .overflow_hidden()
                .child(
                    div()
                        .flex_none()
                        .px(px(8.0))
                        .py(px(4.0))
                        .border_b_1()
                        .border_color(theme.border)
                        .text_color(theme.text_ghost)
                        .child(SharedString::from(tr!("files.symbol_count", count = total))),
                )
                .child(
                    div()
                        .id("file-symbols-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&scroll)
                        .children(rows),
                )
                .child(scrollbar::vertical(&scroll, &scrollbar))
                .into_any_element(),
        )
    }
}

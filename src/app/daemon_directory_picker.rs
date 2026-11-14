//! A modal folder browser over the daemon's own filesystem.
//!
//! The native path prompt reads *this* machine's disks, so it cannot answer for
//! a remote daemon: the folder the user picks has to exist where the daemon
//! runs, not where the window is. This dialog asks the daemon instead, through
//! the same `BrowseDirectory` RPC Waku Web uses, and returns a path the daemon
//! host can resolve.
//!
//! Listing a directory is filesystem I/O, so it never runs in a frame. Reads go
//! through [`QueryCache`], which hands the first renderer a token rather than a
//! result; the fetch lands on the background executor and the next frame draws
//! what arrived. A miss therefore means "not known yet", never "block".

use std::path::PathBuf;

use gpui::{KeyBinding, actions};

use super::composer::next_picker_highlight;
use super::*;

actions!(
    waku_directory_picker,
    [
        ChooseDirectory,
        DismissDirectory,
        SelectNextFolder,
        SelectPreviousFolder
    ]
);

const DIALOG_CONTEXT: &str = "DaemonDirectoryPicker";
/// The filter field holds focus while the list moves under it, so the list's
/// keys have to be claimed from its scope — the same arrangement the project
/// picker and the menus use.
const DIALOG_INPUT_CONTEXT: &str = "DaemonDirectoryPicker > TextInput";

/// Matches the project picker's row pitch, so every list in the app reads alike.
const ROW_HEIGHT: f32 = 26.0;
/// Rows drawn before the list scrolls. Whole rows only; the edge fade is the
/// cue that more follow.
const VISIBLE_ROWS: f32 = 11.0;
const DIALOG_WIDTH: f32 = 460.0;

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", DismissDirectory, Some(DIALOG_CONTEXT)),
        KeyBinding::new("escape", DismissDirectory, Some(DIALOG_INPUT_CONTEXT)),
        KeyBinding::new("enter", ChooseDirectory, Some(DIALOG_INPUT_CONTEXT)),
        KeyBinding::new("down", SelectNextFolder, Some(DIALOG_INPUT_CONTEXT)),
        KeyBinding::new("up", SelectPreviousFolder, Some(DIALOG_INPUT_CONTEXT)),
    ]);
}

/// One directory the daemon answered for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DirectoryListing {
    /// Canonical path the daemon resolved the request to.
    path: PathBuf,
    parent: Option<PathBuf>,
    home: PathBuf,
    filesystem_root: PathBuf,
    folders: Vec<DirectoryFolder>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DirectoryFolder {
    name: String,
    path: PathBuf,
}

impl DirectoryListing {
    fn from_daemon(
        path: PathBuf,
        parent: Option<PathBuf>,
        home: PathBuf,
        filesystem_root: PathBuf,
        entries: Vec<waku_client::workspace::WorkingTreeEntry>,
    ) -> Self {
        let folders = entries
            .into_iter()
            .filter(|entry| entry.is_dir)
            .map(|entry| DirectoryFolder {
                name: entry.name,
                path: entry.absolute_path,
            })
            .collect();
        Self {
            path,
            parent,
            home,
            filesystem_root,
            folders,
        }
    }

    /// The folders this listing draws, narrowed by `query`. Empty query keeps
    /// the daemon's order: directories only, already sorted case-insensitively.
    fn visible_folders(&self, query: &str) -> Vec<DirectoryFolder> {
        if query.is_empty() {
            return self.folders.clone();
        }
        self.folders
            .iter()
            .filter(|folder| folder.name.to_lowercase().contains(query))
            .cloned()
            .collect()
    }

    /// Whether the daemon's home is worth offering as a jump target: not when
    /// the listing already is the home.
    fn home_shortcut(&self) -> Option<(&'static str, PathBuf)> {
        (self.path != self.home).then(|| ("icons/folder.svg", self.home.clone()))
    }

    fn root_shortcut(&self) -> Option<(&'static str, PathBuf)> {
        (self.filesystem_root != self.home && self.path != self.filesystem_root)
            .then(|| ("icons/chevron-up.svg", self.filesystem_root.clone()))
    }
}

/// One drawn row: a pinned location, or a folder from the listing.
enum DirectoryRow {
    Location {
        label: SharedString,
        path: PathBuf,
        icon_path: &'static str,
    },
    Folder(DirectoryFolder),
}

impl DirectoryRow {
    fn path(&self) -> &PathBuf {
        match self {
            Self::Location { path, .. } => path,
            Self::Folder(folder) => &folder.path,
        }
    }
}

pub(super) struct DaemonDirectoryPicker {
    /// What to ask the daemon for. `None` starts at the daemon's home.
    current: Option<PathBuf>,
    /// Every folder visited, with `history_index` marking the current one, so
    /// Back and Forward cost no RPC of their own.
    history: Vec<Option<PathBuf>>,
    history_index: usize,
    filter: Entity<TextInput>,
    highlight: Option<usize>,
    accept_focus: FocusHandle,
    cancel_focus: FocusHandle,
    up_focus: FocusHandle,
    list_state: ListState,
    /// Item count last pushed into `list_state`, so a redraw with the same rows
    /// does not reset the scroll position.
    row_count: RefCell<usize>,
    scrollbar: Rc<ScrollbarState>,
}

impl Waku {
    /// Stage the dialog; the next frame builds it. Opening needs a `Window` for
    /// the filter field, which the callers of `add_project` do not carry.
    pub(super) fn request_daemon_directory_picker(&mut self, cx: &mut Context<Self>) {
        self.daemon_directory_picker_request = Some(());
        cx.notify();
    }

    fn materialize_daemon_directory_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let filter =
            cx.new(|cx| TextInput::new(window, cx).placeholder(tr!("file_picker.filter_folder")));
        let filter_focus = filter.read(cx).focus();
        cx.subscribe(&filter, |_: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Edited) {
                cx.notify();
            }
        })
        .detach();
        self.daemon_directory_picker = Some(DaemonDirectoryPicker {
            current: None,
            history: vec![None],
            history_index: 0,
            filter,
            highlight: None,
            accept_focus: cx.focus_handle(),
            cancel_focus: cx.focus_handle(),
            up_focus: cx.focus_handle(),
            list_state: ListState::new(0, ListAlignment::Top, px(152.0)),
            row_count: RefCell::new(0),
            scrollbar: ScrollbarState::new(),
        });
        // Like the app's other deferred modals, this one joins the dispatch
        // tree only after it has drawn. Focus it two frames later so typing
        // cannot fall through to the composer beneath it.
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| window.focus(&filter_focus, cx));
        });
        cx.notify();
    }

    pub(super) fn close_daemon_directory_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.daemon_directory_picker_request = None;
        if self.daemon_directory_picker.take().is_none() {
            return;
        }
        let focus = self.composer_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Ask for `key`, or read the listing the daemon already delivered.
    fn directory_listing(
        &mut self,
        key: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> (Option<DirectoryListing>, Option<String>) {
        match self.daemon_directories.read(&key) {
            Query::Ready(result) => match result.as_ref() {
                Ok(listing) => (Some(listing.clone()), None),
                Err(error) => (None, Some(error.clone())),
            },
            Query::Pending => (None, None),
            Query::Missing(token) => {
                let workspace = waku_client::WorkspaceClient::new(self.daemon.client());
                cx.spawn(async move |waku, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            match workspace.request(
                                waku_client::WorkspaceOperation::BrowseDirectory { path: key },
                            ) {
                                Ok(waku_client::WorkspaceResult::Directory {
                                    path,
                                    parent,
                                    home,
                                    filesystem_root,
                                    entries,
                                }) => Ok(DirectoryListing::from_daemon(
                                    path,
                                    parent,
                                    home,
                                    filesystem_root,
                                    entries,
                                )),
                                Ok(_) => Err(String::from(
                                    "The daemon returned an unexpected directory response",
                                )),
                                Err(error) => Err(error.to_string()),
                            }
                        })
                        .await;
                    waku.update(cx, |waku, cx| {
                        if waku.daemon_directories.fulfill(token, result) {
                            cx.notify();
                        }
                    })
                })
                .detach();
                (None, None)
            }
        }
    }

    /// Navigate to `path`, recording it so Back can return.
    fn visit_daemon_directory(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        let Some(picker) = self.daemon_directory_picker.as_mut() else {
            return;
        };
        if picker.history.get(picker.history_index) == Some(&path) {
            return;
        }
        picker.history.truncate(picker.history_index + 1);
        picker.history.push(path.clone());
        picker.history_index = picker.history.len() - 1;
        picker.current = path;
        picker.highlight = None;
        cx.notify();
    }

    fn step_daemon_directory_history(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(picker) = self.daemon_directory_picker.as_mut() else {
            return;
        };
        let next = if forward {
            (picker.history_index + 1 < picker.history.len()).then_some(picker.history_index + 1)
        } else {
            picker.history_index.checked_sub(1)
        };
        let Some(next) = next else {
            return;
        };
        picker.history_index = next;
        picker.current = picker.history[next].clone();
        picker.highlight = None;
        cx.notify();
    }

    /// Take the folder currently showing as the new project.
    fn confirm_daemon_directory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self
            .daemon_directory_picker
            .as_ref()
            .map(|picker| picker.current.clone())
        else {
            return;
        };
        let path = match self.daemon_directories.read(&key) {
            Query::Ready(result) => result
                .as_ref()
                .as_ref()
                .ok()
                .map(|listing| listing.path.clone()),
            _ => None,
        };
        let Some(path) = path else {
            return;
        };
        self.close_daemon_directory_picker(window, cx);
        self.begin_project_at(path, cx);
    }

    /// Move the drawn selection over the rows the filter currently leaves.
    fn move_daemon_directory_highlight(&mut self, key: &str, rows: usize, cx: &mut Context<Self>) {
        let Some(picker) = self.daemon_directory_picker.as_mut() else {
            return;
        };
        if rows == 0 {
            return;
        }
        let current = picker.highlight.filter(|index| *index < rows);
        let Some(next) = next_picker_highlight(current, rows, key) else {
            return;
        };
        picker.highlight = Some(next);
        picker.list_state.scroll_to_reveal_item(next);
        cx.notify();
    }

    pub(super) fn render_daemon_directory_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.daemon_directory_picker_request.take().is_some()
            && self.daemon_directory_picker.is_none()
        {
            self.materialize_daemon_directory_picker(window, cx);
        }
        let key = self
            .daemon_directory_picker
            .as_ref()
            .map(|picker| picker.current.clone())?;
        let (listing, failure) = self.directory_listing(key, cx);
        let theme = Theme::current(cx);

        let picker = self.daemon_directory_picker.as_ref()?;
        let query = picker.filter.read(cx).content().trim().to_lowercase();
        let mut rows = Vec::new();
        if let Some(listing) = &listing {
            if query.is_empty() {
                if let Some((icon_path, path)) = listing.home_shortcut() {
                    rows.push(DirectoryRow::Location {
                        label: SharedString::from(tr!("file_picker.home")),
                        path,
                        icon_path,
                    });
                }
                if let Some((icon_path, path)) = listing.root_shortcut() {
                    rows.push(DirectoryRow::Location {
                        label: SharedString::from(tr!("file_picker.file_system")),
                        path,
                        icon_path,
                    });
                }
            }
            rows.extend(
                listing
                    .visible_folders(&query)
                    .into_iter()
                    .map(DirectoryRow::Folder),
            );
        }
        let rows = Rc::new(rows);
        {
            let mut cached = picker.row_count.borrow_mut();
            if *cached != rows.len() {
                *cached = rows.len();
                picker
                    .list_state
                    .reset_with_uniform_height(rows.len(), px(ROW_HEIGHT));
            }
        }
        let highlight = picker.highlight.filter(|index| *index < rows.len());
        let up_target = listing.as_ref().and_then(|listing| listing.parent.clone());
        let path_label = listing
            .as_ref()
            .map(|listing| compact_path(&listing.path))
            .unwrap_or_default();

        let weak = cx.entity().downgrade();
        let list_state = picker.list_state.clone();
        let scrollbar = picker.scrollbar.clone();
        let filter = picker.filter.clone();
        let accept_focus = picker.accept_focus.clone();
        let cancel_focus = picker.cancel_focus.clone();
        let up_focus = picker.up_focus.clone();
        let can_accept = listing.is_some();

        let list_row = {
            let rows = rows.clone();
            let weak = weak.clone();
            move |index: usize, _window: &mut Window, _cx: &mut App| -> AnyElement {
                let Some(row) = rows.get(index) else {
                    return div().into_any_element();
                };
                let path = row.path().clone();
                let (name, icon_path) = match row {
                    DirectoryRow::Location {
                        label, icon_path, ..
                    } => (label.clone(), *icon_path),
                    DirectoryRow::Folder(folder) => {
                        (SharedString::from(folder.name.clone()), "icons/folder.svg")
                    }
                };
                let select_weak = weak.clone();
                div()
                    .id(("daemon-directory-row", index))
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .px(px(6.0))
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_default()
                    .text_size(sp(12.5))
                    .line_height(sp(15.0))
                    .when(highlight == Some(index), |element| {
                        element.bg(theme.overlay_strong)
                    })
                    .hover(|element| element.bg(theme.overlay))
                    .active(|element| element.opacity(0.85))
                    .child(icon(icon_path, 13.0, theme.text_secondary))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_color(theme.text)
                            .child(name),
                    )
                    .on_click(move |_, _, cx| {
                        let _ = select_weak.update(cx, |this, cx| {
                            this.visit_daemon_directory(Some(path.clone()), cx);
                        });
                    })
                    .into_any_element()
            }
        };

        let empty_state = |label: String, detail: String| {
            div()
                .h(px(96.0))
                .w_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(4.0))
                .text_size(sp(12.5))
                .text_color(theme.text_ghost)
                .child(label)
                .child(
                    div()
                        .text_size(sp(11.5))
                        .text_color(theme.text_ghost)
                        .child(detail),
                )
        };

        let list_area = if let Some(error) = &failure {
            empty_state(error.clone(), tr!("file_picker.daemon_folders")).into_any_element()
        } else if listing.is_none() {
            empty_state(
                tr!("file_picker.loading_folder"),
                tr!("file_picker.daemon_folders"),
            )
            .into_any_element()
        } else if rows.is_empty() {
            let label = if query.is_empty() {
                tr!("file_picker.empty_folder")
            } else {
                tr!("file_picker.no_matching_items")
            };
            empty_state(label, tr!("file_picker.folders")).into_any_element()
        } else {
            let height = (rows.len() as f32).min(VISIBLE_ROWS) * ROW_HEIGHT;
            div()
                .id("daemon-directory-list")
                .relative()
                .w_full()
                .h(px(height))
                .flex_none()
                .px(px(4.0))
                .child(list(list_state.clone(), list_row).size_full())
                .child(scrollbar::edge_fade(
                    list_state.clone(),
                    scrollbar::FadeEdge::Top,
                    theme.composer,
                ))
                .child(scrollbar::edge_fade(
                    list_state.clone(),
                    scrollbar::FadeEdge::Bottom,
                    theme.composer,
                ))
                .child(scrollbar::vertical(&list_state, &scrollbar))
                .into_any_element()
        };

        let back_enabled = self
            .daemon_directory_picker
            .as_ref()
            .is_some_and(|picker| picker.history_index > 0);
        let forward_enabled = self
            .daemon_directory_picker
            .as_ref()
            .is_some_and(|picker| picker.history_index + 1 < picker.history.len());

        let nav_button = |id: &'static str,
                          icon_path: &'static str,
                          enabled: bool,
                          focus: &FocusHandle,
                          theme: &Theme| {
            div()
                .id(id)
                .track_focus(focus)
                .when(enabled, |element| element.tab_index(0))
                .size(px(24.0))
                .rounded(px(6.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .cursor_default()
                .when(!enabled, |element| element.opacity(0.4))
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .hover(|style| style.bg(theme.overlay))
                .active(|style| style.bg(theme.overlay_strong))
                .child(icon(icon_path, 13.0, theme.text_secondary))
        };

        let back_button = {
            let weak = weak.clone();
            nav_button(
                "daemon-directory-back",
                "icons/arrow-left.svg",
                back_enabled,
                &cancel_focus,
                &theme,
            )
            .on_click(move |_, _, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.step_daemon_directory_history(false, cx);
                });
            })
        };
        let forward_button = {
            let weak = weak.clone();
            nav_button(
                "daemon-directory-forward",
                "icons/chevron-right.svg",
                forward_enabled,
                &up_focus,
                &theme,
            )
            .on_click(move |_, _, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.step_daemon_directory_history(true, cx);
                });
            })
        };
        let up_button = {
            let weak = weak.clone();
            nav_button(
                "daemon-directory-up",
                "icons/folder-open.svg",
                up_target.is_some(),
                &up_focus,
                &theme,
            )
            .when_some(up_target, |element, parent| {
                element.on_click(move |_, _, cx| {
                    let parent = parent.clone();
                    let _ = weak.update(cx, |this, cx| {
                        this.visit_daemon_directory(Some(parent.clone()), cx);
                    });
                })
            })
        };

        let accept_button = {
            let weak = weak.clone();
            div()
                .id("daemon-directory-accept")
                .track_focus(&accept_focus)
                .when(can_accept, |element| element.tab_index(0))
                .h(px(28.0))
                .px(px(12.0))
                .rounded(px(7.0))
                .flex()
                .items_center()
                .justify_center()
                .cursor_default()
                .text_size(sp(12.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(if can_accept {
                    gpui::white()
                } else {
                    theme.text_ghost
                })
                .bg(if can_accept {
                    theme.accent
                } else {
                    theme.overlay
                })
                .when(can_accept, |element| {
                    element.hover(|style| style.opacity(0.9))
                })
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .child(tr!("file_picker.open"))
                .on_click(move |_, window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.confirm_daemon_directory(window, cx);
                    });
                })
        };

        let cancel_button = {
            let weak = weak.clone();
            div()
                .id("daemon-directory-cancel")
                .track_focus(&cancel_focus)
                .tab_index(0)
                .h(px(28.0))
                .px(px(12.0))
                .rounded(px(7.0))
                .border_1()
                .border_color(theme.border_strong)
                .flex()
                .items_center()
                .justify_center()
                .cursor_default()
                .text_size(sp(12.5))
                .text_color(theme.text_secondary)
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .hover(|style| style.bg(theme.overlay))
                .child(tr!("common.cancel"))
                .on_click(move |_, window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.close_daemon_directory_picker(window, cx);
                    });
                })
        };

        let card = div()
            .id("daemon-directory-picker-card")
            .key_context(DIALOG_CONTEXT)
            .on_action(cx.listener(|this, _: &ChooseDirectory, window, cx| {
                this.confirm_daemon_directory(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DismissDirectory, window, cx| {
                this.close_daemon_directory_picker(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectNextFolder, _, cx| {
                let count = this
                    .daemon_directory_picker
                    .as_ref()
                    .map(|picker| *picker.row_count.borrow())
                    .unwrap_or(0);
                this.move_daemon_directory_highlight("down", count, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPreviousFolder, _, cx| {
                let count = this
                    .daemon_directory_picker
                    .as_ref()
                    .map(|picker| *picker.row_count.borrow())
                    .unwrap_or(0);
                this.move_daemon_directory_highlight("up", count, cx);
            }))
            .tab_group()
            .tab_stop(false)
            .w_full()
            .max_w(px(DIALOG_WIDTH))
            .overflow_hidden()
            .rounded(px(18.0))
            .bg(theme.composer)
            .shadow_xl()
            .flex()
            .flex_col()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            // Header
            .child(
                div()
                    .h(px(48.0))
                    .px(px(16.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .text_size(sp(14.0))
                    .text_color(theme.text)
                    .child(icon("icons/folder.svg", 15.0, theme.text))
                    .child(div().child(tr!("file_picker.choose_project_folder"))),
            )
            // Path bar and filter field
            .child(
                div()
                    .px(px(16.0))
                    .pb(px(10.0))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(back_button)
                            .child(forward_button)
                            .child(up_button)
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .truncate()
                                    .text_size(sp(12.5))
                                    .text_color(theme.text_secondary)
                                    .child(path_label),
                            ),
                    )
                    .child(
                        TextField::new("daemon-directory-filter", filter)
                            .icon("icons/search.svg", 13.0),
                    ),
            )
            // Folders
            .child(div().px(px(8.0)).child(list_area))
            // Actions
            .child(div().mx(px(8.0)).my(px(4.0)).h(px(1.0)).bg(theme.border))
            .child(
                div()
                    .p(px(8.0))
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(8.0))
                    .child(cancel_button)
                    .child(accept_button),
            );

        let scrim = if theme.is_dark {
            gpui::hsla(0.0, 0.0, 0.0, 0.34)
        } else {
            gpui::hsla(0.0, 0.0, 0.0, 0.16)
        };
        let layer = div()
            .id("daemon-directory-picker-layer")
            .absolute()
            .inset_0()
            .occlude()
            .bg(scrim)
            .p(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_daemon_directory_picker(window, cx)),
            )
            .child(card);
        Some(gpui::deferred(layer).with_priority(4).into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> DirectoryFolder {
        DirectoryFolder {
            name: name.to_owned(),
            path: PathBuf::from(format!("/w/{name}")),
        }
    }

    fn listing(path: &str) -> DirectoryListing {
        DirectoryListing {
            path: PathBuf::from(path),
            parent: Some(PathBuf::from("/w")),
            home: PathBuf::from("/home/me"),
            filesystem_root: PathBuf::from("/"),
            folders: vec![folder("src"), folder("Src"), folder("docs")],
        }
    }

    #[test]
    fn an_empty_query_keeps_every_folder_in_daemon_order() {
        let listing = listing("/w");

        assert_eq!(
            listing.visible_folders(""),
            vec![folder("src"), folder("Src"), folder("docs")],
            "an unfiltered listing must not be re-sorted or dropped"
        );
    }

    #[test]
    fn the_filter_ignores_case_and_narrows_to_matches() {
        let listing = listing("/w");

        assert_eq!(
            listing.visible_folders("src"),
            vec![folder("src"), folder("Src")],
            "both cases match, in the daemon's own order"
        );
        assert!(listing.visible_folders("nope").is_empty());
    }

    #[test]
    fn shortcuts_skip_the_location_the_listing_already_is() {
        let home = listing("/home/me");
        assert!(
            home.home_shortcut().is_none(),
            "offering home while sitting in it is dead weight"
        );
        assert!(home.root_shortcut().is_some());

        let root = listing("/");
        assert!(root.home_shortcut().is_some());
        assert!(
            root.root_shortcut().is_none(),
            "the filesystem root is not a target from itself"
        );
    }

    #[test]
    fn the_root_shortcut_is_dropped_when_the_root_is_the_home() {
        let mut listing = listing("/w");
        listing.home = PathBuf::from("/");

        assert!(listing.root_shortcut().is_none());
    }
}

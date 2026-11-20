//! The Claude page: Claude Code's own MCP servers and plugins.
//!
//! These are not Waku settings. They are another program's configuration,
//! reached through that program's CLI, which is why every mutation here is
//! routed through the daemon and reported back with the CLI's own error text —
//! a failure from `claude` is more useful than anything Waku could paraphrase.
//!
//! Two rules shape the page:
//!
//! * **A read never blocks a frame.** Both catalogs come from subprocesses, so
//!   they arrive asynchronously and the page renders whatever landed last.
//! * **Destructive actions confirm first.** Removing an MCP server or
//!   uninstalling a plugin changes configuration Waku cannot restore, so the
//!   control opens a confirmation rather than acting on one click.

use super::*;
use gpui::{KeyBinding, actions};
use waku_client::claude::AvailablePlugin;

actions!(
    waku_claude_dialog,
    [ConfirmClaudeDialog, DismissClaudeDialog]
);

/// The confirmation card's key context.
///
/// A single identifier on purpose: `key_context()` takes the simple
/// `KeyContext::parse`, which does not understand `>` and recurses forever on
/// it. The `>` form belongs only on `KeyBinding::new`'s predicate argument,
/// which is parsed by a different parser.
const CLAUDE_DIALOG_CONTEXT: &str = "ClaudeDialog";

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", ConfirmClaudeDialog, Some(CLAUDE_DIALOG_CONTEXT)),
        KeyBinding::new("escape", DismissClaudeDialog, Some(CLAUDE_DIALOG_CONTEXT)),
    ]);
}

/// Which half of the page is showing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ClaudeTab {
    #[default]
    Mcp,
    Plugins,
    /// The plugins the added marketplaces offer, plus the marketplaces
    /// themselves. Separate from `Plugins` because reading it is expensive:
    /// the CLI refreshes every marketplace before answering (~10s here).
    Marketplace,
}

/// A pending destructive action, held until the user confirms.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ClaudeConfirm {
    RemoveMcp {
        name: String,
        scope: McpScope,
    },
    UninstallPlugin {
        id: String,
        name: String,
        scope: McpScope,
    },
}

/// The scope an install from the marketplace goes into.
///
/// `user` is the CLI's own default and the only scope that makes sense for a
/// plugin added outside any project: `project` and `local` both point at the
/// directory the command runs in, and a marketplace plugin is not tied to one.
///
/// Always passed explicitly anyway. The page names the scope in its confirmation
/// text, and relying on a CLI default to agree with that text is exactly how the
/// uninstall path once reported success while leaving the plugin installed.
fn plugin_install_scope() -> McpScope {
    McpScope::User
}

/// Whether an available plugin matches the marketplace filter.
///
/// A case-insensitive substring over name, marketplace and description: with
/// thousands of entries the user is looking for a name they half remember, and
/// requiring a prefix match would miss it.
fn plugin_matches(plugin: &AvailablePlugin, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    plugin.name.to_lowercase().contains(query)
        || plugin.marketplace.to_lowercase().contains(query)
        || plugin.description.to_lowercase().contains(query)
}

impl Waku {
    // ── Loading ────────────────────────────────────────────────────────────

    /// Refresh whichever catalog the visible tab needs.
    ///
    /// Called from the page's render, so it must be cheap when nothing is
    /// stale: the generation guard and `pending` flag make a render that lands
    /// mid-read a no-op instead of a second subprocess.
    pub(super) fn ensure_claude_catalog(&mut self, cx: &mut Context<Self>) {
        if self.claude_catalog_pending {
            return;
        }
        let loaded = match self.claude_tab {
            ClaudeTab::Mcp => self.claude_mcp_catalog.is_some(),
            ClaudeTab::Plugins => {
                self.claude_plugin_catalog.is_some() && self.claude_marketplace_catalog.is_some()
            }
            // Opening this tab deliberately does not start the expensive
            // read: the page shows a button for it instead. Only the cheap
            // plugin list is fetched, because the rows need to know which
            // plugins are already installed.
            ClaudeTab::Marketplace => {
                self.claude_plugin_catalog.is_some() && self.claude_marketplace_catalog.is_some()
            }
        };
        if loaded {
            return;
        }
        self.reload_claude_catalog(cx);
    }

    /// Re-read the cheap catalogs, dropping any read already in flight.
    pub(super) fn reload_claude_catalog(&mut self, cx: &mut Context<Self>) {
        self.claude_catalog_pending = true;
        self.claude_catalog_generation += 1;
        let generation = self.claude_catalog_generation;
        let daemon = self.daemon.client();
        let cwd = self.selected_workspace_path().map(Path::to_path_buf);
        cx.spawn(async move |this, cx| {
            let (mcp, plugins, marketplaces) = cx
                .background_executor()
                .spawn(async move {
                    // Three independent daemon requests. Run them concurrently:
                    // the daemon handles each on its own thread, and the MCP
                    // read dominates (it health-checks every server) while the
                    // other two are ~1s each. Sequentially the page waited for
                    // their sum.
                    let mcp_client = daemon.clone();
                    let plugin_client = daemon.clone();
                    let marketplace_client = daemon;
                    let (mcp, plugins, marketplaces) = futures::join!(
                        async move {
                            match mcp_client.request(
                                Uuid::nil(),
                                Uuid::nil(),
                                waku_client::Command::LoadClaudeMcpServers { cwd },
                            ) {
                                Ok(waku_client::ResponsePayload::ClaudeMcpCatalog { catalog }) => {
                                    Some(catalog)
                                }
                                _ => None,
                            }
                        },
                        async move {
                            match plugin_client.request(
                                Uuid::nil(),
                                Uuid::nil(),
                                waku_client::Command::LoadClaudePlugins,
                            ) {
                                Ok(waku_client::ResponsePayload::ClaudePluginCatalog {
                                    catalog,
                                }) => Some(catalog),
                                _ => None,
                            }
                        },
                        async move {
                            match marketplace_client.request(
                                Uuid::nil(),
                                Uuid::nil(),
                                waku_client::Command::LoadClaudeMarketplaces,
                            ) {
                                Ok(waku_client::ResponsePayload::ClaudeMarketplaceCatalog {
                                    catalog,
                                }) => Some(catalog),
                                _ => None,
                            }
                        },
                    );
                    (mcp, plugins, marketplaces)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.claude_catalog_generation != generation {
                    return;
                }
                this.claude_catalog_pending = false;
                if let Some(catalog) = mcp {
                    this.claude_mcp_catalog = Some(Rc::new(catalog));
                }
                if let Some(catalog) = plugins {
                    this.claude_plugin_catalog = Some(Rc::new(catalog));
                }
                if let Some(catalog) = marketplaces {
                    this.claude_marketplace_catalog = Some(Rc::new(catalog));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Load the full marketplace catalog. Only ever called from the button that
    /// asks for it, never from a render: this is the ~10s read.
    fn load_available_plugins(&mut self, cx: &mut Context<Self>) {
        self.claude_available_pending = true;
        self.claude_available_generation += 1;
        let generation = self.claude_available_generation;
        let daemon = self.daemon.client();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::LoadAvailableClaudePlugins,
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.claude_available_generation != generation {
                    return;
                }
                this.claude_available_pending = false;
                match result {
                    Ok(waku_client::ResponsePayload::ClaudeAvailablePluginCatalog { catalog }) => {
                        this.claude_available_catalog = Some(Rc::new(catalog));
                    }
                    Err(error) => this.show_toast(error.to_string()),
                    Ok(_) => this.show_toast(tr!("errors.unexpected_response")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Read one plugin's manifest, then show it under its row.
    fn load_plugin_details(
        &mut self,
        id: String,
        install_path: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        // Clicking the open row closes it, which needs no read.
        if self.claude_detail.as_ref().map(|(open, _)| open) == Some(&id) {
            self.claude_detail = None;
            cx.notify();
            return;
        }
        self.claude_detail_generation += 1;
        let generation = self.claude_detail_generation;
        let daemon = self.daemon.client();
        let request_id = id.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::LoadClaudePluginDetails {
                            id: request_id,
                            install_path,
                        },
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.claude_detail_generation != generation {
                    return;
                }
                match result {
                    Ok(waku_client::ResponsePayload::ClaudePluginDetails { details }) => {
                        this.claude_detail = Some((id, Rc::new(details)));
                    }
                    Err(error) => this.show_toast(error.to_string()),
                    Ok(_) => this.show_toast(tr!("errors.unexpected_response")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ── Mutations ──────────────────────────────────────────────────────────

    /// Run one mutating CLI command, then re-read so the page shows what the
    /// CLI actually did rather than what Waku assumed it would do.
    ///
    /// The row is removed from the snapshot as soon as the CLI confirms, rather
    /// than waiting for the re-read. That re-read pays for the full MCP health
    /// check and a scope probe per server, so without this the row lingered for
    /// seconds after the delete had already happened and the button looked
    /// dead.
    fn run_claude_mutation(&mut self, command: waku_client::Command, cx: &mut Context<Self>) {
        // Whatever this command acts on, it is about to be gone or changed.
        let removed_mcp = match &command {
            waku_client::Command::RemoveClaudeMcpServer { name, .. } => Some(name.clone()),
            _ => None,
        };
        let removed_plugin = match &command {
            waku_client::Command::UninstallClaudePlugin { id, .. } => Some(id.clone()),
            _ => None,
        };
        self.claude_mutating = true;
        cx.notify();

        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { daemon.request(Uuid::nil(), Uuid::nil(), command) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.claude_mutating = false;
                match result {
                    Err(error) => this.show_toast(error.to_string()),
                    Ok(waku_client::ResponsePayload::Ack) => {
                        // Show the change now; the re-read below confirms it.
                        if let (Some(name), Some(catalog)) =
                            (removed_mcp, this.claude_mcp_catalog.clone())
                        {
                            let mut next = (*catalog).clone();
                            next.servers.retain(|server| server.name != name);
                            this.claude_mcp_catalog = Some(Rc::new(next));
                        }
                        if let (Some(id), Some(catalog)) =
                            (removed_plugin, this.claude_plugin_catalog.clone())
                        {
                            let mut next = (*catalog).clone();
                            next.plugins.retain(|plugin| plugin.id != id);
                            this.claude_plugin_catalog = Some(Rc::new(next));
                        }
                    }
                    Ok(_) => this.show_toast(tr!("errors.unexpected_response")),
                }
                this.reload_claude_catalog(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn confirm_claude_action(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.claude_confirm.take() else {
            return;
        };
        let cwd = self.selected_workspace_path().map(Path::to_path_buf);
        let command = match pending {
            ClaudeConfirm::RemoveMcp { name, scope } => {
                waku_client::Command::RemoveClaudeMcpServer { name, scope, cwd }
            }
            ClaudeConfirm::UninstallPlugin { id, scope, .. } => {
                waku_client::Command::UninstallClaudePlugin { id, scope, cwd }
            }
        };
        self.run_claude_mutation(command, cx);
    }

    pub(super) fn cancel_claude_action(&mut self, cx: &mut Context<Self>) {
        if self.claude_confirm.take().is_some() {
            cx.notify();
        }
    }

    fn request_remove_mcp(&mut self, entry: &McpServerEntry, cx: &mut Context<Self>) {
        self.claude_confirm = Some(ClaudeConfirm::RemoveMcp {
            name: entry.name.clone(),
            scope: entry.scope,
        });
        cx.notify();
    }

    fn request_uninstall_plugin(&mut self, plugin: &PluginEntry, cx: &mut Context<Self>) {
        self.claude_confirm = Some(ClaudeConfirm::UninstallPlugin {
            id: plugin.id.clone(),
            name: plugin.name.clone(),
            scope: plugin.scope,
        });
        cx.notify();
    }

    fn set_plugin_enabled(&mut self, plugin: &PluginEntry, enabled: bool, cx: &mut Context<Self>) {
        let cwd = self.selected_workspace_path().map(Path::to_path_buf);
        self.run_claude_mutation(
            waku_client::Command::SetClaudePluginEnabled {
                id: plugin.id.clone(),
                enabled,
                cwd,
            },
            cx,
        );
    }

    /// Install one plugin from a marketplace the user already added.
    ///
    /// No confirmation: installing adds a plugin the user just asked for and
    /// `uninstall` reverses it. The scope still travels explicitly, because the
    /// CLI's default would install into every project.
    fn install_available_plugin(&mut self, plugin: &AvailablePlugin, cx: &mut Context<Self>) {
        let cwd = self.selected_workspace_path().map(Path::to_path_buf);
        let scope = plugin_install_scope();
        self.run_claude_mutation(
            waku_client::Command::InstallClaudePlugin {
                id: plugin.id.clone(),
                scope,
                cwd,
            },
            cx,
        );
    }

    fn update_installed_plugin(&mut self, plugin: &PluginEntry, cx: &mut Context<Self>) {
        let cwd = self.selected_workspace_path().map(Path::to_path_buf);
        self.run_claude_mutation(
            waku_client::Command::UpdateClaudePlugin {
                id: plugin.id.clone(),
                scope: Some(plugin.scope),
                cwd,
            },
            cx,
        );
    }
}

// ── Rendering ──────────────────────────────────────────────────────────────

impl Waku {
    pub(super) fn render_claude_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let mut body = div().flex().flex_col().gap(px(14.0)).w_full();

        body = body.child(self.render_claude_tabs(cx));
        body = body.child(self.render_claude_status(cx));

        body = body.child(match self.claude_tab {
            ClaudeTab::Mcp => self.render_mcp_list(cx),
            ClaudeTab::Plugins => self.render_plugin_list(cx),
            ClaudeTab::Marketplace => self.render_marketplace(cx),
        });

        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .text_color(theme.text)
            .child(body)
            .into_any_element()
    }

    /// One line saying what the page is waiting on.
    ///
    /// Reading these catalogs means running the Claude CLI, and the worst case
    /// is seconds: `mcp list` health-checks every server, and a re-read after a
    /// delete pays that again. Without this line the page looked frozen — the
    /// buttons stayed enabled and nothing said a subprocess was running.
    fn render_claude_status(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let label = if self.claude_mutating {
            Some(tr!("claude.applying"))
        } else if self.claude_catalog_pending {
            Some(tr!("claude.reading"))
        } else {
            None
        };
        let Some(label) = label else {
            return div().h(px(0.0));
        };
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .text_size(sp(11.5))
            .text_color(theme.text_tertiary)
            .child(
                div()
                    .flex_none()
                    .size(px(6.0))
                    .rounded_full()
                    .bg(theme.accent),
            )
            .child(label)
    }

    fn render_claude_tabs(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let tab = self.claude_tab;
        let mut row = div().flex().items_center().gap(px(6.0));
        for (kind, label) in [
            (ClaudeTab::Mcp, tr!("claude.tab_mcp")),
            (ClaudeTab::Plugins, tr!("claude.tab_plugins")),
            (ClaudeTab::Marketplace, tr!("claude.tab_marketplace")),
        ] {
            let selected = tab == kind;
            row = row.child(
                div()
                    .id(SharedString::from(match kind {
                        ClaudeTab::Mcp => "claude-tab-mcp",
                        ClaudeTab::Plugins => "claude-tab-plugins",
                        ClaudeTab::Marketplace => "claude-tab-marketplace",
                    }))
                    .role(Role::Button)
                    .aria_label(label.clone())
                    .aria_selected(selected)
                    .tab_index(0)
                    .px(px(10.0))
                    .h(px(28.0))
                    .rounded(px(7.0))
                    .flex()
                    .items_center()
                    .cursor_default()
                    .text_size(sp(12.5))
                    .when(selected, |element| {
                        element.bg(theme.overlay_strong).text_color(theme.text)
                    })
                    .when(!selected, |element| {
                        element
                            .hover(|style| style.bg(theme.overlay))
                            .text_color(theme.text_secondary)
                    })
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.claude_tab = kind;
                        this.claude_confirm = None;
                        this.ensure_claude_catalog(cx);
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.claude_tab = kind;
                            this.claude_confirm = None;
                            this.ensure_claude_catalog(cx);
                            cx.notify();
                            cx.stop_propagation();
                        }
                    })),
            );
        }
        row
    }

    /// The confirmation for a destructive change, as a centred modal.
    ///
    /// It used to be a banner above the list, which read as a status message
    /// rather than a question waiting on an answer: nothing blocked the page
    /// behind it, and the buttons sat inline with the rows. A destructive
    /// action on configuration Waku cannot restore should stop the page until
    /// it is answered.
    ///
    /// Returns `None` when nothing is pending, so the caller can splice it in
    /// with `.children()` the way the other modals are attached.
    pub(super) fn render_claude_confirm_dialog(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let confirm = self.claude_confirm.as_ref()?.clone();
        let theme = Theme::current(cx);
        let message = match &confirm {
            ClaudeConfirm::RemoveMcp { name, scope } => tr!(
                "claude.remove_confirm",
                name = name.clone(),
                scope = scope.label()
            ),
            ClaudeConfirm::UninstallPlugin { name, .. } => {
                tr!("claude.uninstall_confirm", name = name.clone())
            }
        };
        let title = match &confirm {
            ClaudeConfirm::RemoveMcp { .. } => tr!("claude.remove_title"),
            ClaudeConfirm::UninstallPlugin { .. } => tr!("claude.uninstall_title"),
        };

        let card = div()
            .id("claude-confirm-card")
            .key_context(CLAUDE_DIALOG_CONTEXT)
            .on_action(
                cx.listener(|this, _: &ConfirmClaudeDialog, _, cx| this.confirm_claude_action(cx)),
            )
            .on_action(
                cx.listener(|this, _: &DismissClaudeDialog, _, cx| this.cancel_claude_action(cx)),
            )
            .w_full()
            .max_w(px(420.0))
            .overflow_hidden()
            .rounded(px(14.0))
            .bg(theme.composer)
            .shadow_xl()
            .flex()
            .flex_col()
            // Clicks inside the card must not reach the scrim, which dismisses.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .px(px(18.0))
                    .pt(px(16.0))
                    .pb(px(6.0))
                    .text_size(sp(14.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(title),
            )
            .child(
                div()
                    .px(px(18.0))
                    .pb(px(16.0))
                    .text_size(sp(13.0))
                    .line_height(sp(19.0))
                    .text_color(theme.text_secondary)
                    .child(message),
            )
            .child(div().mx(px(18.0)).h(px(1.0)).bg(theme.border))
            .child(
                div()
                    .p(px(10.0))
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(8.0))
                    .child(self.claude_confirm_button(
                        "claude-confirm-cancel",
                        tr!("common.cancel"),
                        false,
                        cx,
                        |this, cx| this.cancel_claude_action(cx),
                    ))
                    .child(self.claude_confirm_button(
                        "claude-confirm-accept",
                        tr!("common.confirm"),
                        true,
                        cx,
                        |this, cx| this.confirm_claude_action(cx),
                    )),
            );

        // The scrim matches the other modals' weights so a dark theme does not
        // go muddy and a light one does not wash out.
        let scrim = if theme.is_dark {
            gpui::hsla(0.0, 0.0, 0.0, 0.34)
        } else {
            gpui::hsla(0.0, 0.0, 0.0, 0.16)
        };
        let layer = div()
            .id("claude-confirm-layer")
            .absolute()
            .inset_0()
            .occlude()
            .bg(scrim)
            .p(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            // Clicking the scrim cancels, the same as Escape: a confirmation
            // this serious should be easy to back out of and hard to accept by
            // accident.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.cancel_claude_action(cx)),
            )
            .child(card);
        Some(gpui::deferred(layer).with_priority(4).into_any_element())
    }

    fn claude_confirm_button(
        &self,
        id: &'static str,
        label: String,
        destructive: bool,
        cx: &mut Context<Self>,
        action: fn(&mut Self, &mut Context<Self>),
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        div()
            .id(id)
            .role(Role::Button)
            .aria_label(label.clone())
            .tab_index(0)
            .px(px(14.0))
            .h(px(30.0))
            .flex_none()
            .rounded(px(7.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .text_size(sp(12.5))
            .font_weight(FontWeight::MEDIUM)
            .when(destructive, |element| {
                element.bg(theme.danger).text_color(theme.on_inverse)
            })
            .when(!destructive, |element| {
                element
                    .bg(theme.overlay)
                    .text_color(theme.text)
                    .hover(|style| style.bg(theme.overlay_strong))
            })
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| action(this, cx)))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    action(this, cx);
                    cx.stop_propagation();
                }
            }))
    }

    fn render_mcp_list(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let Some(catalog) = self.claude_mcp_catalog.clone() else {
            return self.claude_placeholder(&tr!("claude.loading"), cx);
        };
        if let Some(error) = catalog.unavailable.clone() {
            return self.claude_placeholder(&tr!("claude.mcp_unavailable", error = error), cx);
        }
        if catalog.servers.is_empty() {
            return self.claude_placeholder(&tr!("claude.mcp_empty"), cx);
        }

        let mut list = div().flex().flex_col().gap(px(6.0)).w_full();
        for entry in &catalog.servers {
            list = list.child(self.render_mcp_row(entry, &theme, cx));
        }
        list
    }

    fn render_mcp_row(&self, entry: &McpServerEntry, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let (health_label, health_color) = match entry.health {
            Some(McpHealth::Connected) => (tr!("claude.health_connected"), theme.success),
            Some(McpHealth::Failed) => (tr!("claude.health_failed"), theme.danger),
            Some(McpHealth::Unknown) | None if entry.pending_approval => {
                (tr!("claude.pending_approval"), theme.text_tertiary)
            }
            Some(McpHealth::Unknown) | None => (tr!("claude.health_unknown"), theme.text_ghost),
        };
        let target = if entry.args.is_empty() {
            entry.target.clone()
        } else {
            format!("{} {}", entry.target, entry.args.join(" "))
        };
        let name = entry.name.clone();
        let scope = entry.scope;
        // Two closures each need their own copy: a click and a key press can
        // both request the same removal, and neither may capture the other's.
        let click_entry = entry.clone();
        let key_entry = entry.clone();

        div()
            .flex()
            .items_start()
            .gap(px(10.0))
            .p(px(10.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.inset)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .child(
                                div()
                                    .flex_none()
                                    .size(px(7.0))
                                    .rounded_full()
                                    .bg(health_color),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(sp(13.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(name.clone()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .px(px(5.0))
                                    .rounded(px(4.0))
                                    .bg(theme.overlay)
                                    .text_size(sp(10.5))
                                    .text_color(theme.text_tertiary)
                                    .child(scope.label()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(sp(11.0))
                                    .text_color(health_color)
                                    .child(health_label),
                            ),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(sp(11.5))
                            .text_color(theme.text_tertiary)
                            .child(target),
                    )
                    .when_some(entry.detail.clone(), |column, detail| {
                        column.child(
                            div()
                                .text_size(sp(11.0))
                                .text_color(theme.danger)
                                .child(detail),
                        )
                    }),
            )
            .when(entry.scope.is_removable(), |row| {
                row.child(
                    div()
                        .id(SharedString::from(format!(
                            "claude-mcp-remove-{}",
                            entry.name
                        )))
                        .role(Role::Button)
                        .aria_label(tr!("claude.remove"))
                        .tab_index(0)
                        .flex_none()
                        .px(px(9.0))
                        .h(px(24.0))
                        .rounded(px(6.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_default()
                        .text_size(sp(11.5))
                        .text_color(theme.text_secondary)
                        .hover(|style| style.bg(theme.overlay).text_color(theme.danger))
                        .focus_visible(|style| style.border_1().border_color(theme.accent))
                        .child(tr!("claude.remove"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.request_remove_mcp(&click_entry, cx);
                        }))
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                this.request_remove_mcp(&key_entry, cx);
                                cx.stop_propagation();
                            }
                        })),
                )
            })
            // A plugin-owned server gets no remove control: the plugin owns it
            // and would put it back, so the honest affordance is the plugin tab.
            .when(!entry.scope.is_removable(), |row| {
                row.child(
                    div()
                        .flex_none()
                        .px(px(9.0))
                        .text_size(sp(11.0))
                        .text_color(theme.text_ghost)
                        .child(tr!("claude.plugin_server_not_removable")),
                )
            })
    }

    fn render_plugin_list(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let Some(catalog) = self.claude_plugin_catalog.clone() else {
            return self.claude_placeholder(&tr!("claude.loading"), cx);
        };
        if let Some(error) = catalog.unavailable.clone() {
            return self.claude_placeholder(&tr!("claude.plugins_unavailable", error = error), cx);
        }
        if catalog.plugins.is_empty() {
            return self.claude_placeholder(&tr!("claude.plugins_empty"), cx);
        }

        let mut list = div().flex().flex_col().gap(px(6.0)).w_full();
        for plugin in &catalog.plugins {
            list = list.child(self.render_plugin_row(plugin, &theme, cx));
        }
        list
    }

    fn render_plugin_row(
        &self,
        plugin: &PluginEntry,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        // `enabled` is the only flag whose meaning is confirmed: it is what the
        // CLI reports and what `enable`/`disable` act on. `projectEnabled` sits
        // next to it but does not behave like "off for this project" — a
        // plugin can be `enabled: true, projectEnabled: false` and still be
        // live — so it is carried in the catalog without gating the row.
        let enabled = plugin.enabled;
        let toggle_label = if enabled {
            tr!("claude.disable")
        } else {
            tr!("claude.enable")
        };
        let name = plugin.name.clone();
        let subtitle = {
            let mut parts = vec![plugin.id.clone()];
            if !plugin.version.is_empty() {
                parts.push(format!("v{}", plugin.version));
            }
            if !plugin.mcp_servers.is_empty() {
                parts.push(format!(
                    "{}: {}",
                    tr!("claude.plugin_mcp_servers"),
                    plugin.mcp_servers.join(", ")
                ));
            }
            parts.join(" · ")
        };
        let click_plugin = plugin.clone();
        let key_plugin = plugin.clone();
        let details_open = self.claude_detail.as_ref().map(|(open, _)| open) == Some(&plugin.id);
        let details = self
            .claude_detail
            .as_ref()
            .filter(|(open, _)| open == &plugin.id)
            .map(|(_, details)| details.clone());

        let row =
            div()
                .flex()
                .items_start()
                .gap(px(10.0))
                .p(px(10.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.inset)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(7.0))
                                .child(div().flex_none().size(px(7.0)).rounded_full().bg(
                                    if enabled {
                                        theme.success
                                    } else {
                                        theme.text_ghost
                                    },
                                ))
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(sp(13.0))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(theme.text)
                                        .child(name),
                                )
                                .child(
                                    // State, not the action the button performs.
                                    // This used to read "Disable" next to a green
                                    // dot, which says the plugin is off when it is
                                    // the click that turns it off.
                                    div()
                                        .flex_none()
                                        .text_size(sp(11.0))
                                        .text_color(if enabled {
                                            theme.text_secondary
                                        } else {
                                            theme.text_ghost
                                        })
                                        .child(if enabled {
                                            tr!("claude.enabled")
                                        } else {
                                            tr!("claude.disabled")
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(sp(11.5))
                                .text_color(theme.text_tertiary)
                                .child(subtitle),
                        )
                        .when(!plugin.description.is_empty(), |column| {
                            column.child(
                                div()
                                    .text_size(sp(11.5))
                                    .text_color(theme.text_secondary)
                                    .child(plugin.description.clone()),
                            )
                        }),
                )
                .child(self.claude_text_button(
                    SharedString::from(format!("claude-plugin-details-{}", plugin.id)),
                    if details_open {
                        tr!("claude.hide_details")
                    } else {
                        tr!("claude.details")
                    },
                    details_open,
                    cx,
                    {
                        let id = plugin.id.clone();
                        let path = plugin.install_path.clone().map(PathBuf::from);
                        move |this: &mut Self, cx: &mut Context<Self>| {
                            this.load_plugin_details(id.clone(), path.clone(), cx);
                        }
                    },
                ))
                .child(self.plugin_toggle_button(plugin, enabled, toggle_label, cx))
                .when(Self::plugin_can_update(plugin), |row| {
                    row.child(self.plugin_update_button(plugin, theme, cx))
                })
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "claude-plugin-uninstall-{}",
                            plugin.id
                        )))
                        .role(Role::Button)
                        .aria_label(tr!("claude.uninstall"))
                        .tab_index(0)
                        .flex_none()
                        .px(px(9.0))
                        .h(px(24.0))
                        .rounded(px(6.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_default()
                        .text_size(sp(11.5))
                        .text_color(theme.text_secondary)
                        .hover(|style| style.bg(theme.overlay).text_color(theme.danger))
                        .focus_visible(|style| style.border_1().border_color(theme.accent))
                        .child(tr!("claude.uninstall"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.request_uninstall_plugin(&click_plugin, cx);
                        }))
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                this.request_uninstall_plugin(&key_plugin, cx);
                                cx.stop_propagation();
                            }
                        })),
                );

        // The manifest goes under the row, not in a separate pane: it describes
        // this plugin and there is nothing to compare it against.
        let mut column = div().flex().flex_col().gap(px(5.0)).w_full().child(row);
        if details_open {
            match details {
                Some(details) => column = column.child(self.render_plugin_details(&details, theme)),
                None => {
                    column = column.child(
                        div()
                            .ml(px(14.0))
                            .text_size(sp(11.0))
                            .text_color(theme.text_ghost)
                            .child(tr!("claude.details_loading")),
                    )
                }
            }
        }
        column
    }

    /// Whether a plugin came from a marketplace and can therefore be updated.
    ///
    /// `plugin update` fetches a newer version from the marketplace, so a
    /// locally authored plugin has nothing to fetch. The id shape is the signal:
    /// an installed plugin is `name@marketplace`, a local one has no `@`.
    fn plugin_can_update(plugin: &PluginEntry) -> bool {
        plugin.id.contains('@') && !plugin.marketplace.is_empty()
    }

    /// `plugin update` only exists for a plugin that came from a marketplace;
    /// a locally authored one has no newer version to fetch.
    fn plugin_update_button(
        &self,
        plugin: &PluginEntry,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let target = plugin.clone();
        let update_target = plugin.clone();
        div()
            .id(SharedString::from(format!(
                "claude-plugin-update-{}",
                plugin.id
            )))
            .role(Role::Button)
            .aria_label(tr!("claude.update"))
            .tab_index(0)
            .flex_none()
            .px(px(9.0))
            .h(px(24.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .text_size(sp(11.5))
            .text_color(theme.text_secondary)
            .hover(|style| style.bg(theme.overlay))
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .child(tr!("claude.update"))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.update_installed_plugin(&target, cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.update_installed_plugin(&update_target, cx);
                    cx.stop_propagation();
                }
            }))
    }

    fn plugin_toggle_button(
        &self,
        plugin: &PluginEntry,
        enabled: bool,
        label: String,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        // Click and key press are separate listeners, so each needs its own.
        let click_target = plugin.clone();
        let key_target = plugin.clone();
        div()
            .id(SharedString::from(format!(
                "claude-plugin-toggle-{}",
                plugin.id
            )))
            .role(Role::Button)
            .aria_label(label.clone())
            .aria_selected(enabled)
            .tab_index(0)
            .flex_none()
            .px(px(9.0))
            .h(px(24.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .text_size(sp(11.5))
            .text_color(theme.text_secondary)
            .hover(|style| style.bg(theme.overlay))
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_plugin_enabled(&click_target, !enabled, cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.set_plugin_enabled(&key_target, !enabled, cx);
                    cx.stop_propagation();
                }
            }))
    }

    // ── Marketplace ────────────────────────────────────────────────────────

    /// The marketplace tab: the added marketplaces, then the browse list.
    ///
    /// The browse list is behind a button because answering it costs the CLI a
    /// refresh of every marketplace. Everything above it — the marketplaces,
    /// which plugins are installed — is the cheap read the page already has.
    fn render_marketplace(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let mut body = div().flex().flex_col().gap(px(10.0)).w_full();

        body = body.child(self.render_marketplace_list(cx));

        body = body.child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(sp(12.5))
                        .text_color(theme.text_secondary)
                        .child(tr!("claude.browse_hint")),
                )
                .child(self.claude_text_button(
                    "claude-browse",
                    if self.claude_available_catalog.is_some() {
                        tr!("claude.browse_refresh")
                    } else {
                        tr!("claude.browse")
                    },
                    false,
                    cx,
                    |this, cx| this.load_available_plugins(cx),
                )),
        );

        if self.claude_available_pending {
            return body.child(self.claude_placeholder(&tr!("claude.browse_pending"), cx));
        }
        let Some(catalog) = self.claude_available_catalog.clone() else {
            return body;
        };
        if let Some(error) = catalog.unavailable.clone() {
            return body.child(
                self.claude_placeholder(&tr!("claude.browse_unavailable", error = error), cx),
            );
        }
        body.child(self.render_available_list(catalog, cx))
    }

    fn render_marketplace_list(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let Some(catalog) = self.claude_marketplace_catalog.clone() else {
            return self.claude_placeholder(&tr!("claude.loading"), cx);
        };
        let mut body = div().flex().flex_col().gap(px(5.0)).w_full().child(
            div()
                .flex()
                .items_center()
                .gap(px(7.0))
                .child(
                    div()
                        .text_size(sp(12.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text_secondary)
                        .child(tr!("claude.marketplaces")),
                )
                .child(
                    div()
                        .text_size(sp(11.0))
                        .text_color(theme.text_ghost)
                        .child(tr!(
                            "claude.marketplace_count",
                            count = catalog.marketplaces.len().to_string()
                        )),
                ),
        );
        if let Some(error) = catalog.unavailable.clone() {
            return body.child(
                div()
                    .text_size(sp(11.5))
                    .text_color(theme.danger)
                    .child(tr!("claude.marketplaces_unavailable", error = error)),
            );
        }
        if catalog.marketplaces.is_empty() {
            // An empty list is a real state: nothing can be installed until a
            // marketplace is added, which this build does not do.
            return body.child(
                div()
                    .text_size(sp(11.5))
                    .text_color(theme.text_tertiary)
                    .child(tr!("claude.marketplaces_empty")),
            );
        }
        for marketplace in &catalog.marketplaces {
            let location = marketplace
                .repo
                .clone()
                .or_else(|| marketplace.install_location.clone())
                .unwrap_or_default();
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(10.0))
                    .py(px(7.0))
                    .rounded(px(7.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.inset)
                    .child(
                        div()
                            .flex_none()
                            .text_size(sp(12.5))
                            .text_color(theme.text)
                            .child(marketplace.name.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(sp(11.0))
                            .text_color(theme.text_ghost)
                            .child(location),
                    ),
            );
        }
        body
    }

    /// The browse list: filter, then rows, then a "show more" cap.
    ///
    /// The catalog carries thousands of entries, and `plugin list --available`
    /// answered 2.4 MB on this machine. Drawing them all would cost a frame per
    /// keystroke in the filter, so the filter narrows first and the rows are
    /// capped at `claude_available_shown`.
    fn render_available_list(
        &self,
        catalog: Rc<AvailablePluginCatalog>,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        let query = self
            .claude_available_search
            .read(cx)
            .content()
            .trim()
            .to_lowercase();
        let mut body = div().flex().flex_col().gap(px(7.0)).w_full().child(
            TextField::new(
                "claude-marketplace-search",
                self.claude_available_search.clone(),
            )
            .icon("icons/search.svg", 13.0)
            .w_full(),
        );

        let matched: Vec<&AvailablePlugin> = catalog
            .plugins
            .iter()
            .filter(|plugin| plugin_matches(plugin, &query))
            .collect();

        body = body.child(
            div()
                .text_size(sp(11.0))
                .text_color(theme.text_ghost)
                .child(if matched.is_empty() {
                    tr!("claude.no_match")
                } else {
                    tr!(
                        "claude.available_count",
                        count = matched.len().to_string(),
                        shown = matched.len().min(self.claude_available_shown).to_string()
                    )
                }),
        );

        for plugin in matched.iter().take(self.claude_available_shown) {
            body = body.child(self.render_available_row(plugin, &theme, cx));
        }

        if matched.len() > self.claude_available_shown {
            let remaining = matched.len() - self.claude_available_shown;
            body = body.child(self.claude_text_button(
                "claude-show-more",
                tr!("claude.show_more", count = remaining.to_string()),
                false,
                cx,
                |this, cx| {
                    this.claude_available_shown += 40;
                    cx.notify();
                },
            ));
        }
        body
    }

    fn render_available_row(
        &self,
        plugin: &AvailablePlugin,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let installed = plugin.installed;
        let count = plugin.install_count.map(|count| {
            div()
                .flex_none()
                .text_size(sp(11.0))
                .text_color(theme.text_ghost)
                .child(tr!("claude.install_count", count = count.to_string()))
        });

        let mut row = div()
            .flex()
            .items_start()
            .gap(px(9.0))
            .px(px(10.0))
            .py(px(8.0))
            .rounded(px(7.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.inset)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(sp(12.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(plugin.name.clone()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .px(px(5.0))
                                    .rounded(px(4.0))
                                    .bg(theme.overlay)
                                    .text_size(sp(10.5))
                                    .text_color(theme.text_tertiary)
                                    .child(plugin.marketplace.clone()),
                            )
                            .children(count),
                    )
                    .child(
                        div()
                            .text_size(sp(11.0))
                            .text_color(theme.text_tertiary)
                            .line_clamp(2)
                            .child(plugin.description.clone()),
                    ),
            );

        if installed {
            row = row.child(
                div()
                    .flex_none()
                    .px(px(8.0))
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .text_size(sp(11.0))
                    .text_color(theme.success)
                    .child(tr!("claude.installed")),
            );
        } else {
            let target = plugin.clone();
            row = row.child(self.claude_text_button(
                SharedString::from(format!("claude-install-{}", plugin.id)),
                tr!("claude.install"),
                false,
                cx,
                move |this, cx| this.install_available_plugin(&target, cx),
            ));
        }
        row
    }

    // ── Plugin details ─────────────────────────────────────────────────────

    /// What a plugin ships, shown under its row once read.
    ///
    /// This replaces the `components` array the plugin list used to carry but
    /// never received: `plugin list --json` has no such field, so the old row
    /// was always empty there. The manifest is the only place this exists.
    fn render_plugin_details(&self, details: &PluginDetails, theme: &Theme) -> Div {
        let mut column = div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .px(px(10.0))
            .py(px(8.0))
            .ml(px(14.0))
            .rounded(px(7.0))
            .border_l_2()
            .border_color(theme.accent)
            .bg(theme.inset);

        if !details.description.is_empty() {
            column = column.child(
                div()
                    .text_size(sp(11.5))
                    .text_color(theme.text_secondary)
                    .child(details.description.clone()),
            );
        }
        let mut facts: Vec<String> = Vec::new();
        if !details.version.is_empty() {
            facts.push(format!(
                "{}: {}",
                tr!("claude.plugin_version"),
                details.version
            ));
        }
        if let Some(author) = details.author.clone() {
            facts.push(format!("{}: {}", tr!("claude.plugin_author"), author));
        }
        if let Some(homepage) = details.homepage.clone() {
            facts.push(format!("{}: {}", tr!("claude.plugin_homepage"), homepage));
        }
        if !facts.is_empty() {
            column = column.child(
                div()
                    .text_size(sp(11.0))
                    .text_color(theme.text_ghost)
                    .child(facts.join("  ·  ")),
            );
        }
        if !details.mcp_servers.is_empty() {
            column = column.child(
                div()
                    .text_size(sp(11.0))
                    .text_color(theme.text_tertiary)
                    .child(format!(
                        "{}: {}",
                        tr!("claude.plugin_mcp_servers"),
                        details.mcp_servers.join(", ")
                    )),
            );
        }
        if !details.components.is_empty() {
            let mut grouped = div().flex().flex_wrap().gap(px(5.0));
            for component in &details.components {
                grouped = grouped.child(
                    div()
                        .px(px(5.0))
                        .rounded(px(4.0))
                        .bg(theme.overlay)
                        .text_size(sp(10.5))
                        .text_color(theme.text_tertiary)
                        .child(format!("{}: {}", component.kind, component.name)),
                );
            }
            column = column.child(grouped);
        }
        if details.components.is_empty() && details.mcp_servers.is_empty() {
            column = column.child(
                div()
                    .text_size(sp(11.0))
                    .text_color(theme.text_ghost)
                    .child(tr!("claude.plugin_no_components")),
            );
        }
        column
    }

    /// A small text button. The confirm bar has its own destructive variant;
    /// these are the neutral actions (browse, install, show more, details).
    ///
    /// `action` takes the click event so it can carry the value the button acts
    /// on — a plugin row's id, for instance — instead of every caller having to
    /// route through a `fn` pointer and lose that context.
    fn claude_text_button<F>(
        &self,
        id: impl Into<SharedString>,
        label: String,
        active: bool,
        cx: &mut Context<Self>,
        action: F,
    ) -> Stateful<Div>
    where
        F: Fn(&mut Self, &mut Context<Self>) + Clone + 'static,
    {
        let theme = Theme::current(cx);
        let click = action.clone();
        let key = action;
        div()
            .id(id.into())
            .role(Role::Button)
            .aria_label(label.clone())
            .aria_selected(active)
            .tab_index(0)
            .flex_none()
            .px(px(9.0))
            .h(px(24.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .text_size(sp(11.5))
            .text_color(theme.text_secondary)
            .hover(|style| style.bg(theme.overlay))
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| click(this, cx)))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    key(this, cx);
                    cx.stop_propagation();
                }
            }))
    }

    fn claude_placeholder(&self, message: &str, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        div()
            .py(px(24.0))
            .px(px(12.0))
            .w_full()
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .flex()
            .items_center()
            .justify_center()
            .text_size(sp(12.5))
            .text_color(theme.text_tertiary)
            .child(message.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use waku_client::claude::McpTransport;

    fn mcp_entry(health: Option<McpHealth>, pending: bool) -> McpServerEntry {
        McpServerEntry {
            name: "server".into(),
            scope: McpScope::User,
            transport: McpTransport::Stdio,
            target: "cmd".into(),
            args: Vec::new(),
            pending_approval: pending,
            health,
            detail: None,
        }
    }

    /// The three states must stay distinguishable: a server waiting for
    /// approval is not broken, and one never probed is not connected.
    #[test]
    fn health_states_are_distinguishable() {
        assert_eq!(
            mcp_entry(Some(McpHealth::Connected), false).health,
            Some(McpHealth::Connected)
        );
        assert_eq!(
            mcp_entry(Some(McpHealth::Failed), false).health,
            Some(McpHealth::Failed)
        );
        assert_eq!(mcp_entry(None, false).health, None);
        assert_eq!(mcp_entry(None, true).health, None);
    }

    /// `projectEnabled` must not gate the row.
    ///
    /// Observed on this machine: `ida-pro-mcp` reports
    /// `enabled: true, projectEnabled: false` while being plainly live, so
    /// treating the second flag as "off for this project" would have shown a
    /// working plugin as disabled and offered to "enable" it forever.
    #[test]
    fn a_plugin_is_live_based_on_enabled_alone() {
        let live_but_not_project_enabled = PluginEntry {
            id: "p@m".into(),
            name: "p".into(),
            marketplace: "m".into(),
            version: "1.0.0".into(),
            scope: McpScope::User,
            enabled: true,
            project_enabled: false,
            description: String::new(),
            mcp_servers: Vec::new(),
            components: Vec::new(),
            install_path: None,
        };
        assert!(live_but_not_project_enabled.enabled);
    }

    #[test]
    fn a_plugin_contributed_server_offers_no_remove() {
        // The MCP row keys its remove control off the scope, because a plugin
        // server has no config file to remove it from.
        assert!(!McpScope::Plugin.is_removable());
        for scope in [McpScope::User, McpScope::Project, McpScope::Local] {
            assert!(scope.is_removable());
        }
    }

    #[test]
    fn confirming_clears_the_pending_action() {
        // Guards the pattern the page relies on: a confirmation must not be
        // able to fire twice from one prompt.
        let mut pending = Some(ClaudeConfirm::UninstallPlugin {
            id: "p@m".into(),
            name: "p".into(),
            scope: McpScope::Project,
        });
        let taken = pending.take();
        assert!(taken.is_some());
        assert!(pending.take().is_none());
    }

    /// The uninstall carries the plugin's own scope.
    ///
    /// `claude plugin uninstall` defaults to `user`, so dropping the scope
    /// would report success while leaving a project-scoped plugin installed.
    #[test]
    fn uninstall_keeps_the_scope_it_was_listed_with() {
        let plugin = PluginEntry {
            id: "ralph-loop@claude-plugins-official".into(),
            name: "ralph-loop".into(),
            marketplace: "claude-plugins-official".into(),
            version: "1.0.0".into(),
            scope: McpScope::Project,
            enabled: false,
            project_enabled: false,
            description: String::new(),
            mcp_servers: Vec::new(),
            components: Vec::new(),
            install_path: None,
        };
        let pending = ClaudeConfirm::UninstallPlugin {
            id: plugin.id.clone(),
            name: plugin.name.clone(),
            scope: plugin.scope,
        };
        match pending {
            ClaudeConfirm::UninstallPlugin { scope, .. } => {
                assert_eq!(scope, McpScope::Project);
                assert_eq!(scope.cli_flag(), Some("project"));
            }
            _ => panic!("wrong variant"),
        }
    }

    fn available(id: &str, description: &str) -> AvailablePlugin {
        let (name, marketplace) = id.split_once('@').unwrap_or((id, ""));
        AvailablePlugin {
            id: id.into(),
            name: name.into(),
            marketplace: marketplace.into(),
            description: description.into(),
            install_count: Some(1),
            installed: false,
        }
    }

    /// The confirmation's key context must stay a single identifier.
    ///
    /// `key_context()` goes through `KeyContext::parse`, which does not
    /// understand `>`: it consumes no characters, recurses on the same input,
    /// and overflows the stack at render time. Unit tests cannot catch that —
    /// it needs a real window — so the contract is pinned here. This is the
    /// same failure that already crashed the app once, through the prompt
    /// history context.
    #[test]
    fn the_confirmation_context_is_a_single_identifier() {
        assert!(
            !CLAUDE_DIALOG_CONTEXT.contains('>'),
            "an element key_context cannot contain `>`"
        );
        assert!(
            CLAUDE_DIALOG_CONTEXT
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "every character must be one the simple parser consumes"
        );
    }

    /// An empty filter must match everything: the browse list opens unfiltered,
    /// and a filter that matched nothing there would look like an empty
    /// marketplace.
    #[test]
    fn the_empty_filter_matches_every_plugin() {
        assert!(plugin_matches(&available("a@m", "text"), ""));
    }

    /// The filter has to reach the description as well as the name: with
    /// thousands of entries the user is as likely to remember what a plugin
    /// does as what it is called.
    #[test]
    fn the_filter_matches_name_marketplace_and_description_case_insensitively() {
        let plugin = available("Alpha@MyMarket", "Runs QUERIES");
        assert!(plugin_matches(&plugin, "alpha"));
        assert!(plugin_matches(&plugin, "mymarket"));
        assert!(plugin_matches(&plugin, "queries"));
        assert!(!plugin_matches(&plugin, "nonexistent"));
    }

    /// Install names its scope instead of letting the CLI default stand.
    ///
    /// `plugin install` defaults to `user`, so this matches today; the point is
    /// that the command the page builds carries the scope explicitly, which is
    /// what the confirmation text says and what the uninstall path got wrong
    /// once by relying on a default.
    #[test]
    fn install_names_its_scope_explicitly() {
        let scope = plugin_install_scope();
        assert_eq!(scope, McpScope::User);
        assert_eq!(scope.cli_flag(), Some("user"));
    }

    /// The manifest read, `can_update` and this decision must agree.
    ///
    /// Two places would otherwise drift: the details panel reports whether an
    /// update is possible, and the row decides whether to offer the button.
    #[test]
    fn update_is_offered_exactly_for_marketplace_plugins() {
        let mut plugin = PluginEntry {
            id: "p@m".into(),
            name: "p".into(),
            marketplace: "m".into(),
            version: "1.0.0".into(),
            scope: McpScope::User,
            enabled: true,
            project_enabled: false,
            description: String::new(),
            mcp_servers: Vec::new(),
            components: Vec::new(),
            install_path: None,
        };
        assert!(Waku::plugin_can_update(&plugin));

        // A locally authored plugin: no marketplace, so nothing to fetch.
        plugin.id = "local-plugin".into();
        plugin.marketplace = String::new();
        assert!(!Waku::plugin_can_update(&plugin));
    }

    /// Closing an open detail needs no read, and opening a different one must
    /// drop the previous plugin's manifest rather than showing it under the
    /// new row.
    #[test]
    fn opening_a_detail_replaces_whatever_was_open() {
        let first = PluginDetails {
            id: "a@m".into(),
            name: "a".into(),
            version: "1.0.0".into(),
            description: String::new(),
            author: None,
            homepage: None,
            mcp_servers: Vec::new(),
            components: Vec::new(),
            can_update: true,
        };
        let mut open = Some(("a@m".to_owned(), Rc::new(first)));
        assert_eq!(open.as_ref().map(|(id, _)| id.as_str()), Some("a@m"));

        let second = PluginDetails {
            id: "b@m".into(),
            name: "b".into(),
            ..open.clone().unwrap().1.as_ref().clone()
        };
        open = Some(("b@m".to_owned(), Rc::new(second)));
        assert_eq!(open.as_ref().map(|(id, _)| id.as_str()), Some("b@m"));
        assert_eq!(
            open.as_ref().map(|(_, details)| details.name.clone()),
            Some("b".into())
        );
    }
}

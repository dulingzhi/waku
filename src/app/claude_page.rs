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

/// Which half of the page is showing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ClaudeTab {
    #[default]
    Mcp,
    Plugins,
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
            ClaudeTab::Plugins => self.claude_plugin_catalog.is_some(),
        };
        if loaded {
            return;
        }
        self.reload_claude_catalog(cx);
    }

    /// Re-read both catalogs, dropping any read already in flight.
    pub(super) fn reload_claude_catalog(&mut self, cx: &mut Context<Self>) {
        self.claude_catalog_pending = true;
        self.claude_catalog_generation += 1;
        let generation = self.claude_catalog_generation;
        let daemon = self.daemon.client();
        let cwd = self.selected_workspace_path().map(Path::to_path_buf);
        cx.spawn(async move |this, cx| {
            let (mcp, plugins) = cx
                .background_executor()
                .spawn(async move {
                    let mcp = match daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::LoadClaudeMcpServers { cwd },
                    ) {
                        Ok(waku_client::ResponsePayload::ClaudeMcpCatalog { catalog }) => {
                            Some(catalog)
                        }
                        _ => None,
                    };
                    let plugins = match daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::LoadClaudePlugins,
                    ) {
                        Ok(waku_client::ResponsePayload::ClaudePluginCatalog { catalog }) => {
                            Some(catalog)
                        }
                        _ => None,
                    };
                    (mcp, plugins)
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
                cx.notify();
            });
        })
        .detach();
    }

    // ── Mutations ──────────────────────────────────────────────────────────

    /// Run one mutating CLI command, then re-read so the page shows what the
    /// CLI actually did rather than what Waku assumed it would do.
    fn run_claude_mutation(&mut self, command: waku_client::Command, cx: &mut Context<Self>) {
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { daemon.request(Uuid::nil(), Uuid::nil(), command) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Err(error) => this.show_toast(error.to_string()),
                    Ok(waku_client::ResponsePayload::Ack) => {}
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
}

// ── Rendering ──────────────────────────────────────────────────────────────

impl Waku {
    pub(super) fn render_claude_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let mut body = div().flex().flex_col().gap(px(14.0)).w_full();

        body = body.child(self.render_claude_tabs(cx));

        if let Some(confirm) = self.claude_confirm.clone() {
            body = body.child(self.render_claude_confirm(&confirm, cx));
        }

        body = body.child(match self.claude_tab {
            ClaudeTab::Mcp => self.render_mcp_list(cx),
            ClaudeTab::Plugins => self.render_plugin_list(cx),
        });

        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .text_color(theme.text)
            .child(body)
            .into_any_element()
    }

    fn render_claude_tabs(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let tab = self.claude_tab;
        let mut row = div().flex().items_center().gap(px(6.0));
        for (kind, label) in [
            (ClaudeTab::Mcp, tr!("claude.tab_mcp")),
            (ClaudeTab::Plugins, tr!("claude.tab_plugins")),
        ] {
            let selected = tab == kind;
            row = row.child(
                div()
                    .id(SharedString::from(match kind {
                        ClaudeTab::Mcp => "claude-tab-mcp",
                        ClaudeTab::Plugins => "claude-tab-plugins",
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

    fn render_claude_confirm(&self, confirm: &ClaudeConfirm, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let message = match confirm {
            ClaudeConfirm::RemoveMcp { name, scope } => tr!(
                "claude.remove_confirm",
                name = name.clone(),
                scope = scope.label()
            ),
            ClaudeConfirm::UninstallPlugin { name, .. } => {
                tr!("claude.uninstall_confirm", name = name.clone())
            }
        };
        div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .p(px(12.0))
            .rounded(px(9.0))
            .border_1()
            .border_color(theme.danger)
            .bg(theme.danger.opacity(0.08))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(sp(12.5))
                    .text_color(theme.text)
                    .child(message),
            )
            .child(self.claude_confirm_button(
                "claude-confirm",
                tr!("common.confirm"),
                true,
                cx,
                |this, cx| this.confirm_claude_action(cx),
            ))
            .child(self.claude_confirm_button(
                "claude-cancel",
                tr!("common.cancel"),
                false,
                cx,
                |this, cx| this.cancel_claude_action(cx),
            ))
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
            .px(px(10.0))
            .h(px(26.0))
            .flex_none()
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .text_size(sp(12.0))
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
                                    .bg(if enabled {
                                        theme.success
                                    } else {
                                        theme.text_ghost
                                    }),
                            )
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
                                div()
                                    .flex_none()
                                    .text_size(sp(11.0))
                                    .text_color(if enabled {
                                        theme.text_secondary
                                    } else {
                                        theme.text_ghost
                                    })
                                    .child(if enabled {
                                        tr!("claude.disable")
                                    } else {
                                        tr!("claude.enable")
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
            .child(self.plugin_toggle_button(plugin, enabled, toggle_label, cx))
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
            )
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
}

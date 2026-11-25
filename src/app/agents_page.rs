//! The Agents settings page: the identities a delegate can be started as, and
//! the teams that group them.
//!
//! An identity is a small document — name, role label, what it is for, how it
//! works, what it must not do, which model it runs on. That is too much to edit
//! inline in a list, so the page keeps the two-column shape the Skills page
//! already uses: a list on the left that answers "which agents do I have", and
//! a detail pane on the right that answers "what is this one".
//!
//! The catalog lives in files, so every mutation re-reads it from the daemon
//! that owns those files. Nothing is edited in place and written back later: a
//! half-applied identity is worse than one the user has to retype.

use gpui::{SharedString, div, px};

use super::*;

/// Which list the page's left column is showing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AgentsView {
    Identities,
    Teams,
}

impl Waku {
    // ── Catalog ────────────────────────────────────────────────────────────

    /// Re-reads the catalog unless a read is already in flight.
    ///
    /// Unlike the skills scan there is no staleness window: an identity is
    /// edited through this page, so the copy on screen is the copy the daemon
    /// just wrote and there is nothing to go stale behind the user's back.
    pub(super) fn ensure_agents_catalog(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.agents_scan_pending {
            return;
        }
        if !force && self.agents_catalog.is_some() {
            return;
        }
        self.agents_scan_pending = true;
        self.agents_scan_generation += 1;
        let generation = self.agents_scan_generation;
        let project = self.selected_project().map(|project| project.path.clone());
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let catalog = cx
                .background_executor()
                .spawn(async move {
                    match daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::LoadAgents { project },
                    )? {
                        waku_client::ResponsePayload::AgentsCatalog { catalog } => Ok(catalog),
                        _ => anyhow::bail!("the daemon returned an invalid agents response"),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.agents_scan_generation != generation {
                    // A mutation invalidated this read; the flag and the result
                    // now belong to the newer one.
                    return;
                }
                this.agents_scan_pending = false;
                match catalog {
                    Ok(catalog) => this.agents_catalog = Some(Rc::new(catalog)),
                    Err(error) => this.show_toast(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Writes one identity and republishes the catalog the daemon reports.
    pub(super) fn save_agent_definition(
        &mut self,
        definition: waku_client::AgentDefinition,
        cx: &mut Context<Self>,
    ) {
        self.send_agents_command(
            waku_client::Command::SaveAgentDefinition {
                definition,
                project: self.selected_project().map(|project| project.path.clone()),
            },
            cx,
        );
    }

    pub(super) fn delete_agent_definition(
        &mut self,
        scope: waku_client::AgentDefinitionScope,
        id: String,
        cx: &mut Context<Self>,
    ) {
        self.send_agents_command(
            waku_client::Command::DeleteAgentDefinition {
                scope,
                id,
                project: self.selected_project().map(|project| project.path.clone()),
            },
            cx,
        );
    }

    pub(super) fn save_agent_team(&mut self, team: waku_client::AgentTeam, cx: &mut Context<Self>) {
        self.send_agents_command(
            waku_client::Command::SaveAgentTeam {
                team,
                project: self.selected_project().map(|project| project.path.clone()),
            },
            cx,
        );
    }

    pub(super) fn delete_agent_team(
        &mut self,
        scope: waku_client::AgentDefinitionScope,
        id: String,
        cx: &mut Context<Self>,
    ) {
        self.send_agents_command(
            waku_client::Command::DeleteAgentTeam {
                scope,
                id,
                project: self.selected_project().map(|project| project.path.clone()),
            },
            cx,
        );
    }

    /// One write, then whichever catalog the daemon answers with.
    ///
    /// Every mutation command replies with the whole catalogue rather than an
    /// acknowledgement, so an edit, a delete, and a first read all leave the
    /// page showing the same authoritative list — including the case where a
    /// delete removed the identity that was selected.
    fn send_agents_command(&mut self, command: waku_client::Command, cx: &mut Context<Self>) {
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match daemon.request(Uuid::nil(), Uuid::nil(), command)? {
                        waku_client::ResponsePayload::AgentsCatalog { catalog } => Ok(catalog),
                        _ => anyhow::bail!("the daemon returned an invalid agents response"),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(catalog) => {
                        this.agents_catalog = Some(Rc::new(catalog));
                        // The selection may name something the write removed;
                        // dropping it lets the pane fall back to the first row
                        // instead of showing a definition that no longer exists.
                        this.reconcile_agents_selection();
                    }
                    Err(error) => this.show_toast(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Drops a selection the catalog no longer contains.
    fn reconcile_agents_selection(&mut self) {
        let Some(catalog) = self.agents_catalog.clone() else {
            return;
        };
        if let Some(selected) = self.agents_selected.as_deref()
            && catalog.find(selected).is_none()
        {
            self.agents_selected = None;
        }
        if let Some(selected) = self.agents_selected_team.as_deref()
            && catalog.find_team(selected).is_none()
        {
            self.agents_selected_team = None;
        }
    }

    /// The identity the detail pane shows, falling back to the first one.
    fn selected_agent_definition(&self) -> Option<&waku_client::AgentDefinition> {
        let catalog = self.agents_catalog.as_deref()?;
        self.agents_selected
            .as_deref()
            .and_then(|id| catalog.find(id))
            .or_else(|| catalog.definitions.first())
    }

    fn selected_agent_team(&self) -> Option<&waku_client::AgentTeam> {
        let catalog = self.agents_catalog.as_deref()?;
        self.agents_selected_team
            .as_deref()
            .and_then(|id| catalog.find_team(id))
            .or_else(|| catalog.teams.first())
    }

    // ── Rendering ──────────────────────────────────────────────────────────

    pub(super) fn render_agents_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let Some(catalog) = self.agents_catalog.clone() else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(sp(13.0))
                .text_color(theme.text_tertiary)
                .child(tr!("agents.loading"))
                .into_any_element();
        };
        let weak = cx.entity().downgrade();
        let switch = self.render_agents_view_switch(&theme, weak.clone());
        let list = match self.agents_view {
            AgentsView::Identities => self.render_agents_identity_list(&catalog, &theme, weak.clone()),
            AgentsView::Teams => self.render_agents_team_list(&catalog, &theme, weak.clone()),
        };
        let detail = match self.agents_view {
            AgentsView::Identities => self.render_agents_identity_detail(&theme),
            AgentsView::Teams => self.render_agents_team_detail(&catalog, &theme),
        };
        // A file the user wrote and mistyped has to be findable: dropping it
        // silently would look like the page lost their work.
        let problems = (!catalog.problems.is_empty())
            .then(|| self.render_agents_problems(&catalog, &theme))
            .into_iter()
            .collect::<Vec<_>>();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(switch)
            .children(problems)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .gap(px(14.0))
                    .child(list)
                    .child(detail),
            )
            .into_any_element()
    }

    fn render_agents_view_switch(&self, theme: &Theme, weak: WeakEntity<Waku>) -> impl IntoElement {
        let mut row = div().flex().items_center().gap(px(6.0));
        for (view, key) in [
            (AgentsView::Identities, "agents.identities"),
            (AgentsView::Teams, "agents.teams"),
        ] {
            let selected = self.agents_view == view;
            row = row.child(
                div()
                    .id(SharedString::from(format!("agents-view-{key}")))
                    .px(px(10.0))
                    .py(px(5.0))
                    .rounded(px(7.0))
                    .cursor_default()
                    .text_size(sp(12.5))
                    .font_weight(FontWeight::MEDIUM)
                    .when(selected, |element| {
                        element.bg(theme.overlay).text_color(theme.text)
                    })
                    .when(!selected, |element| {
                        element.text_color(theme.text_tertiary)
                    })
                    .hover(|style| style.bg(theme.overlay))
                    .child(tr!(key))
                    .on_click({
                        let weak = weak.clone();
                        move |_, _, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.agents_view = view;
                                cx.notify();
                            });
                        }
                    }),
            );
        }
        row
    }

    fn render_agents_identity_list(
        &self,
        catalog: &waku_client::AgentsCatalog,
        theme: &Theme,
        weak: WeakEntity<Waku>,
    ) -> AnyElement {
        if catalog.definitions.is_empty() {
            return self.render_agents_empty(theme);
        }
        let selected = self.selected_agent_definition().map(|it| it.id.clone());
        let mut column = div()
            .id("agents-identity-list")
            .w(px(260.0))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .overflow_y_scroll();
        for definition in &catalog.definitions {
            let id = definition.id.clone();
            let is_selected = selected.as_deref() == Some(id.as_str());
            let scope = definition.scope.label();
            column = column.child(
                div()
                    .id(SharedString::from(format!("agents-identity-{id}")))
                    .px(px(10.0))
                    .py(px(8.0))
                    .rounded(px(8.0))
                    .cursor_default()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .when(is_selected, |element| {
                        element.bg(theme.overlay).border_1().border_color(theme.border_strong)
                    })
                    .when(!is_selected, |element| element.border_1().border_color(theme.border))
                    .hover(|style| style.bg(theme.overlay))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(sp(11.0))
                                    .text_color(theme.accent)
                                    .child(SharedString::from(definition.role.clone())),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .truncate()
                                    .text_size(sp(12.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(SharedString::from(definition.name.clone())),
                            ),
                    )
                    .child(
                        div()
                            .text_size(sp(11.0))
                            .text_color(theme.text_tertiary)
                            .line_clamp(2)
                            .child(SharedString::from(definition.description.clone())),
                    )
                    .child(
                        div()
                            .text_size(sp(10.5))
                            .text_color(theme.text_ghost)
                            .child(SharedString::from(scope)),
                    )
                    .on_click({
                        let weak = weak.clone();
                        move |_, _, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.agents_selected = Some(id.clone());
                                cx.notify();
                            });
                        }
                    }),
            );
        }
        column.into_any_element()
    }

    fn render_agents_team_list(
        &self,
        catalog: &waku_client::AgentsCatalog,
        theme: &Theme,
        weak: WeakEntity<Waku>,
    ) -> AnyElement {
        if catalog.teams.is_empty() {
            return div()
                .w(px(260.0))
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .text_size(sp(12.5))
                        .text_color(theme.text_secondary)
                        .child(tr!("agents.no_teams")),
                )
                .into_any_element();
        }
        let selected = self.selected_agent_team().map(|it| it.id.clone());
        let mut column = div()
            .id("agents-team-list")
            .w(px(260.0))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .overflow_y_scroll();
        for team in &catalog.teams {
            let id = team.id.clone();
            let is_selected = selected.as_deref() == Some(id.as_str());
            column = column.child(
                div()
                    .id(SharedString::from(format!("agents-team-{id}")))
                    .px(px(10.0))
                    .py(px(8.0))
                    .rounded(px(8.0))
                    .cursor_default()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .when(is_selected, |element| {
                        element.bg(theme.overlay).border_1().border_color(theme.border_strong)
                    })
                    .when(!is_selected, |element| element.border_1().border_color(theme.border))
                    .hover(|style| style.bg(theme.overlay))
                    .child(
                        div()
                            .text_size(sp(12.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(team.name.clone())),
                    )
                    .child(
                        div()
                            .text_size(sp(11.0))
                            .text_color(theme.text_tertiary)
                            .child(SharedString::from(format!(
                                "{} · {}",
                                tr!("agents.member_count", count = team.members.len()),
                                team.scope.label()
                            ))),
                    )
                    .on_click({
                        let weak = weak.clone();
                        move |_, _, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.agents_view = AgentsView::Teams;
                                this.agents_selected_team = Some(id.clone());
                                cx.notify();
                            });
                        }
                    }),
            );
        }
        column.into_any_element()
    }

    fn render_agents_identity_detail(&self, theme: &Theme) -> AnyElement {
        let Some(definition) = self.selected_agent_definition() else {
            return self.render_agents_empty(theme);
        };
        let mut rows = div()
            .id("agents-identity-detail")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(10.0));
        rows = rows
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(sp(15.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(SharedString::from(definition.name.clone())),
                    )
                    .child(
                        div()
                            .px(px(6.0))
                            .py(px(1.0))
                            .rounded(px(5.0))
                            .bg(theme.overlay)
                            .text_size(sp(11.0))
                            .text_color(theme.text_tertiary)
                            .child(SharedString::from(definition.role.clone())),
                    )
                    .child(
                        div()
                            .text_size(sp(11.0))
                            .text_color(theme.text_ghost)
                            .child(SharedString::from(definition.scope.label())),
                    ),
            )
            .child(
                div()
                    .text_size(sp(12.5))
                    .line_height(sp(19.0))
                    .text_color(theme.text_secondary)
                    .child(SharedString::from(definition.description.clone())),
            );
        for (label_key, body) in [
            ("agents.instructions", definition.instructions.clone()),
            ("agents.boundaries", definition.boundaries.clone()),
        ] {
            if body.trim().is_empty() {
                continue;
            }
            rows = rows.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(sp(11.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text_tertiary)
                            .child(tr!(label_key)),
                    )
                    .child(
                        div()
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface)
                            .p(px(9.0))
                            .text_size(sp(12.0))
                            .line_height(sp(18.0))
                            .text_color(theme.text)
                            .child(SharedString::from(body)),
                    ),
            );
        }
        // What the identity changes about the process it starts, as opposed to
        // what it says. A reader deciding whether to trust an agent needs both.
        let mut runtime = Vec::new();
        if let Some(model) = definition.model.as_deref().filter(|it| !it.trim().is_empty()) {
            runtime.push((tr!("agents.model"), model.to_owned()));
        }
        if let Some(effort) = definition.effort.as_deref().filter(|it| !it.trim().is_empty()) {
            runtime.push((tr!("agents.effort"), effort.to_owned()));
        }
        if !definition.tools.is_empty() {
            runtime.push((tr!("agents.tools"), definition.tools.join(", ")));
        }
        if !runtime.is_empty() {
            let mut spec = div().flex().flex_col().gap(px(4.0));
            for (label, value) in runtime {
                spec = spec.child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .child(
                            div()
                                .flex_none()
                                .w(px(80.0))
                                .text_size(sp(11.5))
                                .text_color(theme.text_tertiary)
                                .child(SharedString::from(label)),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .text_size(sp(12.0))
                                .text_color(theme.text)
                                .child(SharedString::from(value)),
                        ),
                );
            }
            rows = rows.child(spec);
        }
        rows.into_any_element()
    }

    fn render_agents_team_detail(
        &self,
        catalog: &waku_client::AgentsCatalog,
        theme: &Theme,
    ) -> AnyElement {
        let Some(team) = self.selected_agent_team() else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(sp(12.5))
                .text_color(theme.text_tertiary)
                .child(tr!("agents.no_teams"))
                .into_any_element();
        };
        let mut rows = div()
            .id("agents-team-detail")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(sp(15.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(SharedString::from(team.name.clone())),
                    )
                    .child(
                        div()
                            .text_size(sp(11.0))
                            .text_color(theme.text_ghost)
                            .child(SharedString::from(team.scope.label())),
                    ),
            );
        if !team.description.trim().is_empty() {
            rows = rows.child(
                div()
                    .text_size(sp(12.5))
                    .text_color(theme.text_secondary)
                    .child(SharedString::from(team.description.clone())),
            );
        }
        if !team.brief.trim().is_empty() {
            rows = rows.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(sp(11.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text_tertiary)
                            .child(tr!("agents.brief")),
                    )
                    .child(
                        div()
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface)
                            .p(px(9.0))
                            .text_size(sp(12.0))
                            .line_height(sp(18.0))
                            .text_color(theme.text)
                            .child(SharedString::from(team.brief.clone())),
                    ),
            );
        }
        // Members are listed by the identity each one resolves to, so a member
        // id that no longer exists is visible as such instead of silently
        // disappearing from the team.
        let mut members = div().flex().flex_col().gap(px(3.0));
        for member in &team.members {
            let name = catalog
                .find(member)
                .map(|definition| definition.roster_line())
                .unwrap_or_else(|| format!("{} (missing)", member));
            members = members.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(icon("icons/users.svg", 12.0, theme.text_tertiary))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .text_size(sp(12.0))
                            .text_color(theme.text)
                            .child(SharedString::from(name)),
                    ),
            );
        }
        rows.child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div()
                        .text_size(sp(11.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text_tertiary)
                        .child(tr!("agents.members")),
                )
                .child(members),
        )
        .into_any_element()
    }

    fn render_agents_problems(
        &self,
        catalog: &waku_client::AgentsCatalog,
        theme: &Theme,
    ) -> impl IntoElement {
        let mut column = div()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .p(px(9.0))
            .child(
                div()
                    .text_size(sp(11.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text_secondary)
                    .child(tr!("agents.problems")),
            );
        for problem in catalog.problems.iter().take(4) {
            column = column.child(
                div()
                    .text_size(sp(11.0))
                    .text_color(theme.text_tertiary)
                    .child(SharedString::from(format!(
                        "{}: {}",
                        problem.path.display(),
                        problem.message
                    ))),
            );
        }
        column
    }

    fn render_agents_empty(&self, theme: &Theme) -> AnyElement {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(sp(13.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(tr!("agents.none")),
            )
            .child(
                div()
                    .text_size(sp(12.0))
                    .text_color(theme.text_tertiary)
                    .child(tr!("agents.none_description")),
            )
            .into_any_element()
    }
}



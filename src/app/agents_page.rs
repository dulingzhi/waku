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

/// One identity being edited, held as text fields rather than as a definition.
///
/// Empty is a legal value for every field while typing, which a definition
/// cannot express: `name` and `role` are required at save time and the draft is
/// where "not filled in yet" lives.
pub(super) struct AgentDraft {
    /// The identity being edited, or `None` for a new one.
    pub(super) id: Option<String>,
    pub(super) scope: waku_client::AgentDefinitionScope,
    /// Every editable field, in the order the form shows them.
    pub(super) fields: Vec<(AgentDraftField, Entity<TextInput>)>,
}

impl AgentDraft {
    pub(super) fn field(&self, field: AgentDraftField) -> Option<&Entity<TextInput>> {
        self.fields
            .iter()
            .find(|(candidate, _)| *candidate == field)
            .map(|(_, input)| input)
    }
}

/// The editable parts of an identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AgentDraftField {
    Name,
    Role,
    Description,
    Instructions,
    Boundaries,
}

impl AgentDraftField {
    fn label_key(self) -> &'static str {
        match self {
            Self::Name => "agents.field_name",
            Self::Role => "agents.field_role",
            Self::Description => "agents.field_description",
            Self::Instructions => "agents.instructions",
            Self::Boundaries => "agents.boundaries",
        }
    }

    fn placeholder_key(self) -> &'static str {
        match self {
            Self::Name => "agents.field_name_placeholder",
            Self::Role => "agents.field_role_placeholder",
            Self::Description => "agents.field_description_placeholder",
            Self::Instructions => "agents.field_instructions_placeholder",
            Self::Boundaries => "agents.field_boundaries_placeholder",
        }
    }

    fn multi_line(self) -> bool {
        matches!(
            self,
            Self::Description | Self::Instructions | Self::Boundaries
        )
    }

    fn value(self, definition: &waku_client::AgentDefinition) -> &str {
        match self {
            Self::Name => &definition.name,
            Self::Role => &definition.role,
            Self::Description => &definition.description,
            Self::Instructions => &definition.instructions,
            Self::Boundaries => &definition.boundaries,
        }
    }
}

impl Waku {
    // ── Catalog ────────────────────────────────────────────────────────────

    /// Opens the detail pane's editor for one identity, or for a new one.
    ///
    /// A built-in is copied rather than edited: the app ships that definition,
    /// so saving over it would be overwritten by the next build. The copy gets a
    /// fresh id so it can live beside the original, and `copied_from` keeps the
    /// pair recognisable.
    pub(super) fn begin_agent_edit(
        &mut self,
        definition: Option<&waku_client::AgentDefinition>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (id, scope, source) = match definition {
            Some(definition) if definition.scope.is_editable() => (
                Some(definition.id.clone()),
                definition.scope,
                definition.clone(),
            ),
            Some(definition) => {
                let mut copy = definition.clone();
                copy.id = format!(
                    "{}-copy-{}",
                    definition.id,
                    &Uuid::new_v4().to_string()[..6]
                );
                copy.name = format!("{} copy", definition.name);
                copy.copied_from = Some(definition.id.clone());
                // A copy is the user's, not the app's.
                copy.scope = waku_client::AgentDefinitionScope::User;
                (None, waku_client::AgentDefinitionScope::User, copy)
            }
            None => (
                None,
                waku_client::AgentDefinitionScope::User,
                waku_client::AgentDefinition {
                    id: String::new(),
                    name: String::new(),
                    role: String::new(),
                    description: String::new(),
                    instructions: String::new(),
                    boundaries: String::new(),
                    tools: Vec::new(),
                    model: None,
                    effort: None,
                    color: None,
                    scope: waku_client::AgentDefinitionScope::User,
                    copied_from: None,
                },
            ),
        };
        // Every field is built here rather than in the render path: creating a
        // text input needs a window, and a render must not spawn entities.
        let fields = [
            AgentDraftField::Name,
            AgentDraftField::Role,
            AgentDraftField::Description,
            AgentDraftField::Instructions,
            AgentDraftField::Boundaries,
        ]
        .into_iter()
        .map(|field| {
            let value = field.value(&source).to_owned();
            let placeholder = tr!(field.placeholder_key());
            let input = cx.new(|cx| {
                let input = TextInput::new(window, cx).placeholder(placeholder);
                if field.multi_line() {
                    input.multi_line()
                } else {
                    input
                }
            });
            if !value.is_empty() {
                input.update(cx, |input, cx| input.set_content(value, cx));
            }
            (field, input)
        })
        .collect();
        self.agents_draft = Some(AgentDraft { id, scope, fields });
        cx.notify();
    }

    pub(super) fn cancel_agent_edit(&mut self, cx: &mut Context<Self>) {
        self.agents_draft = None;
        cx.notify();
    }

    /// Writes the draft as an identity.
    ///
    /// A name or role left empty is refused rather than saved: both are how a
    /// lead recognises the agent in the roster, and an identity nobody can
    /// recognise is one nobody will dispatch to.
    pub(super) fn save_agent_draft(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.agents_draft.as_ref() else {
            return;
        };
        let value = |field: AgentDraftField| {
            draft
                .field(field)
                .map(|input| input.read(cx).content().trim().to_owned())
                .unwrap_or_default()
        };
        let name = value(AgentDraftField::Name);
        let role = value(AgentDraftField::Role);
        if name.is_empty() || role.is_empty() {
            self.show_toast(tr!("agents.name_and_role_required"));
            return;
        }
        // The id is derived from the name for a new identity, so the roster line
        // a lead reads and the id it passes to spawn agree. An edited identity
        // keeps its id: renaming must not orphan its past tasks.
        let id = draft.id.clone().unwrap_or_else(|| {
            let slug = name
                .to_lowercase()
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '-' })
                .collect::<String>();
            let slug = slug.trim_matches('-').to_owned();
            if slug.is_empty() {
                Uuid::new_v4().to_string()
            } else {
                slug
            }
        });
        let scope = draft.scope;
        let mut definition = self
            .agents_catalog
            .as_deref()
            .and_then(|catalog| catalog.find(&id))
            .cloned()
            .unwrap_or_else(|| waku_client::AgentDefinition {
                id: id.clone(),
                name: String::new(),
                role: String::new(),
                description: String::new(),
                instructions: String::new(),
                boundaries: String::new(),
                tools: Vec::new(),
                model: None,
                effort: None,
                color: None,
                scope,
                copied_from: None,
            });
        definition.id = id.clone();
        definition.scope = scope;
        definition.name = name;
        definition.role = role;
        definition.description = value(AgentDraftField::Description);
        definition.instructions = value(AgentDraftField::Instructions);
        definition.boundaries = value(AgentDraftField::Boundaries);
        self.agents_selected = Some(id);
        self.agents_draft = None;
        self.save_agent_definition(definition, cx);
    }

    /// Creates a team from every identity currently defined.
    /// A minimal way to get a reusable group without a second editor: the
    /// identities already describe the roles, so the team is just their ordered
    /// list plus a name. A user who wants a narrower group edits the file, which
    /// is the point of keeping teams as files.
    pub(super) fn create_team_from_identities(&mut self, cx: &mut Context<Self>) {
        let Some(catalog) = self.agents_catalog.as_deref() else {
            return;
        };
        let members = catalog
            .definitions
            .iter()
            .map(|definition| definition.id.clone())
            .collect::<Vec<_>>();
        if members.is_empty() {
            self.show_toast(tr!("agents.team_needs_identities"));
            return;
        }
        let name = format!("{} {}", tr!("agents.default_team_name"), members.len());
        let id = format!("team-{}", &Uuid::new_v4().to_string()[..8]);
        let team = waku_client::AgentTeam {
            id: id.clone(),
            name,
            description: String::new(),
            brief: String::new(),
            members,
            scope: waku_client::AgentDefinitionScope::User,
            copied_from: None,
        };
        self.agents_view = AgentsView::Teams;
        self.agents_selected_team = Some(id);
        self.save_agent_team(team, cx);
    }

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
            AgentsView::Identities => {
                self.render_agents_identity_list(&catalog, &theme, weak.clone())
            }
            AgentsView::Teams => self.render_agents_team_list(&catalog, &theme, weak.clone()),
        };
        let detail = match self.agents_view {
            AgentsView::Identities => self.render_agents_identity_detail(&theme, weak.clone()),
            AgentsView::Teams => self.render_agents_team_detail(&catalog, &theme, weak.clone()),
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
                    .when(!selected, |element| element.text_color(theme.text_tertiary))
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
                        element
                            .bg(theme.overlay)
                            .border_1()
                            .border_color(theme.border_strong)
                    })
                    .when(!is_selected, |element| {
                        element.border_1().border_color(theme.border)
                    })
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
                .child(
                    div()
                        .text_size(sp(11.5))
                        .text_color(theme.text_tertiary)
                        .child(tr!("agents.no_teams_description")),
                )
                .child(
                    div()
                        .id("agents-create-team")
                        .px(px(9.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .cursor_default()
                        .border_1()
                        .border_color(theme.border)
                        .text_size(sp(11.5))
                        .text_color(theme.text_secondary)
                        .hover(|style| style.bg(theme.overlay))
                        .child(tr!("agents.create_team"))
                        .on_click(move |_, _, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.create_team_from_identities(cx);
                            });
                        }),
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
            .overflow_y_scroll()
            // A team is a saved group, and the identities already describe the
            // roles; this builds one from all of them rather than making the
            // user retype what is on the other tab.
            .child(
                div()
                    .id("agents-create-team")
                    .mb(px(4.0))
                    .px(px(9.0))
                    .py(px(4.0))
                    .rounded(px(6.0))
                    .cursor_default()
                    .border_1()
                    .border_color(theme.border)
                    .text_size(sp(11.5))
                    .text_color(theme.text_secondary)
                    .hover(|style| style.bg(theme.overlay))
                    .child(tr!("agents.create_team"))
                    .on_click({
                        let weak = weak.clone();
                        move |_, _, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.create_team_from_identities(cx);
                            });
                        }
                    }),
            );
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
                        element
                            .bg(theme.overlay)
                            .border_1()
                            .border_color(theme.border_strong)
                    })
                    .when(!is_selected, |element| {
                        element.border_1().border_color(theme.border)
                    })
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

    fn render_agents_identity_detail(&self, theme: &Theme, weak: WeakEntity<Waku>) -> AnyElement {
        if let Some(draft) = self.agents_draft.as_ref() {
            return self.render_agent_draft(draft, theme, weak);
        }
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
        // A built-in can be copied but not edited: the app ships it, and the
        // next build would replace an edit anyway. Saying so is what keeps a
        // saved copy from looking like a failed save.
        if !definition.scope.is_editable() {
            rows = rows.child(
                div()
                    .text_size(sp(11.0))
                    .text_color(theme.text_ghost)
                    .child(tr!("agents.builtin_read_only")),
            );
        }
        let edit_definition = definition.clone();
        rows = rows.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .id("agents-edit-identity")
                        .px(px(9.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .cursor_default()
                        .border_1()
                        .border_color(theme.border)
                        .text_size(sp(11.5))
                        .text_color(theme.text)
                        .hover(|style| style.bg(theme.overlay))
                        .child(tr!(if definition.scope.is_editable() {
                            "agents.edit"
                        } else {
                            "agents.copy_to_edit"
                        }))
                        .on_click({
                            let weak = weak.clone();
                            let edit_definition = edit_definition.clone();
                            move |_, window, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    this.begin_agent_edit(Some(&edit_definition), window, cx);
                                });
                            }
                        }),
                )
                .child(
                    div()
                        .id("agents-new-identity")
                        .px(px(9.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .cursor_default()
                        .border_1()
                        .border_color(theme.border)
                        .text_size(sp(11.5))
                        .text_color(theme.text_secondary)
                        .hover(|style| style.bg(theme.overlay))
                        .child(tr!("agents.new"))
                        .on_click({
                            let weak = weak.clone();
                            move |_, window, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    this.begin_agent_edit(None, window, cx);
                                });
                            }
                        }),
                )
                // Deleting is offered only for a stored identity: a built-in has
                // no file to remove, and the button would be a lie.
                .when(definition.scope.is_editable(), |row| {
                    let scope = definition.scope;
                    let id = definition.id.clone();
                    row.child(
                        div()
                            .id("agents-delete-identity")
                            .px(px(9.0))
                            .py(px(4.0))
                            .rounded(px(6.0))
                            .cursor_default()
                            .border_1()
                            .border_color(theme.border)
                            .text_size(sp(11.5))
                            .text_color(theme.text_tertiary)
                            .hover(|style| style.bg(theme.overlay))
                            .child(tr!("agents.delete"))
                            .on_click({
                                let weak = weak.clone();
                                move |_, _, cx| {
                                    let _ = weak.update(cx, |this, cx| {
                                        this.delete_agent_definition(scope, id.clone(), cx);
                                    });
                                }
                            }),
                    )
                }),
        );
        // What the identity changes about the process it starts, as opposed to
        // what it says. A reader deciding whether to trust an agent needs both.
        let mut runtime = Vec::new();
        if let Some(model) = definition
            .model
            .as_deref()
            .filter(|it| !it.trim().is_empty())
        {
            runtime.push((tr!("agents.model"), model.to_owned()));
        }
        if let Some(effort) = definition
            .effort
            .as_deref()
            .filter(|it| !it.trim().is_empty())
        {
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

    /// The identity form: one text field per editable part, plus save and
    /// cancel.
    ///
    /// The fields are entities created when editing began, not on this path: a
    /// render must not spawn entities, and rebuilding them per frame would drop
    /// the cursor on every keystroke.
    fn render_agent_draft(
        &self,
        draft: &AgentDraft,
        theme: &Theme,
        weak: WeakEntity<Waku>,
    ) -> AnyElement {
        let mut rows = div()
            .id("agents-identity-draft")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(9.0))
            .child(
                div()
                    .text_size(sp(13.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(tr!(if draft.id.is_some() {
                        "agents.edit_identity"
                    } else {
                        "agents.new_identity"
                    })),
            );
        for (field, input) in &draft.fields {
            rows = rows.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .text_size(sp(11.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text_tertiary)
                            .child(tr!(field.label_key())),
                    )
                    .child(TextField::new("agents-draft-field", input.clone()).w_full()),
            );
        }
        rows.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .id("agents-save-identity")
                        .px(px(10.0))
                        .py(px(5.0))
                        .rounded(px(6.0))
                        .cursor_default()
                        .bg(theme.overlay)
                        .text_size(sp(11.5))
                        .text_color(theme.text)
                        .child(tr!("common.save"))
                        .on_click({
                            let weak = weak.clone();
                            move |_, _, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    this.save_agent_draft(cx);
                                });
                            }
                        }),
                )
                .child(
                    div()
                        .id("agents-cancel-identity")
                        .px(px(10.0))
                        .py(px(5.0))
                        .rounded(px(6.0))
                        .cursor_default()
                        .border_1()
                        .border_color(theme.border)
                        .text_size(sp(11.5))
                        .text_color(theme.text_secondary)
                        .hover(|style| style.bg(theme.overlay))
                        .child(tr!("common.cancel"))
                        .on_click({
                            let weak = weak.clone();
                            move |_, _, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    this.cancel_agent_edit(cx);
                                });
                            }
                        }),
                ),
        )
        .into_any_element()
    }

    fn render_agents_team_detail(
        &self,
        catalog: &waku_client::AgentsCatalog,
        theme: &Theme,
        weak: WeakEntity<Waku>,
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
                .child(members)
                .child(
                    div()
                        .id("agents-delete-team")
                        .mt(px(4.0))
                        .px(px(9.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .cursor_default()
                        .border_1()
                        .border_color(theme.border)
                        .text_size(sp(11.5))
                        .text_color(theme.text_tertiary)
                        .hover(|style| style.bg(theme.overlay))
                        .child(tr!("agents.delete"))
                        .on_click({
                            let scope = team.scope;
                            let id = team.id.clone();
                            move |_, _, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    this.delete_agent_team(scope, id.clone(), cx);
                                });
                            }
                        }),
                ),
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

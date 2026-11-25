//! Identities and team templates for the delegates a conversation spawns.
//!
//! A delegate is an ordinary Waku task, but an anonymous one: it inherits the
//! lead's provider and model and starts from the task text alone. Real work
//! wants more than that — a reviewer that reviews and does not edit, an
//! implementer with a bounded tool set, a researcher that reports instead of
//! changing files. An identity is that set of things, written once and reused.
//!
//! Identities are files, not database rows, for the same reason skills are:
//! they are worth reviewing, diffing, and sharing with a team through the
//! project's own repository. A user identity lives under `~/.waku/agents`, a
//! project one under `<project>/.waku/agents`, and the project copy wins. Team
//! templates name an ordered list of identities plus the context they share, so
//! a whole group can be set up once and reused in the next conversation.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Where an identity or team was found. The project scope wins over the user
/// scope on a name collision, which is what lets a repository pin its own
/// version of a shared identity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum AgentDefinitionScope {
    /// Shipped with the app. Read-only; copying one is how a user edits it.
    Builtin,
    User,
    Project,
}

impl AgentDefinitionScope {
    pub fn label(self) -> String {
        match self {
            Self::Builtin => tr!("agents.scope_builtin"),
            Self::User => tr!("agents.scope_user"),
            Self::Project => tr!("agents.scope_project"),
        }
    }

    /// Whether a stored definition in this scope may be edited in place.
    ///
    /// A built-in identity is part of the app, so it can be copied but never
    /// rewritten: the next build would overwrite the edit anyway, and a
    /// built-in that silently differs per user is not a default.
    pub fn is_editable(self) -> bool {
        !matches!(self, Self::Builtin)
    }
}

/// One named identity a delegate can be started as.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AgentDefinition {
    /// Stable id the team templates and spawned tasks refer to. A definition
    /// copied from a built-in keeps the original id in `copied_from` so the
    /// pair stays recognisable.
    pub id: String,
    pub name: String,
    /// Two or three characters shown on cards and tabs, the way a bot's role
    /// label works: enough to tell identities apart at a glance.
    pub role: String,
    /// One sentence saying what this agent is for. This is what the lead reads
    /// when it decides whom to send a task to, so it states the job and not
    /// the personality.
    pub description: String,
    /// The identity itself: how to work, what to prioritise, what to hand
    /// back. Appended to the delegate's system prompt.
    #[serde(default)]
    pub instructions: String,
    /// What this agent must not do. Kept apart from `instructions` because it
    /// is appended last and stated as a prohibition, which models follow more
    /// reliably than a limit buried in a paragraph of guidance.
    #[serde(default)]
    pub boundaries: String,
    /// Tool names the agent may use, empty meaning "whatever the provider
    /// allows". Names are provider-native where the provider has a tool gate.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Overrides the lead's model when set, so a cheap read-only reviewer and
    /// an expensive implementer can run side by side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Overrides the lead's thinking level. Only the levels the chosen model
    /// serves are meaningful; an unsupported value is dropped when the process
    /// starts rather than failing the spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub scope: AgentDefinitionScope,
    /// The built-in this one was copied from, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copied_from: Option<String>,
}

impl AgentDefinition {
    /// The text appended to a delegate's system prompt for this identity.
    ///
    /// Boundaries come last and are stated as rules, because that is the part
    /// a reader has to be unable to miss.
    pub fn system_prompt(&self) -> String {
        let mut prompt = String::new();
        let name = self.name.trim();
        let role = self.role.trim();
        match (name.is_empty(), role.is_empty()) {
            (false, false) => prompt.push_str(&format!("You are {name}, the {role}.\n")),
            (false, true) => prompt.push_str(&format!("You are {name}.\n")),
            (true, false) => prompt.push_str(&format!("You are the {role}.\n")),
            (true, true) => {}
        }
        let instructions = self.instructions.trim();
        if !instructions.is_empty() {
            prompt.push('\n');
            prompt.push_str(instructions);
            prompt.push('\n');
        }
        let boundaries = self.boundaries.trim();
        if !boundaries.is_empty() {
            prompt.push_str("\nBoundaries you must not cross:\n");
            prompt.push_str(boundaries);
            prompt.push('\n');
        }
        prompt
    }

    /// The single line a lead reads when choosing whom to dispatch to.
    pub fn roster_line(&self) -> String {
        let name = self.name.trim();
        let role = self.role.trim();
        let description = self.description.trim();
        let label = match (name.is_empty(), role.is_empty()) {
            (false, false) => format!("{name} ({role})"),
            (false, true) => name.to_owned(),
            (true, false) => role.to_owned(),
            (true, true) => self.id.clone(),
        };
        if description.is_empty() {
            label
        } else {
            format!("{label}: {description}")
        }
    }
}

/// A saved group of identities, with the context they all share.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AgentTeam {
    pub id: String,
    pub name: String,
    /// One line saying when this team is the right one to reach for.
    #[serde(default)]
    pub description: String,
    /// Shared context every member of the team is told: the project's shape,
    /// the conventions in force, what the group is building. Written once here
    /// rather than repeated in each identity's instructions.
    #[serde(default)]
    pub brief: String,
    /// The identities to start, in the order they are listed to the lead.
    #[serde(default)]
    pub members: Vec<String>,
    pub scope: AgentDefinitionScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copied_from: Option<String>,
}

/// Every identity and team a daemon can see, with the roots they came from.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AgentsCatalog {
    pub definitions: Vec<AgentDefinition>,
    pub teams: Vec<AgentTeam>,
    /// The roots searched, so the settings page can say where a file lives and
    /// a user can open it in their editor.
    #[ts(type = "string[]")]
    pub roots: Vec<PathBuf>,
    /// Files that exist but could not be read or parsed. Shown rather than
    /// silently dropped: a broken identity the user wrote has to be findable.
    #[serde(default)]
    pub problems: Vec<AgentCatalogProblem>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AgentCatalogProblem {
    #[ts(type = "string")]
    pub path: PathBuf,
    pub message: String,
}

impl AgentsCatalog {
    pub fn find(&self, id: &str) -> Option<&AgentDefinition> {
        self.definitions
            .iter()
            .find(|definition| definition.id == id)
    }

    pub fn find_team(&self, id: &str) -> Option<&AgentTeam> {
        self.teams.iter().find(|team| team.id == id)
    }

    /// The roster text a lead gets, so dispatch is decided from the same
    /// identities the UI shows.
    ///
    /// Empty when nothing is defined: a lead told about an empty roster would
    /// be told it can delegate to nobody, which is true but not worth saying.
    pub fn roster(&self) -> String {
        if self.definitions.is_empty() {
            return String::new();
        }
        let mut roster = String::from(
            "Agents you can start with `spawn_agent`, each of which runs as its own \
             Waku task and works independently:\n",
        );
        for definition in &self.definitions {
            roster.push_str("- ");
            roster.push_str(&definition.roster_line());
            roster.push_str(" [id: ");
            roster.push_str(&definition.id);
            roster.push_str("]\n");
        }
        if !self.teams.is_empty() {
            roster.push_str("\nTeams, when a whole group fits the job better:\n");
            for team in &self.teams {
                roster.push_str("- ");
                roster.push_str(team.name.trim());
                let description = team.description.trim();
                if !description.is_empty() {
                    roster.push_str(": ");
                    roster.push_str(description);
                }
                roster.push_str(" [id: ");
                roster.push_str(&team.id);
                roster.push_str("]\n");
            }
        }
        roster.push_str(
            "\nDispatch a task only when it is separable from your own work; do the \
             work yourself when it is not, or when one agent would spend longer \
             explaining it than doing it.",
        );
        roster
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(name: &str, role: &str, description: &str) -> AgentDefinition {
        AgentDefinition {
            id: name.to_lowercase(),
            name: name.into(),
            role: role.into(),
            description: description.into(),
            instructions: String::new(),
            boundaries: String::new(),
            tools: Vec::new(),
            model: None,
            effort: None,
            color: None,
            scope: AgentDefinitionScope::User,
            copied_from: None,
        }
    }

    /// A lead dispatches from the roster, so a definition with no description
    /// still has to produce a usable line: an unnamed job is a job it cannot
    /// judge.
    #[test]
    fn a_roster_line_names_the_agent_and_its_job() {
        let auditor = definition("Auditor", "RV", "Reviews changes and reports risks.");
        assert_eq!(
            auditor.roster_line(),
            "Auditor (RV): Reviews changes and reports risks."
        );
    }

    /// The id is what the lead passes to `spawn_agent`, so it must appear on
    /// the line even when the agent has no description to help identify it.
    #[test]
    fn the_roster_carries_the_id_a_spawn_has_to_name() {
        let catalog = AgentsCatalog {
            definitions: vec![definition("Auditor", "RV", "")],
            ..AgentsCatalog::default()
        };
        let roster = catalog.roster();
        assert!(
            roster.contains("[id: auditor]"),
            "the lead cannot spawn without the id: {roster}"
        );
    }

    /// An identity with no definitions has nothing to say; printing an empty
    /// roster would tell a lead it may delegate to nobody.
    #[test]
    fn an_empty_catalog_has_no_roster() {
        assert!(AgentsCatalog::default().roster().is_empty());
    }

    /// Boundaries are the part a delegate must not miss, so they are stated
    /// last and as their own section rather than folded into the guidance.
    #[test]
    fn boundaries_are_stated_after_the_instructions() {
        let mut reviewer = definition("Reviewer", "RV", "");
        reviewer.instructions = "Read the diff and report.".into();
        reviewer.boundaries = "Never edit files.".into();
        let prompt = reviewer.system_prompt();
        assert!(prompt.starts_with("You are Reviewer, the RV."));
        let instructions = prompt.find("Read the diff").expect("instructions");
        let boundaries = prompt.find("Never edit files.").expect("boundaries");
        assert!(
            instructions < boundaries,
            "a limit buried above the task reads as optional: {prompt}"
        );
        assert!(prompt.contains("must not cross"));
    }

    /// A built-in is the app's, not the user's: it can be copied but the copy
    /// is what gets edited.
    #[test]
    fn only_stored_scopes_are_editable() {
        assert!(!AgentDefinitionScope::Builtin.is_editable());
        assert!(AgentDefinitionScope::User.is_editable());
        assert!(AgentDefinitionScope::Project.is_editable());
    }
}

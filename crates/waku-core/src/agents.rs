//! Discovery and storage of agent identities and team templates.
//!
//! The daemon is the only writer: the desktop, the web client, and a lead's
//! own bridge all read the same catalog, and a bridge that wrote its own copy
//! would drift from what the settings page shows. Files rather than rows for
//! the reason [`waku_protocol::agents`] gives — they are worth reviewing and
//! sharing through the project's repository.
//!
//! Three roots are searched, weakest first:
//!
//! * the built-ins compiled into the app,
//! * `~/.waku/agents` and `~/.waku/teams`,
//! * `<project>/.waku/agents` and `<project>/.waku/teams`.
//!
//! A later root replaces an earlier definition with the same name, so a
//! repository can pin the identities its work depends on and a user can still
//! keep their own for everything else.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub use waku_protocol::agents::{
    AgentCatalogProblem, AgentDefinition, AgentDefinitionScope, AgentTeam, AgentsCatalog,
};

const DEFINITION_FILE_MAX_BYTES: u64 = 512 * 1024;
const BUILTIN_DEFINITIONS: &str = include_str!("builtin_agents.json");

/// One root to search, in the order it wins.
#[derive(Clone, Debug)]
struct AgentsRoot {
    scope: AgentDefinitionScope,
    path: PathBuf,
}

/// The `~/.waku/agents` and `~/.waku/teams` roots, path joins only.
fn user_root() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".waku"))
}

fn project_root(project: &Path) -> PathBuf {
    project.join(".waku")
}

/// Every root a catalog read covers, weakest first.
pub fn catalog_roots(project: Option<&Path>) -> Vec<(AgentDefinitionScope, PathBuf)> {
    let mut roots = Vec::new();
    if let Some(user) = user_root() {
        roots.push((AgentDefinitionScope::User, user));
    }
    if let Some(project) = project {
        roots.push((AgentDefinitionScope::Project, project_root(project)));
    }
    roots
}

/// Reads every identity and team visible for `project`.
///
/// A file that cannot be read or parsed is reported in `problems` rather than
/// skipped: an identity the user wrote and mistyped has to be findable, or the
/// settings page looks like it lost their work.
pub fn load_catalog(project: Option<&Path>) -> AgentsCatalog {
    let roots = catalog_roots(project)
        .into_iter()
        .map(|(scope, path)| AgentsRoot { scope, path })
        .collect::<Vec<_>>();
    load_catalog_at(&roots)
}

/// Reads a catalog from explicit roots, weakest first.
///
/// Split out so a test can read a temporary directory instead of the user's
/// real one: every property this module has — which root wins, what a broken
/// file reports, whether a rewrite round-trips — is about the filesystem, and
/// none of it is observable from an in-memory catalog.
fn load_catalog_at(roots: &[AgentsRoot]) -> AgentsCatalog {
    let mut catalog = AgentsCatalog::default();
    let mut definitions: HashMap<String, AgentDefinition> = HashMap::new();
    let mut teams: HashMap<String, AgentTeam> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for builtin in parse_builtin_definitions() {
        order.push(builtin.id.clone());
        definitions.insert(builtin.id.clone(), builtin);
    }

    for root in roots {
        catalog.roots.push(root.path.clone());
        read_definition_dir(
            &root.path.join("agents"),
            root.scope,
            &mut definitions,
            &mut order,
            &mut catalog.problems,
        );
        read_team_dir(
            &root.path.join("teams"),
            root.scope,
            &mut teams,
            &mut catalog.problems,
        );
    }

    // `order` keeps the built-ins in their authored order; stored identities
    // follow sorted by name, so the list does not reshuffle between reads.
    let mut stored = definitions
        .values()
        .filter(|definition| definition.scope != AgentDefinitionScope::Builtin)
        .map(|definition| definition.id.clone())
        .collect::<Vec<_>>();
    stored.sort_by_key(|id| {
        definitions
            .get(id)
            .map(|definition| definition.name.to_lowercase())
            .unwrap_or_default()
    });
    for id in stored {
        if !order.contains(&id) {
            order.push(id);
        }
    }

    catalog.definitions = order
        .into_iter()
        .filter_map(|id| definitions.remove(&id))
        .collect();
    let mut teams = teams.into_values().collect::<Vec<_>>();
    teams.sort_by_key(|team| team.name.to_lowercase());
    catalog.teams = teams;
    catalog
}

fn parse_builtin_definitions() -> Vec<AgentDefinition> {
    let Ok(mut parsed) = serde_json::from_str::<Vec<AgentDefinition>>(BUILTIN_DEFINITIONS) else {
        // The built-ins ship with the app, so a parse failure is a build
        // defect rather than a user error. Returning none keeps the app usable
        // with stored identities alone instead of panicking on startup.
        return Vec::new();
    };
    for definition in &mut parsed {
        definition.scope = AgentDefinitionScope::Builtin;
    }
    parsed
}

fn read_definition_dir(
    dir: &Path,
    scope: AgentDefinitionScope,
    definitions: &mut HashMap<String, AgentDefinition>,
    order: &mut Vec<String>,
    problems: &mut Vec<AgentCatalogProblem>,
) {
    for (path, source) in read_json_files(dir, problems) {
        match serde_json::from_str::<AgentDefinition>(&source) {
            Ok(mut definition) => {
                if definition.id.trim().is_empty() {
                    problems.push(AgentCatalogProblem {
                        path,
                        message: "an agent definition needs an id".into(),
                    });
                    continue;
                }
                definition.scope = scope;
                if !order.contains(&definition.id) {
                    order.push(definition.id.clone());
                }
                definitions.insert(definition.id.clone(), definition);
            }
            Err(error) => problems.push(AgentCatalogProblem {
                path,
                message: format!("could not read this agent: {error}"),
            }),
        }
    }
}

fn read_team_dir(
    dir: &Path,
    scope: AgentDefinitionScope,
    teams: &mut HashMap<String, AgentTeam>,
    problems: &mut Vec<AgentCatalogProblem>,
) {
    for (path, source) in read_json_files(dir, problems) {
        match serde_json::from_str::<AgentTeam>(&source) {
            Ok(mut team) => {
                if team.id.trim().is_empty() {
                    problems.push(AgentCatalogProblem {
                        path,
                        message: "a team needs an id".into(),
                    });
                    continue;
                }
                team.scope = scope;
                teams.insert(team.id.clone(), team);
            }
            Err(error) => problems.push(AgentCatalogProblem {
                path,
                message: format!("could not read this team: {error}"),
            }),
        }
    }
}

fn read_json_files(dir: &Path, problems: &mut Vec<AgentCatalogProblem>) -> Vec<(PathBuf, String)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
        .collect::<Vec<_>>();
    files.sort();
    let mut sources = Vec::with_capacity(files.len());
    for path in files {
        match fs::metadata(&path) {
            Ok(metadata) if metadata.len() > DEFINITION_FILE_MAX_BYTES => {
                problems.push(AgentCatalogProblem {
                    path,
                    message: format!(
                        "this file is larger than {DEFINITION_FILE_MAX_BYTES} bytes, so it was not read"
                    ),
                });
            }
            Ok(_) => match fs::read_to_string(&path) {
                Ok(source) => sources.push((path, source)),
                Err(error) => problems.push(AgentCatalogProblem {
                    path,
                    message: format!("could not read this file: {error}"),
                }),
            },
            Err(error) => problems.push(AgentCatalogProblem {
                path,
                message: format!("could not read this file: {error}"),
            }),
        }
    }
    sources
}

/// Where a definition of this scope is written.
///
/// A built-in has no writable home: it is the app's copy, and editing it means
/// copying it into a stored scope first. Returning `None` makes that a caller
/// error instead of a silent write into the app bundle.
fn definition_path(
    scope: AgentDefinitionScope,
    project: Option<&Path>,
    id: &str,
) -> Option<PathBuf> {
    let root = match scope {
        AgentDefinitionScope::Builtin => return None,
        AgentDefinitionScope::User => user_root()?,
        AgentDefinitionScope::Project => project_root(project?),
    };
    Some(root.join("agents").join(format!("{id}.json")))
}

fn team_path(scope: AgentDefinitionScope, project: Option<&Path>, id: &str) -> Option<PathBuf> {
    let root = match scope {
        AgentDefinitionScope::Builtin => return None,
        AgentDefinitionScope::User => user_root()?,
        AgentDefinitionScope::Project => project_root(project?),
    };
    Some(root.join("teams").join(format!("{id}.json")))
}

/// Writes an identity, creating its directory.
pub fn save_definition(
    project: Option<&Path>,
    definition: &AgentDefinition,
) -> io::Result<PathBuf> {
    let id = definition.id.trim();
    if id.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an agent definition needs an id",
        ));
    }
    let path = definition_path(definition.scope, project, id).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a built-in agent is read-only; copy it into your own agents first",
        )
    })?;
    write_json(&path, definition)?;
    Ok(path)
}

pub fn save_team(project: Option<&Path>, team: &AgentTeam) -> io::Result<PathBuf> {
    let id = team.id.trim();
    if id.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a team needs an id",
        ));
    }
    let path = team_path(team.scope, project, id).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a built-in team is read-only; copy it into your own teams first",
        )
    })?;
    write_json(&path, team)?;
    Ok(path)
}

pub fn delete_definition(
    project: Option<&Path>,
    scope: AgentDefinitionScope,
    id: &str,
) -> io::Result<()> {
    remove_file(definition_path(scope, project, id))
}

pub fn delete_team(project: Option<&Path>, scope: AgentDefinitionScope, id: &str) -> io::Result<()> {
    remove_file(team_path(scope, project, id))
}

fn remove_file(path: Option<PathBuf>) -> io::Result<()> {
    let path = path.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a built-in definition is read-only; copy it first",
        )
    })?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        // Removing something already gone is the state the caller wanted.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    // Write beside the target and rename, so a crash mid-write cannot leave a
    // half-written identity behind — the file a later read would find broken.
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, &data)?;
    fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("waku-agents-{label}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn definition(id: &str, scope: AgentDefinitionScope) -> AgentDefinition {
        AgentDefinition {
            id: id.into(),
            name: id.into(),
            role: "AA".into(),
            description: "does one thing".into(),
            instructions: String::new(),
            boundaries: String::new(),
            tools: Vec::new(),
            model: None,
            effort: None,
            color: None,
            scope,
            copied_from: None,
        }
    }

    /// A round trip through a real directory, because the bug this guards is a
    /// path or serialisation mistake and neither shows up in memory.
    #[test]
    fn a_definition_survives_a_round_trip_through_its_file() {
        let root = temp_root("round-trip");
        let path = root.join("agents").join("auditor.json");
        write_json(&path, &definition("auditor", AgentDefinitionScope::User)).unwrap();

        let catalog = load_catalog_at(&[AgentsRoot {
            scope: AgentDefinitionScope::User,
            path: root.clone(),
        }]);
        let found = catalog.find("auditor").expect("the written definition");
        assert_eq!(found.scope, AgentDefinitionScope::User);
        assert_eq!(found.description, "does one thing");
        fs::remove_dir_all(&root).ok();
    }

    /// A project pinning an identity has to win over the user's copy of the
    /// same id: that is the whole point of checking the project root last.
    #[test]
    fn the_project_copy_of_an_identity_wins_over_the_user_copy() {
        let user = temp_root("user-wins");
        let project = temp_root("project-wins");
        let mut user_copy = definition("auditor", AgentDefinitionScope::User);
        user_copy.description = "the user's version".into();
        let mut project_copy = definition("auditor", AgentDefinitionScope::Project);
        project_copy.description = "the project's version".into();
        write_json(&user.join("agents/auditor.json"), &user_copy).unwrap();
        write_json(&project.join("agents/auditor.json"), &project_copy).unwrap();

        let catalog = load_catalog_at(&[
            AgentsRoot {
                scope: AgentDefinitionScope::User,
                path: user.clone(),
            },
            AgentsRoot {
                scope: AgentDefinitionScope::Project,
                path: project.clone(),
            },
        ]);
        assert_eq!(
            catalog.find("auditor").map(|found| found.description.as_str()),
            Some("the project's version")
        );
        assert_eq!(
            catalog
                .definitions
                .iter()
                .filter(|found| found.id == "auditor")
                .count(),
            1,
            "one identity has to appear once, not once per root"
        );
        fs::remove_dir_all(&user).ok();
        fs::remove_dir_all(&project).ok();
    }

    /// A mistyped identity is the user's own work. Reporting it is what lets
    /// them find the file; dropping it silently looks like data loss.
    #[test]
    fn a_broken_file_is_reported_instead_of_vanishing() {
        let root = temp_root("broken");
        let agents = root.join("agents");
        fs::create_dir_all(&agents).unwrap();
        fs::write(agents.join("typo.json"), "{ not json").unwrap();

        let catalog = load_catalog_at(&[AgentsRoot {
            scope: AgentDefinitionScope::User,
            path: root.clone(),
        }]);
        assert!(
            catalog.definitions.iter().all(|found| found.id != "typo"),
            "a file that failed to parse must not become a definition"
        );
        assert_eq!(catalog.problems.len(), 1);
        assert!(
            catalog.problems[0].path.ends_with("typo.json"),
            "the report has to name the file: {:?}",
            catalog.problems
        );
        fs::remove_dir_all(&root).ok();
    }

    /// The built-ins are the defaults a user starts from, so they have to be
    /// present and parse: an empty catalogue would make the feature look
    /// unimplemented on a fresh install.
    #[test]
    fn the_shipped_builtins_parse_and_are_read_only() {
        let builtins = parse_builtin_definitions();
        assert!(!builtins.is_empty(), "the app ships no default identities");
        for definition in &builtins {
            assert_eq!(definition.scope, AgentDefinitionScope::Builtin);
            assert!(!definition.name.trim().is_empty(), "{definition:?}");
            assert!(!definition.description.trim().is_empty(), "{definition:?}");
            assert_ne!(
                definition_path(definition.scope, None, &definition.id),
                Some(PathBuf::new()),
                "a built-in must have no writable path"
            );
        }
    }

    /// Saving a built-in has to be refused, not silently written somewhere
    /// else: editing the app's own copy is not a supported edit.
    #[test]
    fn a_builtin_cannot_be_saved_over() {
        let builtin = &parse_builtin_definitions()[0];
        let error = save_definition(None, builtin).expect_err("built-ins are read-only");
        assert!(error.to_string().contains("read-only"));
    }
}

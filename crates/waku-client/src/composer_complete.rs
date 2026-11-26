//! Pure client-side composer matching over daemon-provided command/file lists.

use std::ops::Range;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};
pub use waku_protocol::composer::{CommandScope, FileEntry, SlashCommand};
use waku_protocol::model::{ProviderKind, ProviderModelOption, ReportedCommand};

pub const FILTER_CAP: usize = 64;
pub const FILE_INDEX_CAP: usize = 50_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TriggerKind {
    Command,
    File,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Trigger {
    pub kind: TriggerKind,
    pub query: String,
    pub range: Range<usize>,
}

pub fn detect_trigger(text: &str, cursor: usize) -> Option<Trigger> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let line_start = text[..cursor].rfind('\n').map_or(0, |index| index + 1);
    let line_prefix = &text[line_start..cursor];
    if let Some(query) = line_prefix.strip_prefix('/') {
        if !query.chars().any(char::is_whitespace) {
            return Some(Trigger {
                kind: TriggerKind::Command,
                query: query.to_owned(),
                range: line_start..cursor,
            });
        }
        return None;
    }
    let token_start = text[..cursor]
        .rfind(char::is_whitespace)
        .map_or(0, |index| {
            index + text[index..].chars().next().unwrap().len_utf8()
        });
    let token = &text[token_start..cursor];
    Some(Trigger {
        kind: TriggerKind::File,
        query: token.strip_prefix('@')?.to_owned(),
        range: token_start..cursor,
    })
}

pub fn merge_reported_commands(
    discovered: &[SlashCommand],
    reported: &[ReportedCommand],
) -> Vec<SlashCommand> {
    let mut merged = discovered.to_vec();
    for report in reported {
        if let Some(known) = merged
            .iter_mut()
            .find(|command| command.name == report.name)
        {
            if known.description.is_empty() {
                known.description = report.description.clone();
            }
        } else {
            merged.push(SlashCommand {
                name: report.name.clone(),
                description: report.description.clone(),
                scope: CommandScope::Builtin,
                argument_hint: None,
                template: None,
            });
        }
    }
    merged
        .sort_by(|a, b| (a.scope.display_rank(), &a.name).cmp(&(b.scope.display_rank(), &b.name)));
    merged
}

/// Build the slash-prefixed text shown for an autocomplete command.
pub fn command_composer_text(command: &SlashCommand) -> String {
    format!("/{}", command.name)
}

/// Whether the composer submitted Waku's global terminal-session picker.
/// The command is reserved by daemon-side discovery, so it is intentionally
/// provider-neutral and never crosses into a provider transport.
pub fn is_resume_submission(prompt: &str) -> bool {
    prompt.trim() == "/resume"
}

/// Whether the submitted text resolves to Codex's native fast-mode command,
/// which Waku bridges to the provider's service-tier control. Checking the
/// resolved entry preserves project/user command precedence when one of them
/// intentionally owns `/fast`.
pub fn is_fast_mode_toggle_submission(
    provider: ProviderKind,
    prompt: &str,
    commands: &[SlashCommand],
) -> bool {
    provider == ProviderKind::Codex
        && prompt.trim() == "/fast"
        && commands.iter().any(|command| {
            command.name == "fast"
                && command.scope == CommandScope::Builtin
                && command.template.is_none()
        })
}

/// Resolve the next concrete service-tier ID for Codex's Fast toggle. Model
/// metadata may expose the Fast tier as `fast` or as `priority`; the display
/// label is the stable product vocabulary, while the ID is provider-owned.
pub fn toggled_fast_service_tier(
    current: Option<&str>,
    service_tiers: &[ProviderModelOption],
) -> Option<String> {
    let fast = service_tiers.iter().find(|tier| {
        matches!(tier.id.as_str(), "fast" | "priority") || tier.label.eq_ignore_ascii_case("fast")
    })?;
    Some(if current == Some(fast.id.as_str()) {
        "default".to_owned()
    } else {
        fast.id.clone()
    })
}

/// A `/goal` composer submission parsed into its intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalCommand {
    /// Bare `/goal` — show the current goal (and offer to create one).
    Show,
    Edit,
    Pause,
    Resume,
    Clear,
    /// `/goal <objective>` — start or replace the goal with this objective.
    Set(String),
}

/// Whether this provider exposes a thread goal Waku can read and set.
///
/// One answer for both surfaces that offer goals: the `/goal` command and the
/// command palette entry that opens the same dialog. They were written apart
/// and had to agree; asking one function keeps a provider added later from
/// growing an entry that cannot work.
pub fn provider_supports_goals(provider: ProviderKind) -> bool {
    matches!(provider, ProviderKind::Codex | ProviderKind::Claude)
}

/// What a provider can do with a goal, beyond setting and clearing it.
///
/// Codex persists a goal with a status the host can flip between active and
/// paused, and reports token and time accounting. Claude's `/goal` is a
/// session Stop hook over a single condition instead: there is nothing to
/// pause, resume or budget, and asking its CLI to would set a new goal (every
/// argument is taken as a condition — `/goal pause` sets a goal named
/// "pause"). Both surfaces that show goal controls ask this, so the same
/// provider cannot offer a Pause button in one place and refuse it in another.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GoalCapabilities {
    /// The goal can be held and released without touching a turn.
    pub can_pause_and_resume: bool,
    /// The goal reports a token budget and its consumption.
    pub has_token_budget: bool,
}

pub fn goal_capabilities(provider: ProviderKind) -> GoalCapabilities {
    match provider {
        ProviderKind::Codex => GoalCapabilities {
            can_pause_and_resume: true,
            has_token_budget: true,
        },
        // Claude holds a goal as a live Stop hook: it either is set or it is
        // cleared, and the CLI owns the only accounting it produces.
        ProviderKind::Claude => GoalCapabilities {
            can_pause_and_resume: false,
            has_token_budget: false,
        },
        _ => GoalCapabilities {
            can_pause_and_resume: false,
            has_token_budget: false,
        },
    }
}

/// Parse the submitted text as a native `/goal` command, which Waku bridges
/// to the provider's goal surface. `None` when it is not one — wrong
/// provider, other text, or a project/user command that deliberately owns
/// `/goal` (resolution precedence stands).
///
/// The subcommands are Codex's. Claude's `/goal` takes a condition and
/// `clear` and nothing else, so on Claude only those two are read as such;
/// every other argument is the condition itself, which is what the CLI would
/// do with it anyway (`/goal pause` there sets a goal named "pause" rather
/// than pausing one). Parsing it as a subcommand would make Waku claim a
/// pause the provider cannot perform.
pub fn parse_goal_submission(
    provider: ProviderKind,
    prompt: &str,
    commands: &[SlashCommand],
) -> Option<GoalCommand> {
    if !provider_supports_goals(provider) {
        return None;
    }
    let invocation = prompt.trim().strip_prefix('/')?;
    let (name, arguments) = invocation
        .split_once(char::is_whitespace)
        .map_or((invocation, ""), |(name, arguments)| {
            (name, arguments.trim())
        });
    if name != "goal" {
        return None;
    }
    let goal_is_native_builtin = commands.iter().any(|command| {
        command.name == "goal"
            && command.scope == CommandScope::Builtin
            && command.template.is_none()
    });
    if !goal_is_native_builtin {
        return None;
    }
    let subcommands = goal_capabilities(provider).can_pause_and_resume;
    Some(match arguments {
        "" => GoalCommand::Show,
        "edit" if subcommands => GoalCommand::Edit,
        "pause" if subcommands => GoalCommand::Pause,
        "resume" if subcommands => GoalCommand::Resume,
        "clear" => GoalCommand::Clear,
        objective => GoalCommand::Set(objective.to_owned()),
    })
}

pub fn expand_command_template(template: &str, args: &str) -> String {
    let positional = args.split_whitespace().collect::<Vec<_>>();
    let mut expanded = String::with_capacity(template.len() + args.len());
    let mut consumed_args = false;
    let mut rest = template;
    while let Some(index) = rest.find('$') {
        expanded.push_str(&rest[..index]);
        let after = &rest[index + 1..];
        if let Some(tail) = after.strip_prefix("ARGUMENTS") {
            expanded.push_str(args);
            consumed_args = true;
            rest = tail;
        } else if let Some(tail) = after.strip_prefix('@') {
            expanded.push_str(args);
            consumed_args = true;
            rest = tail;
        } else if let Some(digit) = after
            .chars()
            .next()
            .and_then(|character| character.to_digit(10))
            .filter(|digit| (1..=9).contains(digit))
        {
            if let Some(argument) = positional.get(digit as usize - 1) {
                expanded.push_str(argument);
            }
            consumed_args = true;
            rest = &after[1..];
        } else {
            expanded.push('$');
            rest = after;
        }
    }
    expanded.push_str(rest);
    if !consumed_args && !args.is_empty() {
        expanded.push_str("\n\n");
        expanded.push_str(args);
    }
    expanded
}

/// Resolve composer text into the exact prompt expected by the provider.
///
/// Template commands expand to their body. Skills keep a slash in the
/// composer and transcript, then resolve to each provider's native syntax at
/// the transport boundary.
pub fn resolved_submission(
    provider: ProviderKind,
    prompt: &str,
    commands: &[SlashCommand],
) -> Option<String> {
    if let Some(skill) = resolved_skill_submission(provider, prompt, commands) {
        return Some(skill);
    }
    let invocation = prompt.strip_prefix('/')?;
    let (name, args) = invocation
        .split_once(char::is_whitespace)
        .map_or((invocation, ""), |(name, args)| (name, args.trim()));
    let command = commands.iter().find(|command| command.name == name)?;
    let template = command.template.as_deref()?;
    Some(expand_command_template(template, args))
}

/// Resolve only provider-native skill syntax, without expanding templates.
pub fn resolved_skill_submission(
    provider: ProviderKind,
    prompt: &str,
    commands: &[SlashCommand],
) -> Option<String> {
    if !matches!(
        provider,
        ProviderKind::Codex | ProviderKind::Fx | ProviderKind::Pi | ProviderKind::OhMyPi
    ) {
        return None;
    }
    let invocation = prompt.strip_prefix('/')?;
    let name = invocation
        .split_once(char::is_whitespace)
        .map_or(invocation, |(name, _)| name);
    if !commands
        .iter()
        .any(|command| command.name == name && command.scope == CommandScope::Skill)
    {
        return None;
    }
    Some(match provider {
        ProviderKind::Codex | ProviderKind::Fx => format!("${invocation}"),
        ProviderKind::Pi | ProviderKind::OhMyPi => format!("/skill:{invocation}"),
        _ => unreachable!("non-native skill providers returned above"),
    })
}

pub fn matcher() -> Matcher {
    Matcher::new(nucleo_matcher::Config::DEFAULT.match_paths())
}

#[derive(Clone, Debug)]
pub struct Scored<T> {
    pub item: T,
    pub positions: Vec<u32>,
}

fn filter_scored(
    haystack: &[&str],
    query: &str,
    matcher: &mut Matcher,
    cap: usize,
) -> Vec<(usize, Vec<u32>)> {
    if query.trim().is_empty() {
        return (0..haystack.len().min(cap))
            .map(|index| (index, Vec::new()))
            .collect();
    }
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored = Vec::new();
    for (index, text) in haystack.iter().enumerate() {
        if let Some(score) = pattern.score(Utf32Str::new(text, &mut buf), matcher) {
            scored.push((score, index));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(cap);
    scored
        .into_iter()
        .map(|(_, index)| {
            let mut positions = Vec::new();
            pattern.indices(
                Utf32Str::new(haystack[index], &mut buf),
                matcher,
                &mut positions,
            );
            positions.sort_unstable();
            positions.dedup();
            (index, positions)
        })
        .collect()
}

pub fn filter_commands(
    commands: &[SlashCommand],
    query: &str,
    matcher: &mut Matcher,
) -> Vec<Scored<SlashCommand>> {
    let names = commands
        .iter()
        .map(|command| command.name.as_str())
        .collect::<Vec<_>>();
    filter_scored(&names, query, matcher, FILTER_CAP)
        .into_iter()
        .map(|(index, positions)| Scored {
            item: commands[index].clone(),
            positions,
        })
        .collect()
}

pub fn filter_files(
    files: &[FileEntry],
    query: &str,
    matcher: &mut Matcher,
) -> Vec<Scored<FileEntry>> {
    let paths = files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    filter_scored(&paths, query, matcher, FILTER_CAP)
        .into_iter()
        .map(|(index, positions)| Scored {
            item: files[index].clone(),
            positions,
        })
        .collect()
}

pub fn highlight_byte_ranges(
    text: &str,
    positions: &[u32],
    char_offset: usize,
) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for (char_index, (byte_index, character)) in (char_offset..).zip(text.char_indices()) {
        if positions.binary_search(&(char_index as u32)).is_ok() {
            let byte_end = byte_index + character.len_utf8();
            match ranges.last_mut() {
                Some(last) if last.end == byte_index => last.end = byte_end,
                _ => ranges.push(byte_index..byte_end),
            }
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_command_picker_puts_builtins_first_and_skills_last() {
        let discovered = vec![
            command("deploy", CommandScope::Skill),
            command("format", CommandScope::User),
            command("lint", CommandScope::Project),
            command("review", CommandScope::Builtin),
            command("resume", CommandScope::Waku),
        ];
        let reported = vec![ReportedCommand {
            name: "compact".into(),
            description: "Free up context".into(),
        }];

        let merged = merge_reported_commands(&discovered, &reported);
        assert_eq!(
            merged
                .iter()
                .map(|command| (command.scope, command.name.as_str()))
                .collect::<Vec<_>>(),
            [
                (CommandScope::Builtin, "compact"),
                (CommandScope::Waku, "resume"),
                (CommandScope::Builtin, "review"),
                (CommandScope::Project, "lint"),
                (CommandScope::User, "format"),
                (CommandScope::Skill, "deploy"),
            ]
        );
    }

    #[test]
    fn fast_toggle_is_codex_only_and_respects_command_overrides() {
        let builtin = command("fast", CommandScope::Builtin);
        assert!(is_fast_mode_toggle_submission(
            ProviderKind::Codex,
            "/fast ",
            std::slice::from_ref(&builtin),
        ));
        assert!(!is_fast_mode_toggle_submission(
            ProviderKind::Claude,
            "/fast",
            std::slice::from_ref(&builtin),
        ));
        assert!(!is_fast_mode_toggle_submission(
            ProviderKind::Codex,
            "/fast now",
            std::slice::from_ref(&builtin),
        ));
        assert!(!is_fast_mode_toggle_submission(
            ProviderKind::Codex,
            "/fast",
            &[command("fast", CommandScope::Project)],
        ));
    }

    #[test]
    fn resume_is_an_exact_provider_neutral_local_command() {
        assert!(is_resume_submission("/resume"));
        assert!(is_resume_submission("  /resume  "));
        assert!(!is_resume_submission("/resume latest"));
        assert!(!is_resume_submission("please /resume"));
    }

    #[test]
    fn fast_toggle_uses_the_models_concrete_service_tier_id() {
        let tiers = [ProviderModelOption::new("priority", "Fast")];
        assert_eq!(
            toggled_fast_service_tier(Some("default"), &tiers).as_deref(),
            Some("priority")
        );
        assert_eq!(
            toggled_fast_service_tier(Some("priority"), &tiers).as_deref(),
            Some("default")
        );
        assert_eq!(toggled_fast_service_tier(None, &[]), None);
    }

    #[test]
    fn codex_skill_completion_keeps_slash_in_the_composer() {
        let skill = command("mattpocock-skills:to-spec", CommandScope::Skill);
        assert_eq!(command_composer_text(&skill), "/mattpocock-skills:to-spec");
        assert_eq!(
            command_composer_text(&command("fast", CommandScope::Builtin)),
            "/fast"
        );
    }

    #[test]
    fn codex_skill_submission_uses_the_catalog_invocation() {
        let skill = command("mattpocock-skills:to-spec", CommandScope::Skill);
        assert_eq!(
            resolved_submission(
                ProviderKind::Codex,
                "/mattpocock-skills:to-spec carefully",
                std::slice::from_ref(&skill)
            )
            .as_deref(),
            Some("$mattpocock-skills:to-spec carefully")
        );
        assert_eq!(
            resolved_submission(
                ProviderKind::Claude,
                "/mattpocock-skills:to-spec carefully",
                std::slice::from_ref(&skill)
            ),
            None
        );
    }

    #[test]
    fn fx_skill_submission_uses_the_catalog_invocation() {
        let skill = command("deploy", CommandScope::Skill);
        assert_eq!(
            resolved_submission(
                ProviderKind::Fx,
                "/deploy production",
                std::slice::from_ref(&skill)
            )
            .as_deref(),
            Some("$deploy production")
        );
    }

    #[test]
    fn pi_skill_submission_uses_the_skill_command() {
        for provider in [ProviderKind::Pi, ProviderKind::OhMyPi] {
            for name in ["to-spec", "to-tickets"] {
                let skill = command(name, CommandScope::Skill);
                let expected = format!("/skill:{name} carefully");
                assert_eq!(
                    resolved_skill_submission(provider, &format!("/{name} carefully"), &[skill])
                        .as_deref(),
                    Some(expected.as_str())
                );
            }
        }
    }

    fn command(name: &str, scope: CommandScope) -> SlashCommand {
        SlashCommand {
            name: name.into(),
            description: String::new(),
            scope,
            argument_hint: None,
            template: None,
        }
    }

    #[test]
    fn goal_submissions_parse_into_their_intent() {
        let builtin = command("goal", CommandScope::Builtin);
        let commands = std::slice::from_ref(&builtin);
        let parse = |prompt: &str| parse_goal_submission(ProviderKind::Codex, prompt, commands);

        assert_eq!(parse("/goal"), Some(GoalCommand::Show));
        assert_eq!(parse("/goal "), Some(GoalCommand::Show));
        assert_eq!(parse("/goal edit"), Some(GoalCommand::Edit));
        assert_eq!(parse("/goal pause"), Some(GoalCommand::Pause));
        assert_eq!(parse("/goal resume"), Some(GoalCommand::Resume));
        assert_eq!(parse("/goal clear"), Some(GoalCommand::Clear));
        assert_eq!(
            parse("/goal improve benchmark coverage"),
            Some(GoalCommand::Set("improve benchmark coverage".into()))
        );
        assert_eq!(parse("/goals"), None);
        assert_eq!(parse("ship /goal"), None);
    }

    #[test]
    fn goal_command_respects_what_each_provider_offers_and_overrides() {
        let builtin = command("goal", CommandScope::Builtin);
        // Neither provider is excluded any more, but the subcommands are not
        // shared: Claude has no pause, so there `pause` is a condition.
        assert_eq!(
            parse_goal_submission(
                ProviderKind::Claude,
                "/goal pause",
                std::slice::from_ref(&builtin)
            ),
            Some(GoalCommand::Set("pause".into()))
        );
        assert_eq!(
            parse_goal_submission(
                ProviderKind::Codex,
                "/goal pause",
                std::slice::from_ref(&builtin)
            ),
            Some(GoalCommand::Pause)
        );
        // A project command deliberately owning /goal wins the collision.
        let mut project = command("goal", CommandScope::Project);
        project.template = Some("do project things".into());
        assert_eq!(
            parse_goal_submission(ProviderKind::Codex, "/goal", std::slice::from_ref(&project)),
            None
        );
    }

    /// A provider with no goal surface at all parses nothing, whichever
    /// command table it happens to carry.
    #[test]
    fn a_provider_without_goals_parses_none() {
        let builtin = command("goal", CommandScope::Builtin);
        let commands = std::slice::from_ref(&builtin);
        for provider in ProviderKind::ALL {
            if provider_supports_goals(provider) {
                continue;
            }
            assert_eq!(
                parse_goal_submission(provider, "/goal", commands),
                None,
                "{provider:?}"
            );
        }
    }

    /// The command palette's entry and the `/goal` command read the same
    /// answer, so a provider cannot offer one surface and refuse the other.
    ///
    /// It has to be the same one the parser checks: the palette entry opens
    /// the very dialog a `/goal` submission acts through, and an entry shown
    /// on a provider with no goal surface would open a dialog whose Save could
    /// never land.
    #[test]
    fn the_goal_capability_answer_matches_what_the_parser_accepts() {
        let builtin = command("goal", CommandScope::Builtin);
        let commands = std::slice::from_ref(&builtin);
        for provider in [
            ProviderKind::Codex,
            ProviderKind::Claude,
            ProviderKind::OpenCode,
        ] {
            let parses = parse_goal_submission(provider, "/goal", commands).is_some();
            assert_eq!(
                provider_supports_goals(provider),
                parses,
                "{provider:?} must answer the same in the palette and in /goal"
            );
        }
    }

    /// The set of providers offering goals is not "whatever the parser
    /// happens to say".
    ///
    /// The agreement test above cannot catch a widening that moves both
    /// surfaces together — make the capability say yes to everything and the
    /// parser follows it. This pins the set itself.
    #[test]
    fn exactly_codex_and_claude_report_goal_support() {
        // `ALL` rather than a hand-written list, so a provider added later is
        // covered without anyone remembering to extend this.
        for provider in ProviderKind::ALL {
            assert_eq!(
                provider_supports_goals(provider),
                matches!(provider, ProviderKind::Codex | ProviderKind::Claude),
                "{provider:?}"
            );
        }
        assert_eq!(
            ProviderKind::ALL
                .iter()
                .filter(|provider| provider_supports_goals(**provider))
                .count(),
            2,
            "exactly two providers expose a goal surface"
        );
    }

    /// Pause and resume are Codex's alone, and that has to be asked of the
    /// capability rather than assumed from goal support.
    ///
    /// Claude's `/goal pause` sets a goal *named* pause, so a Pause button
    /// there would look like it worked while quietly replacing the user's
    /// objective. One function answers this so both goal surfaces agree.
    #[test]
    fn only_codex_can_pause_and_resume_a_goal() {
        for provider in ProviderKind::ALL {
            let capabilities = goal_capabilities(provider);
            let pausable = provider == ProviderKind::Codex;
            assert_eq!(capabilities.can_pause_and_resume, pausable, "{provider:?}");
            assert_eq!(capabilities.has_token_budget, pausable, "{provider:?}");
        }
        // The capability must not contradict the parse: anywhere a pause is
        // refused as a capability, the parser must read it as a condition
        // instead of an action Waku cannot perform.
        let builtin = command("goal", CommandScope::Builtin);
        let commands = std::slice::from_ref(&builtin);
        for provider in ProviderKind::ALL
            .iter()
            .copied()
            .filter(|p| provider_supports_goals(*p))
        {
            let parsed = parse_goal_submission(provider, "/goal pause", commands);
            if goal_capabilities(provider).can_pause_and_resume {
                assert_eq!(parsed, Some(GoalCommand::Pause), "{provider:?}");
            } else {
                assert_eq!(
                    parsed,
                    Some(GoalCommand::Set("pause".into())),
                    "{provider:?}"
                );
            }
        }
    }
}

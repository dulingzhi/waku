import type { ProviderKind, SkillEntry, SkillSource, SkillsCatalog } from '@waku/client';

import type { Translator } from './i18n-context';
import { providerLabel } from './session-presentation';

/** One row of the skills list: a project section header, or a skill. */
export type SkillListRow =
  | { type: 'section'; key: string; label: string; count: number }
  | { type: 'skill'; key: string; skill: SkillEntry };

/**
 * Skills arrive as one flat catalogue; the library groups them by the project
 * they belong to, with user-scoped skills under their own header. The order
 * follows the catalogue so a skill never jumps between renders, and both
 * groups keep their own section so a project override reads as one.
 */
export function groupSkillRows(skills: readonly SkillEntry[], t: Translator): SkillListRow[] {
  const groups = new Map<string, SkillEntry[]>();
  for (const skill of skills) {
    const key = skill.project ?? '';
    const group = groups.get(key);
    if (group) group.push(skill);
    else groups.set(key, [skill]);
  }
  const rows: SkillListRow[] = [];
  for (const [project, grouped] of groups) {
    rows.push({
      type: 'section',
      key: `section:${project || 'user'}`,
      label: project || t('skills.section_user_skills'),
      count: grouped.length,
    });
    for (const skill of grouped) {
      rows.push({ type: 'skill', key: `skill:${skill.rowKey}`, skill });
    }
  }
  return rows;
}

/** Whether a skill matches the search box. Mirrors the web client: the name,
 * description, project and every install directory are searchable, so a user
 * who knows only the on-disk path can still find it. */
export function skillMatches(skill: SkillEntry, query: string): boolean {
  const needle = query.trim().toLocaleLowerCase();
  if (!needle) return true;
  return [
    skill.name,
    skill.description,
    skill.project ?? '',
    ...skill.installs.map((install) => install.dir),
  ].join(' ').toLocaleLowerCase().includes(needle);
}

/** The label for one install source: "Shared" for the ecosystem-neutral
 * root, otherwise the provider's brand name. */
export function skillSourceLabel(source: SkillSource, t: Translator): string {
  return source === 'shared' ? t('skills.source_shared') : providerLabel(source.provider);
}

/** Every distinct source a skill is installed into, joined for one line. */
export function skillSourcesLabel(skill: SkillEntry, t: Translator): string {
  return [...new Set(skill.installs.map((install) => skillSourceLabel(install.source, t)))]
    .join(' · ');
}

/** The provider marks to show beside a skill, in install order and deduped.
 * Shared installs carry no provider mark. */
export function skillSourceProviders(skill: SkillEntry): ProviderKind[] {
  const seen = new Set<ProviderKind>();
  const providers: ProviderKind[] = [];
  for (const install of skill.installs) {
    if (install.source === 'shared') continue;
    const { provider } = install.source;
    if (seen.has(provider)) continue;
    seen.add(provider);
    providers.push(provider);
  }
  return providers;
}

/** Compact byte size, matching the web client's thresholds. */
export function formatSkillBytes(value: number): string {
  const format = (amount: number) => Number.isInteger(amount) ? String(amount) : amount.toFixed(1);
  if (value < 1_024) return `${format(value)} B`;
  if (value < 1_048_576) return `${format(value / 1_024)} KB`;
  return `${format(value / 1_048_576)} MB`;
}

/**
 * Relative "modified" label. `modifiedAt` is seconds since the epoch and may
 * be null when the daemon could not stat the file, in which case there is no
 * line to show at all.
 */
export function formatSkillUpdated(
  modifiedAt: number | null,
  nowSeconds: number,
  t: Translator,
): string | null {
  if (modifiedAt === null) return null;
  const seconds = Math.max(0, Math.floor(nowSeconds) - modifiedAt);
  if (seconds < 60) return t('skills.updated_just_now');
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return t('skills.updated_minutes', { count: minutes });
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return t('skills.updated_hours', { count: hours });
  return t('skills.updated_days', { count: Math.floor(hours / 24) });
}

/** The count line under the library header: total skills, and how many are
 * disabled when any is. */
export function skillsCountLabel(
  skills: readonly SkillEntry[],
  t: Translator,
): string {
  const total = skills.length === 1
    ? t('skills.count_one')
    : t('skills.count_many', { count: skills.length });
  const disabled = skills.filter((skill) => !skill.enabled).length;
  return disabled ? `${total} · ${t('skills.count_disabled', { count: disabled })}` : total;
}

/**
 * The route key for a skill: `(project, name)`.
 *
 * Not `rowKey`: the daemon derives it by hashing every install dir *and* its
 * enabled flag, so toggling a skill off changes the key and the detail route
 * would stop resolving against the skill the user just toggled. Project and
 * name are the daemon's own grouping identity (`by_identity` in
 * `crates/waku-core/src/skills.rs`), so they stay put across toggles.
 *
 * Both halves are percent-encoded and joined with `:`, which
 * `encodeURIComponent` escapes inside either half — so the second `:` in the
 * string is always the separator, and the key survives being a route segment.
 */
export function skillRouteKey(skill: SkillEntry): string {
  return `${encodeURIComponent(skill.project ?? '')}:${encodeURIComponent(skill.name)}`;
}

/** Resolve a route key back against a catalogue. */
export function findSkillByRouteKey(
  skills: readonly SkillEntry[],
  key: string,
): SkillEntry | undefined {
  return skills.find((skill) => skillRouteKey(skill) === key);
}

/** The scope caption for the detail pane: user-scoped skills are available
 * everywhere, project skills name their project. */
export function skillScopeLabel(skill: SkillEntry, t: Translator): string {
  return skill.project
    ? t('skills.scope_in_project', { project: skill.project })
    : t('skills.scope_user_detail');
}

/** The supporting-file count, singular or plural. */
export function skillFilesLabel(skill: SkillEntry, t: Translator): string {
  return skill.supportingFiles === 1
    ? t('skills.file_count_one')
    : t('skills.file_count_many', { count: skill.supportingFiles });
}

/** The duplicate warning, when the same skill name exists elsewhere. */
export function skillDuplicatesLabel(skill: SkillEntry, t: Translator): string | null {
  if (skill.duplicates <= 0) return null;
  return skill.duplicates === 1
    ? t('skills.duplicate_one')
    : t('skills.duplicate_many', { count: skill.duplicates });
}

/** How many install locations a skill spans, pluralized — the delete button
 * says this so the user knows how much one tap removes. */
export function skillInstallCountLabel(count: number, t: Translator): string {
  return count === 1
    ? t('skills.install_count_one')
    : t('skills.install_count_many', { count });
}

/**
 * How many install locations the detail pane spells out. A skill can be copied
 * into every ecosystem's root, and printing ten absolute paths pushes the
 * SKILL.md off the screen; the rest collapse into a count.
 */
export const SKILL_VISIBLE_INSTALLS = 3;

export function visibleSkillInstalls(
  skill: SkillEntry,
  limit = SKILL_VISIBLE_INSTALLS,
): { shown: string[]; hidden: number } {
  const dirs = skill.installs.map((install) => install.dir);
  return {
    shown: dirs.slice(0, limit),
    hidden: Math.max(0, dirs.length - limit),
  };
}

/**
 * The catalogue with one skill's enabled flag flipped, for an optimistic
 * update while the daemon round-trip is in flight.
 *
 * The daemon derives a skill's `enabled` from its installs (enabling renames
 * `SKILL.md`/`DISABLED_SKILL.md` in every one of them), so both the entry and
 * each install flip together and the row's badge does not disagree with its
 * source caption for the duration.
 *
 * `rowKey` is deliberately left alone. The daemon re-hashes it on the next
 * scan because the enabled flag is part of that hash, but inventing a value
 * here would only make the list reorder twice.
 */
export function withSkillEnabled(
  catalog: SkillsCatalog,
  routeKey: string,
  enabled: boolean,
): SkillsCatalog {
  let changed = false;
  const skills = catalog.skills.map((skill) => {
    if (skillRouteKey(skill) !== routeKey) return skill;
    changed = true;
    return {
      ...skill,
      enabled,
      installs: skill.installs.map((install) => ({ ...install, enabled })),
    };
  });
  return changed ? { ...catalog, skills } : catalog;
}

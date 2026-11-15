import { describe, expect, test } from 'bun:test';
import type { SkillEntry } from '@waku/client';

import { translate, type TranslationParams } from './i18n';
import {
  findSkillByRouteKey,
  formatSkillBytes,
  formatSkillUpdated,
  groupSkillRows,
  skillDuplicatesLabel,
  skillFilesLabel,
  skillInstallCountLabel,
  skillMatches,
  skillRouteKey,
  skillScopeLabel,
  skillSourceProviders,
  skillSourcesLabel,
  skillsCountLabel,
  visibleSkillInstalls,
  withSkillEnabled,
} from './skills-presentation';

const t = (key: string, params?: TranslationParams) => translate('en', key, params);

function skill(overrides: Partial<SkillEntry> = {}): SkillEntry {
  return {
    name: 'deploy',
    description: 'Ship the app',
    scope: 'user',
    project: null,
    installs: [{ source: 'shared', dir: '/home/.claude/skills/deploy', skillFile: 'SKILL.md', enabled: true }],
    enabled: true,
    allowedTools: null,
    body: '# Deploy',
    supportingFiles: 2,
    totalBytes: 2048,
    modifiedAt: 1_000,
    duplicates: 0,
    rowKey: 1,
    ...overrides,
  };
}

describe('mobile skills presentation', () => {
  test('groups user skills and each project under their own section', () => {
    const rows = groupSkillRows([
      skill({ rowKey: 1, project: null }),
      skill({ rowKey: 2, project: 'Waku', name: 'review' }),
      skill({ rowKey: 3, project: 'Waku', name: 'test' }),
    ], t);
    expect(rows.map((row) => row.type === 'section'
      ? `${row.label}:${row.count}`
      : (row as { skill: SkillEntry }).skill.name)).toEqual([
      'User skills:1',
      'deploy',
      'Waku:2',
      'review',
      'test',
    ]);
  });

  test('keeps catalogue order inside a group so a skill never jumps', () => {
    const rows = groupSkillRows([
      skill({ rowKey: 1, project: 'A', name: 'zeta' }),
      skill({ rowKey: 2, project: 'A', name: 'alpha' }),
    ], t);
    expect(rows.filter((row) => row.type === 'skill').map((row) => (
      row as { skill: SkillEntry }
    ).skill.name)).toEqual(['zeta', 'alpha']);
  });

  test('searches name, description, project and install directory', () => {
    const entry = skill({
      name: 'deploy',
      description: 'Ship the app',
      project: 'Waku',
      installs: [{ source: 'shared', dir: '/opt/skills/deploy', skillFile: 'SKILL.md', enabled: true }],
    });
    expect(skillMatches(entry, 'dep')).toBe(true);
    expect(skillMatches(entry, 'ship')).toBe(true);
    expect(skillMatches(entry, 'waku')).toBe(true);
    expect(skillMatches(entry, '/opt/skills')).toBe(true);
    expect(skillMatches(entry, 'nothing')).toBe(false);
    // An empty query matches everything, so clearing the box restores the list.
    expect(skillMatches(entry, '   ')).toBe(true);
  });

  test('labels shared installs and provider installs distinctly', () => {
    const entry = skill({
      installs: [
        { source: 'shared', dir: '/shared/deploy', skillFile: 'SKILL.md', enabled: true },
        { source: { provider: 'codex' }, dir: '/codex/deploy', skillFile: 'SKILL.md', enabled: true },
      ],
    });
    expect(skillSourcesLabel(entry, t)).toBe('Shared · Codex');
    // The provider marks read from installs, deduped, and skip shared.
    expect(skillSourceProviders(entry)).toEqual(['codex']);
  });

  test('dedupes repeated provider installs into one mark', () => {
    const entry = skill({
      installs: [
        { source: { provider: 'codex' }, dir: '/a', skillFile: 'SKILL.md', enabled: true },
        { source: { provider: 'codex' }, dir: '/b', skillFile: 'SKILL.md', enabled: true },
        { source: { provider: 'claude' }, dir: '/c', skillFile: 'SKILL.md', enabled: true },
      ],
    });
    expect(skillSourceProviders(entry)).toEqual(['codex', 'claude']);
    expect(skillSourcesLabel(entry, t)).toBe('Codex · Claude');
  });

  test('formats byte sizes compactly', () => {
    expect(formatSkillBytes(512)).toBe('512 B');
    expect(formatSkillBytes(2048)).toBe('2 KB');
    expect(formatSkillBytes(1024 * 1024 * 1.5)).toBe('1.5 MB');
  });

  test('formats a relative modified time and omits it when unknown', () => {
    const now = 10_000;
    expect(formatSkillUpdated(null, now, t)).toBeNull();
    expect(formatSkillUpdated(now, now, t)).toBe('Just now');
    expect(formatSkillUpdated(now - 120, now, t)).toBe('2m ago');
    expect(formatSkillUpdated(now - 7_200, now, t)).toBe('2h ago');
    expect(formatSkillUpdated(now - 172_800, now, t)).toBe('2d ago');
    // A clock skew that puts the modification ahead of now must not read as
    // a negative age.
    expect(formatSkillUpdated(now + 500, now, t)).toBe('Just now');
  });

  test('counts skills and how many are disabled', () => {
    expect(skillsCountLabel([], t)).toBe('0 skills');
    expect(skillsCountLabel([skill()], t)).toBe('1 skill');
    expect(skillsCountLabel([skill({ rowKey: 1 }), skill({ rowKey: 2 })], t)).toBe('2 skills');
    expect(skillsCountLabel([
      skill({ rowKey: 1 }),
      skill({ rowKey: 2, enabled: false }),
    ], t)).toBe('2 skills · 1 disabled');
  });
});

describe('skill detail presentation', () => {
  test('route key survives the enabled flag flipping rowKey', () => {
    const before = skill({ rowKey: 11, enabled: true });
    const after = skill({ rowKey: 99, enabled: false });
    // The daemon re-hashes rowKey from the install dirs plus their enabled
    // flags, so a toggle changes it. The route must not move with it.
    expect(before.rowKey).not.toBe(after.rowKey);
    expect(skillRouteKey(before)).toBe(skillRouteKey(after));
  });

  test('route key round-trips through a URL segment', () => {
    const entry = skill({ project: 'Waku App/ios:beta', name: 'deploy:prod' });
    const key = skillRouteKey(entry);
    // encodeURIComponent escapes both the separators a project name could
    // contain and the route's own path shell.
    expect(key).not.toContain('/');
    expect(decodeURIComponent(key)).toBe('Waku App/ios:beta:deploy:prod');
    expect(findSkillByRouteKey([entry], key)).toBe(entry);
  });

  test('finds nothing for a key that no longer matches the catalogue', () => {
    const entry = skill();
    expect(findSkillByRouteKey([entry], 'gone:away')).toBeUndefined();
  });

  test('labels project scope and user scope differently', () => {
    expect(skillScopeLabel(skill({ project: 'Waku' }), t)).toBe('in Waku');
    expect(skillScopeLabel(skill({ project: null }), t)).toBe('available in every project');
  });

  test('pluralizes the supporting-file count', () => {
    expect(skillFilesLabel(skill({ supportingFiles: 1 }), t)).toBe('1 supporting file');
    expect(skillFilesLabel(skill({ supportingFiles: 4 }), t)).toBe('4 supporting files');
  });

  test('warns about duplicates only when there are some', () => {
    expect(skillDuplicatesLabel(skill({ duplicates: 0 }), t)).toBeNull();
    expect(skillDuplicatesLabel(skill({ duplicates: 1 }), t))
      .toBe('Same name in 1 other location — the most specific copy wins');
    expect(skillDuplicatesLabel(skill({ duplicates: 3 }), t))
      .toBe('Same name in 3 other locations — the most specific copy wins');
  });
});

describe('skill mutations', () => {
  test('caps the install locations the detail pane prints', () => {
    const entry = skill({
      installs: Array.from({ length: 5 }, (_, index) => ({
        source: { provider: 'codex' as const },
        dir: `/install/${index}`,
        skillFile: 'SKILL.md',
        enabled: true,
      })),
    });
    const page = visibleSkillInstalls(entry);
    expect(page.shown).toEqual(['/install/0', '/install/1', '/install/2']);
    expect(page.hidden).toBe(2);
    // Fewer installs than the cap hide nothing.
    expect(visibleSkillInstalls(skill({ installs: [] })).hidden).toBe(0);
  });

  test('flips the entry and every install together, leaving rowKey alone', () => {
    const entry = skill({
      rowKey: 7,
      enabled: true,
      installs: [
        { source: 'shared', dir: '/a', skillFile: 'SKILL.md', enabled: true },
        { source: { provider: 'codex' }, dir: '/b', skillFile: 'SKILL.md', enabled: true },
      ],
    });
    const next = withSkillEnabled({ skills: [entry] }, skillRouteKey(entry), false);
    const updated = next.skills[0]!;
    expect(updated.enabled).toBe(false);
    expect(updated.installs.every((install) => !install.enabled)).toBe(true);
    // The daemon re-hashes rowKey from the enabled flags on the next scan;
    // inventing one here would only make the list reorder twice.
    expect(updated.rowKey).toBe(7);
    // The route key is unaffected, so the detail page keeps resolving.
    expect(skillRouteKey(updated)).toBe(skillRouteKey(entry));
  });

  test('leaves an unrelated skill untouched', () => {
    const target = skill({ rowKey: 1, name: 'deploy' });
    const other = skill({ rowKey: 2, name: 'review' });
    const next = withSkillEnabled({ skills: [target, other] }, skillRouteKey(target), false);
    expect(next.skills[1]).toBe(other);
  });

  test('returns the same catalogue when no skill matches', () => {
    const catalog = { skills: [skill()] };
    expect(withSkillEnabled(catalog, 'nope:nothing', false)).toBe(catalog);
  });

  test('pluralizes the install-count note on the delete button', () => {
    expect(skillInstallCountLabel(1, t)).toBe('1 location');
    expect(skillInstallCountLabel(2, t)).toBe('2 locations');
    expect(skillInstallCountLabel(4, t)).toBe('4 locations');
  });

  test('the remembered route key still resolves after a toggle reorders rowKey', () => {
    // The list remembers `skillRouteKey` for the row the user opened. A toggle
    // re-hashes rowKey, so a restore keyed on that would silently give up;
    // this is the path that has to keep working.
    const before = skill({ rowKey: 1, enabled: true });
    const remembered = skillRouteKey(before);
    const afterToggle = withSkillEnabled({ skills: [before] }, remembered, false).skills[0]!;
    expect(afterToggle.rowKey).toBe(before.rowKey); // optimistic write keeps it
    // The daemon's next scan supplies a new rowKey; the route key still matches.
    const rescanned = { ...afterToggle, rowKey: 4242 };
    const rows = groupSkillRows([rescanned], t).filter((row) => row.type === 'skill');
    expect(rows.some((row) => row.type === 'skill' && skillRouteKey(row.skill) === remembered))
      .toBe(true);
  });
});

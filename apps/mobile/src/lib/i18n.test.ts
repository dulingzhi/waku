import { describe, expect, test } from 'bun:test';
import { readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { MOBILE_CATALOG } from './i18n-catalog';
import {
  currentLanguage,
  deviceLanguages,
  languageLabel,
  normalizeLanguage,
  resolvedLocale,
  translate,
} from './i18n';

// The test lives in `src/lib`, so the app root is two levels up. `new URL('..')`
// alone would land on `apps/` and silently scan the sibling web app too.
const mobileRoot = fileURLToPath(new URL('../../', import.meta.url));

async function sourceFiles(dir: string, out: string[] = []): Promise<string[]> {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) await sourceFiles(path, out);
    else if (/\.tsx?$/.test(entry.name) && !entry.name.includes('.test.')) out.push(path);
  }
  return out;
}

/** Every `t('some.key')` / `t("some.key")` literal in the mobile sources. */
async function referencedKeys(): Promise<Map<string, string[]>> {
  const found = new Map<string, string[]>();
  for (const file of await sourceFiles(mobileRoot)) {
    const text = await Bun.file(file).text();
    // Keys are lowercase by convention, but match case-insensitively so a typo
    // with capitals is reported instead of silently skipped.
    for (const match of text.matchAll(/\bt\(\s*['"]([A-Za-z][A-Za-z0-9_]*(?:\.[A-Za-z0-9_]+)+)['"]/g)) {
      const key = match[1]!;
      found.set(key, [...(found.get(key) ?? []), file]);
    }
  }
  return found;
}

describe('mobile i18n', () => {
  test('every locale ships the same keys as English', () => {
    const english = Object.keys(MOBILE_CATALOG.en).sort();
    expect(english.length).toBeGreaterThan(0);
    for (const locale of ['zh-CN', 'ja'] as const) {
      expect(Object.keys(MOBILE_CATALOG[locale]).sort()).toEqual(english);
    }
  });

  test('no catalog ships an empty message', () => {
    // Mirrors the shared-cache exception: this message renders on both sides of
    // an inline control, so a language may carry the sentence in the suffix.
    const allowedEmpty = new Set(['onboarding.what_should_we_build_in']);
    for (const locale of ['en', 'zh-CN', 'ja'] as const) {
      const empty = Object.keys(MOBILE_CATALOG[locale]).filter(
        (key) => MOBILE_CATALOG[locale][key]!.trim() === '' && !allowedEmpty.has(key),
      );
      expect({ locale, empty }).toEqual({ locale, empty: [] });
    }
  });

  test('every key the app references exists in the catalog', async () => {
    // A typo would otherwise silently render the raw key in the UI.
    const missing: string[] = [];
    for (const [key, files] of await referencedKeys()) {
      if (key in MOBILE_CATALOG.en) continue;
      missing.push(`${key} (${files.map((file) => file.replace(mobileRoot, '')).join(', ')})`);
    }
    expect(missing.sort()).toEqual([]);
  });

  test('translations keep the placeholders their English message declares', () => {
    const placeholders = (value: string) =>
      [...value.matchAll(/%\{([A-Za-z0-9_]+)\}/g)].map((match) => match[1]!).sort();
    const drifted: string[] = [];
    for (const key of Object.keys(MOBILE_CATALOG.en)) {
      const expected = placeholders(MOBILE_CATALOG.en[key]!);
      for (const locale of ['zh-CN', 'ja'] as const) {
        const actual = placeholders(MOBILE_CATALOG[locale][key] ?? '');
        if (expected.join(',') !== actual.join(',')) {
          drifted.push(`${locale}:${key} expected [${expected}] got [${actual}]`);
        }
      }
    }
    expect(drifted).toEqual([]);
  });

  test('interpolates and falls back like the shared core', () => {
    expect(translate('ja', 'errors.task_not_found')).toBe('タスクが見つかりません');
    expect(translate('zh-CN', 'connection.retrying_in', { seconds: 3 })).toContain('3');
    expect(translate('en', 'definitely.missing.key')).toBe('definitely.missing.key');
  });

  test('resolves system language and keeps autonyms legible', () => {
    expect(resolvedLocale('system', ['ja-JP'])).toBe('ja');
    expect(resolvedLocale('system', ['zh-Hans-CN'])).toBe('zh-CN');
    expect(resolvedLocale('system', ['fr-FR'])).toBe('en');
    expect(resolvedLocale('zh-CN', ['en-US'])).toBe('zh-CN');
    expect(languageLabel('en', 'ja')).toBe('English');
    expect(languageLabel('zh-CN', 'ja')).toBe('简体中文');
    expect(languageLabel('ja', 'ja')).toBe('日本語');
    // `language.system` is translated, so it follows the active locale.
    expect(languageLabel('system', 'zh-CN')).toBe('跟随系统');
    expect(languageLabel('system', 'ja')).toBe('システム');
  });

  test('normalizes persisted values and detects a device language', () => {
    expect(normalizeLanguage('ja')).toBe('ja');
    expect(normalizeLanguage('nonsense')).toBe('system');
    expect(currentLanguage()).toBe('system');
    expect(Array.isArray(deviceLanguages())).toBe(true);
  });
});

import { describe, expect, test } from 'bun:test'

import {
  APP_LANGUAGES,
  interpolateTranslation,
  normalizeLanguage,
  parseRustI18nCatalog,
  resolveLanguage,
  type AppLocale,
} from './i18n-core'

// Shared by the web and mobile clients, so this runs against the same YAML the
// Rust app embeds via `_LOCALE_SOURCES` in `src/lib.rs`. When a locale is added
// there, it has to be added here too.
const LOCALE_FILES = {
  en: 'app.yml',
  'zh-CN': 'zh-CN.yml',
  ja: 'ja.yml',
} as const satisfies Record<AppLocale, string>

const localeRoot = new URL('../../../locales/', import.meta.url)

/**
 * Messages rendered on both sides of an inline control (here, the project
 * selector in the onboarding headline). A language may legitimately leave the
 * prefix empty and carry the whole sentence in the suffix, so these are exempt
 * from the empty-message check. Nothing else may be blank: a blank message
 * renders as nothing at all rather than as a visible gap.
 */
const SPLIT_MESSAGE_PREFIXES = new Set(['onboarding.what_should_we_build_in'])

async function readCatalog(locale: AppLocale) {
  return parseRustI18nCatalog(
    await Bun.file(new URL(LOCALE_FILES[locale], localeRoot)).text(),
    locale,
  )
}

function placeholders(value: string) {
  const msgUnescaped = value.startsWith('"') ? (JSON.parse(value) as string) : value
  return [...msgUnescaped.matchAll(/%\{([A-Za-z0-9_]+)\}/g)]
    .map((match) => match[1]!)
    .sort()
}

describe('shared i18n core', () => {
  test('shipped locales cover exactly the declared languages', () => {
    // The fallback locale is `en` in both `rust_i18n::i18n!` and the client
    // catalogs, so it must list every language the app lets a user pick.
    expect(Object.keys(LOCALE_FILES).sort()).toEqual(
      [...APP_LANGUAGES].filter((language) => language !== 'system').sort(),
    )
  })

  test('keeps every translated catalog in sync with English', async () => {
    const english = await readCatalog('en')
    expect(Object.keys(english).length).toBeGreaterThan(0)

    for (const locale of Object.keys(LOCALE_FILES) as AppLocale[]) {
      if (locale === 'en') continue
      const translated = await readCatalog(locale)
      expect(Object.keys(translated).sort()).toEqual(Object.keys(english).sort())
    }
  })

  test('no catalog ships an empty message', async () => {
    for (const locale of Object.keys(LOCALE_FILES) as AppLocale[]) {
      const catalog = await readCatalog(locale)
      const empty = Object.keys(catalog).filter(
        (key) => catalog[key]!.trim() === '' && !SPLIT_MESSAGE_PREFIXES.has(key),
      )
      expect({ locale, empty }).toEqual({ locale, empty: [] })
    }
  })

  test('exempt split-message prefixes stay non-empty English', async () => {
    // The exemption above is for languages that relocate the whole sentence,
    // never for English dropping copy.
    const english = await readCatalog('en')
    for (const key of SPLIT_MESSAGE_PREFIXES) {
      expect(english[key]?.trim()).toBeTruthy()
    }
  })

  test('translations keep the placeholders their English message declares', async () => {
    // A dropped `%{turn}` silently renders the placeholder instead of the
    // value, so treat the interpolation contract as part of the parity check.
    const english = await readCatalog('en')

    for (const locale of Object.keys(LOCALE_FILES) as AppLocale[]) {
      if (locale === 'en') continue
      const translated = await readCatalog(locale)
      const drifted: string[] = []
      for (const key of Object.keys(english)) {
        const expected = placeholders(english[key]!)
        const actual = placeholders(translated[key] ?? '')
        if (expected.join(',') !== actual.join(',')) {
          drifted.push(`${key}: expected [${expected}] but got [${actual}]`)
        }
      }
      expect(drifted).toEqual([])
    }
  })

  test('parses the nested English catalog and flat translated catalogs', () => {
    expect(parseRustI18nCatalog(`
_version: 2
plain:
  en: Plain text
quoted:
  en: "Line one\\nLine two: %{count}"
`, 'en')).toEqual({
      plain: 'Plain text',
      quoted: 'Line one\nLine two: %{count}',
    })
    expect(parseRustI18nCatalog(`
_version: 1
plain: 简体中文
quoted: "值：%{count}"
`, 'zh-CN')).toEqual({ plain: '简体中文', quoted: '值：%{count}' })
  })

  test('matches the desktop system-language resolution', () => {
    expect(resolveLanguage('system', ['zh-Hans-SG', 'en-US'])).toBe('zh-CN')
    expect(resolveLanguage('system', ['zh-Hant-TW'])).toBe('en')
    expect(resolveLanguage('system', ['zh-Hant-TW', 'ja-JP'])).toBe('ja')
    expect(resolveLanguage('system', ['ja-JP'])).toBe('ja')
    expect(resolveLanguage('system', ['fr-FR'])).toBe('en')
    // A preferred English must not fall through to a later Japanese entry.
    expect(resolveLanguage('system', ['en-US', 'ja-JP'])).toBe('en')
    expect(resolveLanguage('zh-CN', ['en-US'])).toBe('zh-CN')
  })

  test('normalizes persisted values and interpolates named parameters', () => {
    expect(normalizeLanguage('ja')).toBe('ja')
    expect(normalizeLanguage('invalid')).toBe('system')
    expect(interpolateTranslation('%{count} files in %{project}', {
      count: 2,
      project: 'waku',
    })).toBe('2 files in waku')
  })
})

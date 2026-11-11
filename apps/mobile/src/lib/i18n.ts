/**
 * Mobile translation runtime.
 *
 * The catalogs themselves are generated into `i18n-catalog.ts` from
 * `locales/*.yml`; the language/locale resolution logic is shared with the web
 * client through `@waku/i18n` so both agree with the Rust app.
 */
import AsyncStorage from '@react-native-async-storage/async-storage';
import {
  interpolateTranslation,
  normalizeLanguage,
  resolveLanguage,
  type AppLanguage,
  type AppLocale,
  type TranslationParams,
} from '@waku/i18n';

import { MOBILE_CATALOG } from './i18n-catalog';

export type { AppLanguage, AppLocale, TranslationParams };
export { APP_LANGUAGES, normalizeLanguage } from '@waku/i18n';

const LANGUAGE_KEY = 'waku.language';

/**
 * Same key the web client uses, so the preference survives moving between the
 * two clients on one machine and one code path documents the value.
 */
export const LANGUAGE_STORAGE_KEY = LANGUAGE_KEY;

let current: AppLanguage = 'system';
const listeners = new Set<() => void>();

export function translate(
  locale: AppLocale,
  key: string,
  params?: TranslationParams,
): string {
  const value = MOBILE_CATALOG[locale][key] ?? MOBILE_CATALOG.en[key] ?? key;
  return interpolateTranslation(value, params);
}

/** Autonyms, so the selector stays legible in an unfamiliar locale. */
export function languageLabel(language: AppLanguage, locale: AppLocale): string {
  switch (language) {
    case 'system': return translate(locale, 'language.system');
    case 'en': return 'English';
    case 'zh-CN': return '简体中文';
    case 'ja': return '日本語';
  }
}

/**
 * Device languages, most-preferred first. Hermes ships `Intl` without locale
 * negotiation, so read the platform list rather than calling `Intl.Locale`.
 */
export function deviceLanguages(): string[] {
  // Hermes ships `Intl` without locale negotiation, and `Intl.Locale` throws
  // without an argument, so derive the tag defensively and fall back to `en`.
  try {
    const resolved = Intl.DateTimeFormat().resolvedOptions().locale;
    if (resolved) return [resolved];
  } catch {
    // Runtime without Intl.DateTimeFormat; fall through.
  }
  return ['en'];
}

export function resolvedLocale(language: AppLanguage, preferred = deviceLanguages()): AppLocale {
  return resolveLanguage(language, preferred);
}

export function currentLanguage(): AppLanguage {
  return current;
}

export function subscribeLanguage(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export async function loadStoredLanguage(): Promise<AppLanguage> {
  try {
    return normalizeLanguage(await AsyncStorage.getItem(LANGUAGE_KEY));
  } catch {
    return 'system';
  }
}

export async function setStoredLanguage(language: AppLanguage): Promise<void> {
  current = language;
  for (const listener of listeners) listener();
  try {
    await AsyncStorage.setItem(LANGUAGE_KEY, language);
  } catch {
    // The preference is cosmetic state; a failed write must not break the UI.
  }
}

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from 'react';

import {
  currentLanguage,
  deviceLanguages,
  languageLabel,
  loadStoredLanguage,
  resolvedLocale,
  setStoredLanguage,
  subscribeLanguage,
  translate,
  type AppLanguage,
  type AppLocale,
  type TranslationParams,
} from './i18n';

export type Translator = (key: string, params?: TranslationParams) => string;

interface I18nValue {
  language: AppLanguage;
  locale: AppLocale;
  t: Translator;
  setLanguage: (language: AppLanguage) => void;
  languageLabel: (language: AppLanguage) => string;
}

const I18nContext = createContext<I18nValue | null>(null);

/**
 * Loads the stored preference once, then keeps the tree in sync with it. The
 * provider sits above the navigator so screen `title` options and accessibility
 * labels resolve in the active language too.
 */
export function I18nProvider({ children }: { children: ReactNode }) {
  const [language, setLanguageState] = useState<AppLanguage>(currentLanguage);
  const [device, setDevice] = useState<string[]>(deviceLanguages);

  useEffect(() => {
    void loadStoredLanguage().then((stored) => {
      setLanguageState(stored);
      // Re-detect here: the platform list is only reliable after the bridge is
      // up, so a value read during render can lag behind a language change.
      setDevice(deviceLanguages());
    });
    return subscribeLanguage(() => setLanguageState(currentLanguage()));
  }, []);

  const setLanguage = useCallback((next: AppLanguage) => {
    setLanguageState(next);
    void setStoredLanguage(next);
  }, []);

  const value = useMemo<I18nValue>(() => {
    const locale = resolvedLocale(language, device);
    return {
      language,
      locale,
      t: (key, params) => translate(locale, key, params),
      setLanguage,
      languageLabel: (target) => languageLabel(target, locale),
    };
  }, [device, language, setLanguage]);

  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>;
}

export function useI18n(): I18nValue {
  const value = useContext(I18nContext);
  if (!value) throw new Error('useI18n must be used inside I18nProvider');
  return value;
}

/**
 * Translation without a provider, for helpers that run outside the tree
 * (error formatters, non-component modules). It reads the module-level
 * language, so it follows the selector without subscribing to it.
 */
export function translateNow(key: string, params?: TranslationParams): string {
  return translate(resolvedLocale(currentLanguage()), key, params);
}

//! Shared locale selection and translation access.

use serde::{Deserialize, Serialize};

/// The language preference Waku persists. `System` resolves to one of the
/// locales Waku deliberately ships today.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum AppLanguage {
    #[default]
    System,
    English,
    SimplifiedChinese,
    Japanese,
}

impl AppLanguage {
    pub const ALL: [Self; 4] = [
        Self::System,
        Self::English,
        Self::SimplifiedChinese,
        Self::Japanese,
    ];

    pub fn locale(self) -> &'static str {
        match self.resolved() {
            Self::System => unreachable!("system language always resolves to a shipped locale"),
            Self::English => "en",
            Self::SimplifiedChinese => "zh-CN",
            Self::Japanese => "ja",
        }
    }

    /// Explicit language names are autonyms so the selector remains
    /// understandable even when the current locale is unfamiliar.
    pub fn label(self) -> String {
        match self {
            Self::System => translate("language.system"),
            Self::English => "English".to_owned(),
            Self::SimplifiedChinese => "简体中文".to_owned(),
            Self::Japanese => "日本語".to_owned(),
        }
    }

    pub fn resolved(self) -> Self {
        match self {
            Self::System => Self::from_system(),
            explicit => explicit,
        }
    }

    fn from_system() -> Self {
        Self::from_preferred_locale_ids(preferred_locale_ids())
    }

    /// The first preferred language Waku ships. A language Waku does not ship
    /// earlier in the list does not stop the search, which is how the shared
    /// web and mobile resolvers in `packages/waku-i18n` read the same list: a
    /// system preferring `["fr-FR", "zh-Hans-CN"]` reads Chinese, not English.
    fn from_preferred_locale_ids<I, S>(locales: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        locales
            .into_iter()
            .find_map(|locale| Self::matched_locale_id(locale.as_ref()))
            .unwrap_or(Self::English)
    }

    fn matched_locale_id(locale: &str) -> Option<Self> {
        let locale = locale.replace('_', "-").to_ascii_lowercase();
        if locale == "zh-cn" || locale == "zh-sg" || locale.starts_with("zh-hans") {
            Some(Self::SimplifiedChinese)
        } else if locale == "ja" || locale.starts_with("ja-") {
            Some(Self::Japanese)
        } else if locale == "en" || locale.starts_with("en-") {
            // English is a shipped language like any other, so a system that
            // prefers it must not fall through to a later Japanese entry.
            Some(Self::English)
        } else {
            None
        }
    }

    #[cfg(test)]
    fn from_locale_id(locale: &str) -> Self {
        Self::matched_locale_id(locale).unwrap_or(Self::English)
    }
}

pub fn set_language(language: AppLanguage) {
    rust_i18n::set_locale(language.locale());
}

pub fn translate(key: &str) -> String {
    rust_i18n::t!(key).into_owned()
}

pub fn uses_east_asian_date_format() -> bool {
    locale_uses_east_asian_date_format(&rust_i18n::locale())
}

fn locale_uses_east_asian_date_format(locale: &str) -> bool {
    matches!(locale, "zh-CN" | "ja")
}

/// The locales the operating system prefers, most preferred first. Waku reads
/// the first one it ships; an empty list means the system reported nothing.
#[cfg(target_os = "macos")]
fn preferred_locale_ids() -> Vec<String> {
    use objc2_foundation::NSLocale;

    NSLocale::preferredLanguages()
        .iter()
        .map(|locale| locale.to_string())
        .collect()
}

/// Windows never sets `LANG`, so a Unix-style environment lookup leaves a
/// Chinese Windows showing English. Ask the list the display-language picker
/// writes, and fall back to the language the UI itself runs in when that list
/// comes back empty.
#[cfg(target_os = "windows")]
fn preferred_locale_ids() -> Vec<String> {
    let preferred = preferred_ui_languages();
    if preferred.is_empty() {
        return default_ui_language().into_iter().collect();
    }
    preferred
}

#[cfg(target_os = "windows")]
fn preferred_ui_languages() -> Vec<String> {
    use windows_sys::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};

    // Only a sizing call that passes no buffer knows how long the list is,
    // because a language name is variable length.
    let mut languages = 0u32;
    let mut characters = 0u32;
    // SAFETY: a null buffer makes the call report the required length without
    // writing anywhere, and both out parameters are valid for the call.
    let sized = unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut languages,
            std::ptr::null_mut(),
            &mut characters,
        )
    };
    if sized == 0 || characters == 0 {
        return Vec::new();
    }

    let mut buffer = vec![0u16; characters as usize];
    // SAFETY: `buffer` holds exactly the length the sizing call reported;
    // `characters` counts UTF-16 code units, terminator included.
    let filled = unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut languages,
            buffer.as_mut_ptr(),
            &mut characters,
        )
    };
    if filled == 0 {
        return Vec::new();
    }

    // The names are null separated and closed by a second null; a short list
    // leaves the tail of the buffer zeroed, so skip empty names.
    buffer
        .split(|unit| *unit == 0)
        .filter(|name| !name.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// The language Windows runs its own UI in, which is what "follow the system"
/// means even when the preferred list is unavailable.
#[cfg(target_os = "windows")]
fn default_ui_language() -> Option<String> {
    use windows_sys::Win32::Globalization::{GetUserDefaultUILanguage, LCIDToLocaleName};

    /// `LOCALE_NAME_MAX_LENGTH`, the longest name `LCIDToLocaleName` accepts.
    const LOCALE_NAME_MAX_LENGTH: usize = 85;

    // A neutral sort order leaves the LCID equal to the LANGID the call wants.
    // SAFETY: the call reads no arguments and only returns a language id.
    let language_id = unsafe { GetUserDefaultUILanguage() };
    let mut buffer = [0u16; LOCALE_NAME_MAX_LENGTH];
    // SAFETY: `buffer` is the maximum number of UTF-16 code units the call
    // documents, and `written` is checked before the buffer is read.
    let written = unsafe {
        LCIDToLocaleName(
            language_id.into(),
            buffer.as_mut_ptr(),
            buffer.len() as i32,
            0,
        )
    };
    if written <= 1 {
        return None;
    }
    // `written` counts the null terminator.
    Some(String::from_utf16_lossy(&buffer[..written as usize - 1]))
}

/// Every other platform reports the locale through the POSIX environment.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn preferred_locale_ids() -> Vec<String> {
    std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LC_MESSAGES"))
        .or_else(|_| std::env::var("LANG"))
        .map(|value| value.split('.').next().unwrap_or_default().to_owned())
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_locale_ids_are_supported() {
        assert_eq!(AppLanguage::English.locale(), "en");
        assert_eq!(AppLanguage::SimplifiedChinese.locale(), "zh-CN");
        assert_eq!(AppLanguage::Japanese.locale(), "ja");
        let locales = rust_i18n::available_locales!();
        assert_eq!(locales.len(), 3);
        assert!(locales.iter().any(|locale| locale.as_ref() == "en"));
        assert!(locales.iter().any(|locale| locale.as_ref() == "zh-CN"));
        assert!(locales.iter().any(|locale| locale.as_ref() == "ja"));
    }

    #[test]
    fn language_names_are_autonyms() {
        assert_eq!(AppLanguage::English.label(), "English");
        assert_eq!(AppLanguage::SimplifiedChinese.label(), "简体中文");
        assert_eq!(AppLanguage::Japanese.label(), "日本語");
    }

    #[test]
    fn system_is_the_default_persisted_preference_and_resolves_to_a_shipped_locale() {
        assert_eq!(AppLanguage::default(), AppLanguage::System);
        assert_eq!(
            serde_json::to_string(&AppLanguage::System).unwrap(),
            r#""system""#
        );
        assert!(matches!(
            AppLanguage::System.locale(),
            "en" | "zh-CN" | "ja"
        ));
    }

    #[test]
    fn japanese_system_locales_are_detected() {
        assert_eq!(AppLanguage::from_locale_id("ja"), AppLanguage::Japanese);
        assert_eq!(AppLanguage::from_locale_id("ja_JP"), AppLanguage::Japanese);
    }

    #[test]
    fn japanese_and_simplified_chinese_use_east_asian_dates() {
        assert!(locale_uses_east_asian_date_format("ja"));
        assert!(locale_uses_east_asian_date_format("zh-CN"));
        assert!(!locale_uses_east_asian_date_format("en"));
    }

    #[test]
    fn simplified_chinese_system_locales_are_detected_without_enabling_traditional_chinese() {
        assert_eq!(
            AppLanguage::from_locale_id("zh-Hans-CN"),
            AppLanguage::SimplifiedChinese
        );
        assert_eq!(
            AppLanguage::from_locale_id("zh_SG"),
            AppLanguage::SimplifiedChinese
        );
        assert_eq!(
            AppLanguage::from_locale_id("zh-Hant-TW"),
            AppLanguage::English
        );
    }

    #[test]
    fn the_first_shipped_preferred_language_wins() {
        assert_eq!(
            AppLanguage::from_preferred_locale_ids(["fr-FR", "zh-Hans-CN", "en-US"]),
            AppLanguage::SimplifiedChinese
        );
        assert_eq!(
            AppLanguage::from_preferred_locale_ids(["zh-Hant-TW", "ja-JP"]),
            AppLanguage::Japanese
        );
        assert_eq!(
            AppLanguage::from_preferred_locale_ids(["en-US", "ja-JP"]),
            AppLanguage::English
        );
        assert_eq!(
            AppLanguage::from_preferred_locale_ids(Vec::<String>::new()),
            AppLanguage::English
        );
    }

    /// Windows reports its languages through Win32 rather than the environment,
    /// so an empty list here is exactly the Chinese-system-reads-English bug.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_reports_a_preferred_ui_language() {
        assert!(!preferred_locale_ids().is_empty());
    }

    #[test]
    fn translations_are_complete_and_interpolate_naturally() {
        assert_eq!(&*rust_i18n::t!("settings.daemon", locale = "en"), "Daemon");
        assert_eq!(
            &*rust_i18n::t!("daemon.expose_title", locale = "en"),
            "Expose managed daemon"
        );
        assert_eq!(
            &*rust_i18n::t!("settings.general", locale = "zh-CN"),
            "通用"
        );
        assert_eq!(
            &*rust_i18n::t!(
                "computer_use.allow_control",
                locale = "zh-CN",
                app = "Finder"
            ),
            "允许 Waku 控制“Finder”吗？"
        );
        assert_eq!(
            &*rust_i18n::t!("session.rewound", locale = "zh-CN", turn = 3),
            "已回退到第 3 轮任务之前"
        );
        assert_eq!(&*rust_i18n::t!("settings.general", locale = "ja"), "一般");
        assert_eq!(
            &*rust_i18n::t!("computer_use.allow_control", locale = "ja", app = "Finder"),
            "Waku に「Finder」の操作を許可しますか？"
        );
        assert_eq!(
            &*rust_i18n::t!("session.rewound", locale = "ja", turn = 3),
            "タスクをターン 3 の前まで巻き戻しました"
        );
    }

    #[test]
    fn task_creation_copy_uses_task_terminology() {
        assert_eq!(&*rust_i18n::t!("menu.new_task", locale = "en"), "New Task");
        assert_eq!(
            &*rust_i18n::t!("command_palette.new_task", locale = "en"),
            "New task"
        );
        assert_eq!(
            &*rust_i18n::t!("providers.disabled_for_new_tasks", locale = "en"),
            "Disabled for new tasks"
        );
    }
}

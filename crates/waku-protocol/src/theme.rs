//! Process-neutral appearance settings persisted in the desktop settings file.
//!
//! Appearance is two independent choices, not one:
//!
//! - **`ColorMode`** is light-or-dark. It is what the native window chrome
//!   follows (AppKit appearance, traffic lights, menus), and it is the axis the
//!   system setting can take over.
//! - **`PaletteId`** is which set of colors. It is a free-form id rather than an
//!   enum so a new theme costs one id and one `Theme` literal — the protocol
//!   does not change, and the three clients do not need a lockstep release.
//!
//! The two were one `ThemePreference` enum before. That made every theme a new
//! protocol variant and, worse, forced a colored palette to also be a *dark*
//! palette: there was nowhere to say "this theme, but in its light form".

use serde::{Deserialize, Serialize};

/// Whether the app paints light or dark, and where that decision comes from.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorMode {
    /// Follow the operating system, and keep following it as it changes.
    #[default]
    System,
    Light,
    Dark,
}

impl ColorMode {
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn label(self) -> String {
        match self {
            Self::System => crate::i18n::translate("settings.theme_system"),
            Self::Light => crate::i18n::translate("settings.theme_light"),
            Self::Dark => crate::i18n::translate("settings.theme_dark"),
        }
    }

    /// The appearance the window chrome should force, or `None` to follow the
    /// system. Chrome and canvas must agree: a dark canvas under a light
    /// titlebar reads as a rendering bug.
    pub fn native_override(self) -> Option<bool> {
        match self {
            Self::System => None,
            Self::Light => Some(false),
            Self::Dark => Some(true),
        }
    }

    pub fn resolves_to_dark(self, system_is_dark: bool) -> bool {
        match self {
            Self::System => system_is_dark,
            Self::Light => false,
            Self::Dark => true,
        }
    }
}

/// Which palette, by stable id.
///
/// A newtype rather than a bare `String` so it cannot be confused with any
/// other settings string, and so the one fallback rule lives in one place: an
/// id from a newer build — or a palette that was removed — resolves to the
/// default instead of failing to load the settings file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PaletteId(String);

impl PaletteId {
    /// The palette every build has. Also the fallback for an unknown id.
    pub const DEFAULT: &'static str = "graphite";

    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this settings file names a palette this build knows about.
    /// Callers fall back to [`Self::DEFAULT`] when it does not.
    pub fn is_known(&self, known: impl IntoIterator<Item = &'static str>) -> bool {
        let mut known = known.into_iter();
        known.any(|candidate| candidate == self.0)
    }

    pub fn or_default(self, known: impl IntoIterator<Item = &'static str>) -> Self {
        if self.is_known(known) {
            self
        } else {
            Self(Self::DEFAULT.to_owned())
        }
    }
}

impl Default for PaletteId {
    fn default() -> Self {
        Self(Self::DEFAULT.to_owned())
    }
}

/// Every palette id this build can resolve.
///
/// Lives beside the type rather than in the desktop crate because the settings
/// loader — in `waku-client`, which the desktop app depends on — has to
/// validate an id read from disk, and it must not depend on the UI crate to do
/// it. A new palette adds its id here and its `Theme` literal in the desktop
/// `theme` module; nothing else changes.
pub const KNOWN_PALETTES: [&str; 2] = ["graphite", "one"];

use gpui::{App, Global, Hsla, Rems, Window, WindowAppearance, hsla, rems, rgb, transparent_black};

pub use waku_client::theme::{ColorMode, PaletteId};

/// Scaled pixels: a dimension authored at the default 14px UI font size,
/// expressed in rems so the UI font size setting scales it. The window's rem
/// size *is* the UI font size, so at the default setting this resolves to
/// exactly the authored pixel value.
///
/// Chrome text sizes and their line heights go through here. Content surfaces
/// that already derive from a font-size setting — markdown metrics, the file
/// editor, diff rows, tool-output mono — stay in `px` so they never scale
/// twice.
pub fn sp(value: f32) -> Rems {
    rems(value / waku_client::persistence::DEFAULT_UI_FONT_SIZE)
}

/// Waku's visual language, take two: neutral graphite surfaces in the spirit
/// of Cursor — color is reserved for meaning. On macOS the sidebar's semantic
/// tint is installed as a native layer above Sidebar vibrancy; keeping this
/// GPUI surface clear avoids incorrectly accumulating the alpha of nested Metal
/// backgrounds. Selected, hovered, and pressed rows remain a 6% neutral layer.
#[derive(Clone, Copy)]
pub struct Theme {
    pub is_dark: bool,
    pub canvas: Hsla,
    pub sidebar: Hsla,
    pub sidebar_drag_background: Hsla,
    pub sidebar_item_background: Hsla,
    pub surface: Hsla,
    pub raised: Hsla,
    pub composer: Hsla,
    pub inset: Hsla,
    /// Terminal screen surface: paper-white in light mode, near-black in dark.
    pub terminal: Hsla,
    pub overlay: Hsla,
    pub overlay_strong: Hsla,

    pub border: Hsla,
    pub border_strong: Hsla,
    pub sidebar_border: Hsla,

    pub text: Hsla,
    pub text_secondary: Hsla,
    pub text_tertiary: Hsla,
    pub text_ghost: Hsla,

    /// Brand coral. Logo, caret, live-activity pulses — nothing structural.
    pub accent: Hsla,
    pub resize_handle: Hsla,
    /// Meter fills in the usage panel. Quota-meter blue by convention;
    /// warning/danger take over as a lane fills.
    pub gauge: Hsla,

    /// Text-selection wash. Painted *under* the glyphs, so it stays
    /// translucent and deliberately reads as the familiar browser blue rather
    /// than as brand color.
    pub selection: Hsla,
    /// Inline `code` foreground and its rounded wash.
    pub code_text: Hsla,
    pub code_wash: Hsla,

    /// Light fill for primary buttons (send, allow), dark glyph on top.
    pub inverse: Hsla,
    pub on_inverse: Hsla,

    pub warning: Hsla,
    pub success: Hsla,
    pub favorite: Hsla,
    pub danger: Hsla,
    pub danger_soft: Hsla,
}

impl Theme {
    pub fn current(cx: &App) -> Self {
        if cx.has_global::<ActiveWakuTheme>() {
            cx.global::<ActiveWakuTheme>().0
        } else {
            Self::dark()
        }
    }

    /// Graphite dark: the neutral baseline every other palette is read
    /// against, and the fallback before the settings file has been read.
    pub fn dark() -> Self {
        palette::graphite(true)
    }
}

/// What a palette has to supply.
///
/// `Theme` is the token set — canvas, surface, four text weights, three
/// borders, semantic accents — and it is deliberately not re-shaped per
/// palette: every one of the ~1274 color reads in the app names a `Theme`
/// field, so the struct cannot change without touching all of them. A palette
/// is therefore just "one `Theme` literal per color mode", and
/// [`Theme::dark`] stays as the graphite shortcut callers already use.
pub mod palette {
    use super::*;

    /// Where the selected id and mode live, so [`Theme::current`] and the
    /// settings page read one source.
    #[derive(Clone, Copy)]
    pub struct ActiveWakuTheme(pub Theme);

    /// Build a palette for `mode`, or the default when the id is unknown.
    ///
    /// Waku's own palettes come in a dark and a light form and follow the
    /// appearance setting. The Winc palettes do not: `winc-night` is a dark
    /// palette and `winc-day` a light one, each on its own terms, which is how
    /// Winc ships them. Inverting one to satisfy "two forms" would invent a
    /// palette nobody designed, so a single-form palette answers with itself in
    /// both modes and `Theme::is_dark` reports what it actually is. The
    /// appearance setting still chooses between *palettes*; it just cannot
    /// repaint one of these into the other.
    pub fn resolve(id: &PaletteId, dark: bool) -> Theme {
        match (id.as_str(), dark) {
            ("graphite", dark) => graphite(dark),
            ("one", true) => one_dark(),
            ("one", false) => one_light(),
            ("winc-night", _) => winc_night(),
            ("winc-slate", _) => winc_slate(),
            ("winc-day", _) => winc_day(),
            ("winc-paper", _) => winc_paper(),
            // An id from a newer build, or one that was removed: the default
            // palette, never a failure to start.
            _ => graphite(dark),
        }
    }

    /// Whether `id` has a form for each appearance, so the settings page can say
    /// so instead of offering a choice that does nothing.
    pub fn follows_appearance(id: &str) -> bool {
        !matches!(id, "winc-night" | "winc-slate" | "winc-day" | "winc-paper")
    }

    pub fn label(id: &str) -> String {
        match id {
            "one" => "One".to_owned(),
            "winc-night" => "Winc 夜航".to_owned(),
            "winc-slate" => "Winc 远山".to_owned(),
            "winc-day" => "Winc 晴昼".to_owned(),
            "winc-paper" => "Winc 宣纸".to_owned(),
            _ => "Graphite".to_owned(),
        }
    }

    pub fn graphite(dark: bool) -> Theme {
        if dark {
            graphite_dark()
        } else {
            graphite_light()
        }
    }

    fn graphite_dark() -> Theme {
        Theme {
            is_dark: true,
            canvas: rgb(0x1A1A1A).into(),
            sidebar: if cfg!(target_os = "macos") {
                transparent_black()
            } else {
                rgb(0x181818).into()
            },
            sidebar_drag_background: rgb(0x181818).into(),
            sidebar_item_background: hsla(0.0, 0.0, 0.941, 0.06),
            surface: rgb(0x1A1A1A).into(),
            raised: rgb(0x232323).into(),
            composer: rgb(0x212121).into(),
            inset: rgb(0x151515).into(),
            terminal: rgb(0x151515).into(),
            overlay: hsla(220.0 / 360.0, 0.10, 0.90, 0.05),
            overlay_strong: hsla(220.0 / 360.0, 0.10, 0.90, 0.09),

            border: hsla(220.0 / 360.0, 0.10, 0.90, 0.07),
            border_strong: hsla(220.0 / 360.0, 0.10, 0.90, 0.14),
            sidebar_border: hsla(126.93 / 360.0, 0.000_000_1, 0.16077, 1.0),

            text: rgb(0xE2E2E2).into(),
            text_secondary: rgb(0xA3A3A3).into(),
            text_tertiary: rgb(0x7D7D7D).into(),
            text_ghost: rgb(0x575757).into(),

            accent: rgb(0xE2795B).into(),
            resize_handle: rgb(0x3B82F6).into(),
            gauge: rgb(0x3B82F6).into(),

            selection: hsla(211.0 / 360.0, 1.0, 0.50, 0.55),
            code_text: rgb(0xE0A882).into(),
            code_wash: hsla(220.0 / 360.0, 0.10, 0.90, 0.08),

            inverse: rgb(0xE7E9EC).into(),
            on_inverse: rgb(0x17181C).into(),

            warning: rgb(0xE0B36A).into(),
            success: rgb(0x62C987).into(),
            favorite: rgb(0xEAB308).into(),
            danger: rgb(0xE2726A).into(),
            danger_soft: hsla(4.0 / 360.0, 0.55, 0.63, 0.10),
        }
    }

    fn graphite_light() -> Theme {
        Theme {
            is_dark: false,
            canvas: rgb(0xF6F5F6).into(),
            sidebar: if cfg!(target_os = "macos") {
                transparent_black()
            } else {
                rgb(0xF3F3F3).into()
            },
            sidebar_drag_background: rgb(0xF3F3F3).into(),
            sidebar_item_background: hsla(0.0, 0.0, 0.078, 0.06),
            surface: rgb(0xF6F5F6).into(),
            raised: rgb(0xECECEC).into(),
            composer: rgb(0xFFFFFF).into(),
            inset: rgb(0xE6E6E6).into(),
            terminal: rgb(0xFFFFFF).into(),
            overlay: hsla(220.0 / 360.0, 0.10, 0.12, 0.05),
            overlay_strong: hsla(220.0 / 360.0, 0.10, 0.12, 0.09),

            border: hsla(220.0 / 360.0, 0.10, 0.12, 0.08),
            border_strong: hsla(220.0 / 360.0, 0.10, 0.12, 0.15),
            sidebar_border: hsla(0.0, 0.0, 0.078, 0.12),

            text: rgb(0x242424).into(),
            text_secondary: rgb(0x666666).into(),
            text_tertiary: rgb(0x858585).into(),
            text_ghost: rgb(0xA4A4A4).into(),

            accent: rgb(0xC85F44).into(),
            resize_handle: rgb(0x2563EB).into(),
            gauge: rgb(0x2563EB).into(),

            selection: hsla(211.0 / 360.0, 1.0, 0.50, 0.35),
            code_text: rgb(0x9A5528).into(),
            code_wash: hsla(220.0 / 360.0, 0.10, 0.12, 0.07),

            inverse: rgb(0x202227).into(),
            on_inverse: rgb(0xF8F8F9).into(),

            warning: rgb(0xA66B20).into(),
            success: rgb(0x2F8F52).into(),
            favorite: rgb(0xCA8A04).into(),
            danger: rgb(0xC64A42).into(),
            danger_soft: hsla(4.0 / 360.0, 0.55, 0.52, 0.10),
        }
    }

    /// One Dark. Atom's palette, which most editors can be mistaken for, so it
    /// costs a reader nothing to recognize.
    ///
    /// Contrast is held to the same floor as graphite rather than copied from
    /// the original: One Dark's `#5c6370` comment grey is about 2.6:1 on
    /// `#282c34`, which fails AA for the secondary text this app uses it for.
    fn one_dark() -> Theme {
        Theme {
            is_dark: true,
            canvas: rgb(0x282C34).into(),
            sidebar: if cfg!(target_os = "macos") {
                transparent_black()
            } else {
                rgb(0x21252B).into()
            },
            sidebar_drag_background: rgb(0x21252B).into(),
            sidebar_item_background: hsla(220.0 / 360.0, 0.13, 0.85, 0.07),
            surface: rgb(0x282C34).into(),
            raised: rgb(0x2F343E).into(),
            composer: rgb(0x2C313A).into(),
            inset: rgb(0x21252B).into(),
            terminal: rgb(0x21252B).into(),
            overlay: hsla(220.0 / 360.0, 0.13, 0.85, 0.06),
            overlay_strong: hsla(220.0 / 360.0, 0.13, 0.85, 0.11),

            border: hsla(220.0 / 360.0, 0.13, 0.85, 0.09),
            border_strong: hsla(220.0 / 360.0, 0.13, 0.85, 0.17),
            sidebar_border: hsla(220.0 / 360.0, 0.13, 0.60, 0.55),

            text: rgb(0xDCDFE4).into(),
            text_secondary: rgb(0x9DA5B4).into(),
            text_tertiary: rgb(0x7F8796).into(),
            text_ghost: rgb(0x5C6370).into(),

            accent: rgb(0x61AFEF).into(),
            resize_handle: rgb(0x61AFEF).into(),
            gauge: rgb(0x61AFEF).into(),

            selection: hsla(207.0 / 360.0, 0.74, 0.60, 0.35),
            code_text: rgb(0xE5C07B).into(),
            code_wash: hsla(220.0 / 360.0, 0.13, 0.85, 0.09),

            inverse: rgb(0xDCDFE4).into(),
            on_inverse: rgb(0x21252B).into(),

            warning: rgb(0xE5C07B).into(),
            success: rgb(0x98C379).into(),
            favorite: rgb(0xE5C07B).into(),
            danger: rgb(0xE06C75).into(),
            danger_soft: hsla(355.0 / 360.0, 0.65, 0.65, 0.12),
        }
    }

    /// One Light, the same palette read on paper.
    fn one_light() -> Theme {
        Theme {
            is_dark: false,
            canvas: rgb(0xFAFAFA).into(),
            sidebar: if cfg!(target_os = "macos") {
                transparent_black()
            } else {
                rgb(0xF0F0F1).into()
            },
            sidebar_drag_background: rgb(0xF0F0F1).into(),
            sidebar_item_background: hsla(220.0 / 360.0, 0.13, 0.35, 0.07),
            surface: rgb(0xFAFAFA).into(),
            raised: rgb(0xF0F0F1).into(),
            composer: rgb(0xFFFFFF).into(),
            inset: rgb(0xEAEAEB).into(),
            terminal: rgb(0xFFFFFF).into(),
            overlay: hsla(220.0 / 360.0, 0.13, 0.30, 0.05),
            overlay_strong: hsla(220.0 / 360.0, 0.13, 0.30, 0.10),

            border: hsla(220.0 / 360.0, 0.13, 0.30, 0.10),
            border_strong: hsla(220.0 / 360.0, 0.13, 0.30, 0.18),
            sidebar_border: hsla(220.0 / 360.0, 0.13, 0.30, 0.16),

            text: rgb(0x383A42).into(),
            text_secondary: rgb(0x696C77).into(),
            text_tertiary: rgb(0x8B8E99).into(),
            text_ghost: rgb(0xA0A1A7).into(),

            accent: rgb(0x4078F2).into(),
            resize_handle: rgb(0x4078F2).into(),
            gauge: rgb(0x4078F2).into(),

            selection: hsla(213.0 / 360.0, 0.85, 0.60, 0.28),
            code_text: rgb(0x986801).into(),
            code_wash: hsla(220.0 / 360.0, 0.13, 0.30, 0.07),

            inverse: rgb(0x383A42).into(),
            on_inverse: rgb(0xFAFAFA).into(),

            warning: rgb(0x986801).into(),
            success: rgb(0x3E7A3E).into(),
            favorite: rgb(0xB7791F).into(),
            danger: rgb(0xC0392B).into(),
            danger_soft: hsla(6.0 / 360.0, 0.60, 0.50, 0.10),
        }
    }

    /// The Winc palettes, ported from its own theme module.
    ///
    /// Winc's `Palette` is wider than Waku's `Theme` — it carries per-token
    /// syntax colours and a 16-colour ANSI ramp, which belong to an editor and a
    /// terminal that this program draws differently. Only the colours both
    /// programs have a use for are carried across, mapped by meaning rather than
    /// by position: `frame` is the space around the cards (canvas), `card` is
    /// what those cards are painted in (surface), and `popup` is the raised
    /// layer above them. Copying the literals wholesale would need every field
    /// renamed at each of the ~1274 call sites that read them.
    ///
    /// Contrast is Winc's, measured against its own background and not
    /// re-derived here: it holds body text to 7:1 and accents to 4.5:1, which is
    /// stricter than the palettes above, so these are not adjusted.
    ///
    /// Winc's default dark palette: a blue-grey charcoal, not pure black.
    fn winc_night() -> Theme {
        Theme {
            is_dark: true,
            canvas: rgb(0x15171C).into(),
            sidebar: if cfg!(target_os = "macos") {
                transparent_black()
            } else {
                rgb(0x1B1E24).into()
            },
            sidebar_drag_background: rgb(0x1B1E24).into(),
            sidebar_item_background: hsla(218.0 / 360.0, 0.16, 0.80, 0.06),
            surface: rgb(0x1B1E24).into(),
            raised: rgb(0x23272E).into(),
            composer: rgb(0x23272E).into(),
            inset: rgb(0x121419).into(),
            terminal: rgb(0x15171C).into(),
            overlay: hsla(218.0 / 360.0, 0.16, 0.80, 0.05),
            overlay_strong: hsla(218.0 / 360.0, 0.16, 0.80, 0.09),

            border: hsla(218.0 / 360.0, 0.16, 0.80, 0.08),
            border_strong: hsla(218.0 / 360.0, 0.16, 0.80, 0.15),
            sidebar_border: hsla(218.0 / 360.0, 0.16, 0.80, 0.10),

            text: rgb(0xD4D8DF).into(),
            text_secondary: rgb(0x959CA8).into(),
            text_tertiary: rgb(0x7A8290).into(),
            text_ghost: rgb(0x656C78).into(),

            accent: rgb(0x78AEF2).into(),
            resize_handle: rgb(0x78AEF2).into(),
            gauge: rgb(0x78AEF2).into(),

            selection: rgb(0x2C4468).into(),
            code_text: rgb(0xD7915C).into(),
            code_wash: hsla(218.0 / 360.0, 0.16, 0.80, 0.08),

            inverse: rgb(0xD4D8DF).into(),
            on_inverse: rgb(0x15171C).into(),

            warning: rgb(0xE2BA6A).into(),
            success: rgb(0x5FBFA0).into(),
            favorite: rgb(0xE4CF7A).into(),
            danger: rgb(0xF07F7F).into(),
            danger_soft: hsla(0.0, 0.60, 0.72, 0.12),
        }
    }

    /// Winc's softer dark palette: the same hues one step lighter, for a room
    /// with the lights on.
    fn winc_slate() -> Theme {
        Theme {
            is_dark: true,
            canvas: rgb(0x1E2127).into(),
            sidebar: if cfg!(target_os = "macos") {
                transparent_black()
            } else {
                rgb(0x252930).into()
            },
            sidebar_drag_background: rgb(0x252930).into(),
            sidebar_item_background: hsla(218.0 / 360.0, 0.16, 0.80, 0.06),
            surface: rgb(0x252930).into(),
            raised: rgb(0x2C3038).into(),
            composer: rgb(0x2C3038).into(),
            inset: rgb(0x1A1D22).into(),
            terminal: rgb(0x1E2127).into(),
            overlay: hsla(218.0 / 360.0, 0.16, 0.80, 0.05),
            overlay_strong: hsla(218.0 / 360.0, 0.16, 0.80, 0.09),

            border: hsla(218.0 / 360.0, 0.16, 0.80, 0.08),
            border_strong: hsla(218.0 / 360.0, 0.16, 0.80, 0.15),
            sidebar_border: hsla(218.0 / 360.0, 0.16, 0.80, 0.10),

            text: rgb(0xCED3DB).into(),
            text_secondary: rgb(0x99A0AC).into(),
            text_tertiary: rgb(0x808896).into(),
            text_ghost: rgb(0x6C7380).into(),

            accent: rgb(0x7DB2F2).into(),
            resize_handle: rgb(0x7DB2F2).into(),
            gauge: rgb(0x7DB2F2).into(),

            selection: rgb(0x35496B).into(),
            code_text: rgb(0xDB9763).into(),
            code_wash: hsla(218.0 / 360.0, 0.16, 0.80, 0.08),

            inverse: rgb(0xCED3DB).into(),
            on_inverse: rgb(0x1E2127).into(),

            warning: rgb(0xE2BD70).into(),
            success: rgb(0x66C2A5).into(),
            favorite: rgb(0xE6D282).into(),
            danger: rgb(0xF08585).into(),
            danger_soft: hsla(0.0, 0.60, 0.73, 0.12),
        }
    }

    /// Winc's default light palette: near-white with a hint of grey, so a bright
    /// screen does not glare.
    fn winc_day() -> Theme {
        Theme {
            is_dark: false,
            canvas: rgb(0xE9EBEF).into(),
            sidebar: if cfg!(target_os = "macos") {
                transparent_black()
            } else {
                rgb(0xFBFBFC).into()
            },
            sidebar_drag_background: rgb(0xFBFBFC).into(),
            sidebar_item_background: hsla(220.0 / 360.0, 0.13, 0.35, 0.07),
            surface: rgb(0xFBFBFC).into(),
            raised: rgb(0xFFFFFF).into(),
            composer: rgb(0xFFFFFF).into(),
            inset: rgb(0xE2E5EA).into(),
            terminal: rgb(0xFFFFFF).into(),
            overlay: hsla(220.0 / 360.0, 0.13, 0.30, 0.05),
            overlay_strong: hsla(220.0 / 360.0, 0.13, 0.30, 0.10),

            border: hsla(220.0 / 360.0, 0.13, 0.30, 0.10),
            border_strong: hsla(220.0 / 360.0, 0.13, 0.30, 0.18),
            sidebar_border: hsla(220.0 / 360.0, 0.13, 0.30, 0.16),

            text: rgb(0x24272E).into(),
            text_secondary: rgb(0x5C6370).into(),
            text_tertiary: rgb(0x7A8290).into(),
            text_ghost: rgb(0x9AA0AB).into(),

            accent: rgb(0x2B66D2).into(),
            resize_handle: rgb(0x2B66D2).into(),
            gauge: rgb(0x2B66D2).into(),

            selection: rgb(0xC9DCFA).into(),
            code_text: rgb(0xA43E3A).into(),
            code_wash: hsla(220.0 / 360.0, 0.13, 0.30, 0.07),

            inverse: rgb(0x24272E).into(),
            on_inverse: rgb(0xFBFBFC).into(),

            warning: rgb(0x9A6200).into(),
            success: rgb(0x16805F).into(),
            favorite: rgb(0x574504).into(),
            danger: rgb(0xC2373A).into(),
            danger_soft: hsla(359.0 / 360.0, 0.60, 0.50, 0.10),
        }
    }

    /// Winc's warm light palette: paper and warm grey, the day palette with its
    /// hues held and its luminance brought down to match on a warm ground.
    fn winc_paper() -> Theme {
        Theme {
            is_dark: false,
            canvas: rgb(0xEAE6DE).into(),
            sidebar: if cfg!(target_os = "macos") {
                transparent_black()
            } else {
                rgb(0xF7F4EE).into()
            },
            sidebar_drag_background: rgb(0xF7F4EE).into(),
            sidebar_item_background: hsla(38.0 / 360.0, 0.18, 0.35, 0.07),
            surface: rgb(0xF7F4EE).into(),
            raised: rgb(0xFCFAF6).into(),
            composer: rgb(0xFCFAF6).into(),
            inset: rgb(0xE3DED4).into(),
            terminal: rgb(0xFCFAF6).into(),
            overlay: hsla(38.0 / 360.0, 0.18, 0.30, 0.05),
            overlay_strong: hsla(38.0 / 360.0, 0.18, 0.30, 0.10),

            border: hsla(38.0 / 360.0, 0.18, 0.30, 0.10),
            border_strong: hsla(38.0 / 360.0, 0.18, 0.30, 0.18),
            sidebar_border: hsla(38.0 / 360.0, 0.18, 0.30, 0.16),

            text: rgb(0x2A2824).into(),
            text_secondary: rgb(0x5F5A52).into(),
            text_tertiary: rgb(0x7D786F).into(),
            text_ghost: rgb(0x9C968C).into(),

            accent: rgb(0x2A62C8).into(),
            resize_handle: rgb(0x2A62C8).into(),
            gauge: rgb(0x2A62C8).into(),

            selection: rgb(0xD6E0EE).into(),
            code_text: rgb(0x9C3934).into(),
            code_wash: hsla(38.0 / 360.0, 0.18, 0.30, 0.07),

            inverse: rgb(0x2A2824).into(),
            on_inverse: rgb(0xF7F4EE).into(),

            warning: rgb(0x935C00).into(),
            success: rgb(0x157A5A).into(),
            favorite: rgb(0x524000).into(),
            danger: rgb(0xBB3434).into(),
            danger_soft: hsla(0.0, 0.60, 0.47, 0.10),
        }
    }
}

impl Global for ActiveWakuTheme {}

/// Publish the resolved palette. [`Theme::current`] reads it back from the
/// global, which is how every view gets its colors.
fn set_active_theme(theme: Theme, cx: &mut App) {
    cx.set_global(ActiveWakuTheme(theme));
}
/// True when the system is currently in its dark appearance.
fn system_is_dark(appearance: WindowAppearance) -> bool {
    matches!(
        appearance,
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

/// Resolve and publish the startup palette, before any window exists.
///
/// Startup has no settings in hand yet, so this is the graphite default in
/// whichever mode the system is in; the app re-applies the saved palette as
/// soon as it has read the settings file.
pub fn init(cx: &mut App) {
    let dark = system_is_dark(cx.window_appearance());
    set_active_theme(palette::graphite(dark), cx);
}

/// Re-resolve the palette after a settings change and re-apply everything that
/// is not GPUI-painted: the native window appearance and the macOS sidebar's
/// vibrancy tint.
///
/// A theme change is a whole-window invalidation — `docs/performance.md` lists
/// it as a legitimate `window.refresh()` — because every cached pane holds
/// colors resolved at build time.
pub fn apply_appearance(
    mode: ColorMode,
    palette_id: &PaletteId,
    window: &mut Window,
    cx: &mut App,
) {
    let dark = mode.resolves_to_dark(system_is_dark(cx.window_appearance()));
    crate::platform::set_window_appearance(window, mode.native_override());
    let theme = palette::resolve(palette_id, dark);
    crate::platform::configure_sidebar_material(window, &theme);
    set_active_theme(theme, cx);
    window.refresh();
}

/// Every palette id this build resolves, for the settings page's list.
///
/// Re-exported from the protocol crate so the settings loader (which validates
/// an id read from disk) and the settings page (which lists them) share one
/// list rather than two that can drift.
pub use palette::ActiveWakuTheme;
pub use waku_protocol::theme::KNOWN_PALETTES;

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 2.1 relative luminance of an sRGB color.
    fn relative_luminance(color: Hsla) -> f32 {
        let rgb = color.to_rgb();
        fn channel(value: f32) -> f32 {
            if value <= 0.039_28 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(rgb.r) + 0.7152 * channel(rgb.g) + 0.0722 * channel(rgb.b)
    }

    /// WCAG contrast ratio, 1.0 (identical) to 21.0 (black on white).
    fn contrast(a: Hsla, b: Hsla) -> f32 {
        let (a, b) = (relative_luminance(a), relative_luminance(b));
        let (lighter, darker) = if a > b { (a, b) } else { (b, a) };
        (lighter + 0.05) / (darker + 0.05)
    }

    /// Every palette has to clear the same readability floor, in both of its
    /// forms.
    ///
    /// This is the gate that makes "more themes" safe to accept. A palette is
    /// easy to make pretty and easy to make unreadable; without this, the only
    /// thing standing between a new theme and a user would be whether it
    /// looked fine on the author's monitor. The floors are WCAG AA: 4.5:1 for
    /// body text, 3:1 for large text and for the semantic colors that carry
    /// meaning on their own (an error that cannot be read is not an error
    /// you can act on).
    #[test]
    fn every_palette_keeps_its_text_readable() {
        for id in KNOWN_PALETTES {
            for dark in [true, false] {
                let theme = palette::resolve(&PaletteId::new(id), dark);
                let case = format!("{id} ({})", if dark { "dark" } else { "light" });

                for (role, color, floor) in [
                    ("text", theme.text, 4.5),
                    ("text_secondary", theme.text_secondary, 3.0),
                    ("danger", theme.danger, 3.0),
                    ("success", theme.success, 3.0),
                    ("warning", theme.warning, 3.0),
                    ("accent", theme.accent, 3.0),
                ] {
                    let ratio = contrast(color, theme.surface);
                    assert!(
                        ratio >= floor,
                        "{case}: {role} is {ratio:.2}:1 on surface, below the {floor}:1 floor"
                    );
                }
            }
        }
    }

    /// A palette that comes in two forms must actually differ between them.
    /// Copying one into the other compiles, ships, and looks correct on exactly
    /// one of the two appearances.
    ///
    /// The Winc palettes are exempt by name, not by accident: each is one
    /// designed appearance rather than a pair, and [`every_winc_palette_is_
    /// readable_in_the_appearance_it_was_designed_for`] covers them instead.
    #[test]
    fn every_palette_has_two_distinct_forms() {
        for id in KNOWN_PALETTES {
            if !palette::follows_appearance(id) {
                continue;
            }
            let dark = palette::resolve(&PaletteId::new(id), true);
            let light = palette::resolve(&PaletteId::new(id), false);
            assert!(dark.is_dark, "{id}'s dark form reports itself light");
            assert!(!light.is_dark, "{id}'s light form reports itself dark");
            assert!(
                relative_luminance(dark.canvas) < relative_luminance(light.canvas),
                "{id}'s dark canvas is not darker than its light one"
            );
        }
    }

    /// A single-appearance palette must report the appearance it was drawn for,
    /// and stay readable on it.
    ///
    /// This is what stops a `winc-night` from being handed to a light-mode user
    /// and vice versa: the pairing is a fact about the palette, not about
    /// whichever mode happens to be selected.
    #[test]
    fn every_winc_palette_is_readable_in_the_appearance_it_was_designed_for() {
        for (id, expected_dark) in [
            ("winc-night", true),
            ("winc-slate", true),
            ("winc-day", false),
            ("winc-paper", false),
        ] {
            for mode in [true, false] {
                let theme = palette::resolve(&PaletteId::new(id), mode);
                assert_eq!(
                    theme.is_dark, expected_dark,
                    "{id} reports the wrong appearance for mode {mode}"
                );
                let ratio = contrast(theme.text, theme.surface);
                assert!(
                    ratio >= 4.5,
                    "{id}: body text is {ratio:.2}:1 on its own surface"
                );
            }
        }
    }

    /// An id that this build does not know resolves to the default rather than
    /// failing — a settings file written by a newer build still opens.
    #[test]
    fn an_unknown_palette_id_falls_back_to_the_default() {
        let unknown = PaletteId::new("from-a-future-build");
        let resolved = palette::resolve(&unknown, true);
        let fallback = palette::resolve(&PaletteId::default(), true);
        assert_eq!(
            relative_luminance(resolved.surface),
            relative_luminance(fallback.surface)
        );
        assert_eq!(unknown.or_default(KNOWN_PALETTES).as_str(), "graphite");
    }

    /// The mode decides light-or-dark, and says so consistently for the native
    /// window chrome.
    #[test]
    fn the_color_mode_decides_light_or_dark() {
        assert!(ColorMode::Dark.resolves_to_dark(false));
        assert!(!ColorMode::Light.resolves_to_dark(true));
        assert!(ColorMode::System.resolves_to_dark(true));
        assert!(!ColorMode::System.resolves_to_dark(false));
        assert_eq!(ColorMode::System.native_override(), None);
        assert_eq!(ColorMode::Dark.native_override(), Some(true));
        assert_eq!(ColorMode::Light.native_override(), Some(false));
    }
}

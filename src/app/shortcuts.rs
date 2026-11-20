//! The keyboard reference shown in Settings, and the test that keeps it honest.
//!
//! The bindings live in `lib.rs` as imperative `cx.bind_keys([...])` calls,
//! which cannot be enumerated at runtime — the registry is inside GPUI. A
//! second hand-written list would therefore drift the moment someone adds a
//! binding, and a shortcut reference that is wrong is worse than none: it
//! teaches keys that do nothing.
//!
//! So the table here is the *display* form, and the test below parses the
//! binding source and fails when the two disagree. Adding a binding means
//! updating this table, and the test is what says so.

/// Render a binding's neutral form — `secondary-shift-b` — the way the user's
/// platform writes it.
///
/// The bindings are written with `secondary`, which GPUI resolves to Command on
/// macOS and Control elsewhere. A reference that printed `secondary` would name
/// a key that does not exist, so the same substitution has to happen here.
pub fn display_keys(keys: &str) -> String {
    let mut parts = keys.split('-').peekable();
    let mut rendered: Vec<String> = Vec::new();
    while let Some(part) = parts.next() {
        let is_last = parts.peek().is_none();
        let text = match part {
            // The last segment is the key itself, and `-` is a literal key.
            "-" if is_last => "-".to_owned(),
            "secondary" => {
                if cfg!(target_os = "macos") {
                    "⌘".to_owned()
                } else {
                    "Ctrl".to_owned()
                }
            }
            "shift" => {
                if cfg!(target_os = "macos") {
                    "⇧".to_owned()
                } else {
                    "Shift".to_owned()
                }
            }
            "alt" => {
                if cfg!(target_os = "macos") {
                    "⌥".to_owned()
                } else {
                    "Alt".to_owned()
                }
            }
            "ctrl" => {
                if cfg!(target_os = "macos") {
                    "⌃".to_owned()
                } else {
                    "Ctrl".to_owned()
                }
            }
            other => other.to_uppercase(),
        };
        rendered.push(text);
    }
    // macOS writes modifiers as glyphs with no separator; elsewhere they read
    // as words joined by `+`.
    if cfg!(target_os = "macos") {
        rendered.concat()
    } else {
        rendered.join("+")
    }
}

/// A group of related shortcuts.
pub struct ShortcutSection {
    pub title_key: &'static str,
    pub rows: &'static [(&'static str, &'static str)],
}

/// The reference, grouped the way a reader looks for something: "how do I
/// switch tasks" is a different question from "how do I quit".
///
/// `secondary` is the platform primary modifier — ⌘ on macOS, Ctrl elsewhere —
/// and every row that uses it resolves through
/// [`crate::platform::primary_shortcut`] at render time, so this table stores
/// the neutral form.
///
/// Scoped bindings are deliberately absent, and the drift test enforces that
/// they stay absent: the composer's `up`/`down` prompt-history keys live in
/// [`crate::ui::menu`] under a key context, not here. Listing a key that only
/// works inside one focused surface would promise it in the transcript.
pub const SHORTCUT_SECTIONS: &[ShortcutSection] = &[
    ShortcutSection {
        title_key: "shortcuts.section_general",
        rows: &[
            ("secondary-n", "shortcuts.new_task"),
            ("secondary-o", "shortcuts.open_project"),
            ("secondary-k", "shortcuts.command_palette"),
            ("secondary-,", "shortcuts.settings"),
            ("secondary-w", "shortcuts.close_window"),
            ("secondary-q", "shortcuts.quit"),
        ],
    },
    ShortcutSection {
        title_key: "shortcuts.section_navigation",
        rows: &[
            ("secondary-b", "shortcuts.toggle_sidebar"),
            ("secondary-shift-b", "shortcuts.toggle_right_panel"),
            ("secondary-l", "shortcuts.focus_composer"),
            ("secondary-[", "shortcuts.back"),
            ("secondary-]", "shortcuts.forward"),
            ("ctrl-tab", "shortcuts.switch_task_forward"),
            ("ctrl-shift-tab", "shortcuts.switch_task_backward"),
        ],
    },
    ShortcutSection {
        title_key: "shortcuts.section_agent",
        rows: &[
            ("secondary-/", "shortcuts.model_picker"),
            ("secondary-u", "shortcuts.usage"),
            ("escape", "shortcuts.stop_turn"),
        ],
    },
    ShortcutSection {
        title_key: "shortcuts.section_editing",
        rows: &[
            ("secondary-f", "shortcuts.find"),
            ("secondary-alt-f", "shortcuts.find_replace"),
            ("secondary-g", "shortcuts.find_next"),
            ("secondary-shift-g", "shortcuts.find_previous"),
            ("secondary-s", "shortcuts.save_file"),
            ("secondary-c", "shortcuts.copy"),
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every shortcut in the reference must actually be bound.
    ///
    /// Read from the binding source rather than a copied list: the failure
    /// this prevents is a reference that documents a key nobody bound, or a
    /// binding nobody documented.
    #[test]
    fn the_reference_names_only_bound_shortcuts() {
        let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
            .expect("lib.rs is readable");

        for section in SHORTCUT_SECTIONS {
            for (keys, _) in section.rows {
                let needle = format!("KeyBinding::new(\"{keys}\"");
                assert!(
                    source.contains(&needle),
                    "the shortcut reference lists `{keys}`, which no KeyBinding binds"
                );
            }
        }
    }

    /// Window-wide bindings that are deliberately not in the reference.
    ///
    /// `secondary-alt-shift-f` toggles the FPS counter — a diagnostic, not a
    /// feature, and listing it would teach a key that most users should never
    /// press. Excluding it by name keeps the drift test strict for everything
    /// else instead of loosening it to "some bindings may be missing".
    const INTENTIONALLY_UNDOCUMENTED: &[&str] = &["secondary-alt-shift-f"];

    /// And every window-wide binding must appear in the reference, so the
    /// reference cannot quietly go stale as bindings are added.
    ///
    /// Scoped bindings (`Some(context)`) are excluded: they only fire inside a
    /// focused surface, where the surface itself is the affordance, and a flat
    /// list of them would claim keys that do nothing in the transcript.
    #[test]
    fn every_window_wide_binding_is_in_the_reference() {
        let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
            .expect("lib.rs is readable");

        let documented: Vec<&str> = SHORTCUT_SECTIONS
            .iter()
            .flat_map(|section| section.rows.iter().map(|(keys, _)| *keys))
            .collect();

        for line in source.lines() {
            let trimmed = line.trim();
            let Some(rest) = trimmed.strip_prefix("KeyBinding::new(\"") else {
                continue;
            };
            let Some(keys) = rest.split('"').next() else {
                continue;
            };
            // Only unscoped bindings: `..., None)` is the window-wide form.
            if !trimmed.contains(", None)") {
                continue;
            }
            if INTENTIONALLY_UNDOCUMENTED.contains(&keys) {
                continue;
            }
            assert!(
                documented.contains(&keys),
                "`{keys}` is bound window-wide but missing from the shortcut reference"
            );
        }
    }
}

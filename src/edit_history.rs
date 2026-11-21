//! Undo and redo for the text surfaces that record edits as splices.
//!
//! A step is "the text at `start` was `old` and is now `new`". Undo splices
//! `old` back over `new`; redo reverses that. Steps grow in place while a run of
//! typing or deleting coalesces into one, so the stack stays proportional to
//! gestures rather than keystrokes.
//!
//! ## Why this is shared rather than copied
//!
//! The one-line fields and the file editor must undo the same way. Their
//! contents differ — a `SharedString` there, a rope here — but the question a
//! user asks of Ctrl+Z is identical: after typing `abc` and pausing, one undo
//! should remove `abc`, and a backspace run should undo as one deletion. Two
//! implementations of that would drift, and the drift would be invisible until
//! someone undid the wrong thing.
//!
//! ## What it deliberately does not know
//!
//! It never holds the document. Every method takes the current text, so a
//! backend that stores lines in a rope pays no extra copy, and a step that no
//! longer matches the text is detected instead of applied blindly.

use std::ops::Range;
use std::time::{Duration, Instant};

/// How long after the previous edit a new one may still coalesce into the
/// same undo step — Zed's transaction group interval.
pub const UNDO_GROUP_INTERVAL: Duration = Duration::from_millis(300);

/// Undo steps kept before the oldest fall off. Steps are whole coalesced
/// gestures, so this is far more editing than anyone steps back through.
pub const UNDO_HISTORY_CAP: usize = 1000;

/// One undo step.
struct EditRecord {
    start: usize,
    old: String,
    new: String,
    /// Selection to restore when the step is undone.
    selection_before: Range<usize>,
    selection_reversed_before: bool,
    /// When the newest coalesced edit landed, bounding the group interval.
    edited_at: Instant,
    /// A sealed step never coalesces with later edits. Set at gesture
    /// boundaries: cut, paste, a completion insert, a finished composition.
    sealed: bool,
    /// An IME composition still underway, amended in place as the marked
    /// text changes so the whole entry undoes as one step.
    composing: bool,
}

#[derive(Default)]
pub struct EditHistory {
    undo: Vec<EditRecord>,
    redo: Vec<EditRecord>,
}

impl EditHistory {
    /// Record a splice of `new_text` over `range`, called with the content
    /// the splice has not yet been applied to. Coalesces with the newest
    /// step where the pair reads as one gesture: an insertion continuing at
    /// the end of an insertion, or a deletion extending a deletion run in
    /// either direction.
    pub fn record(
        &mut self,
        content: &str,
        range: &Range<usize>,
        new_text: &str,
        selection: Range<usize>,
        selection_reversed: bool,
        now: Instant,
    ) {
        // A whole-content replace (Replace All) arrives as one huge splice;
        // trimming the shared affixes stores only the span that changed.
        let (start, old, new) = trimmed_splice(content, range, new_text);
        if old.is_empty() && new.is_empty() {
            return;
        }
        self.redo.clear();
        if let Some(last) = self.undo.last_mut()
            && !last.sealed
            && !last.composing
            && now.duration_since(last.edited_at) < UNDO_GROUP_INTERVAL
        {
            // Typing run: this insertion continues where the last edit's text
            // ended, which also lets typing extend a replaced selection.
            if old.is_empty() && !last.new.is_empty() && start == last.start + last.new.len() {
                last.new.push_str(&new);
                last.edited_at = now;
                return;
            }
            if new.is_empty() && last.new.is_empty() && !last.old.is_empty() {
                // Backspace run: this deletion ends where the last one started.
                if start + old.len() == last.start {
                    last.start = start;
                    last.old.insert_str(0, &old);
                    last.edited_at = now;
                    return;
                }
                // Forward-delete run: this deletion starts where the last did.
                if start == last.start {
                    last.old.push_str(&old);
                    last.edited_at = now;
                    return;
                }
            }
        }
        self.push(EditRecord {
            start,
            old,
            new,
            selection_before: selection,
            selection_reversed_before: selection_reversed,
            edited_at: now,
            sealed: false,
            composing: false,
        });
    }

    /// Record an IME splice. The whole composition — every marked-text
    /// revision and the final commit — stays one step, amended in place.
    pub fn record_composition(
        &mut self,
        content: &str,
        range: &Range<usize>,
        new_text: &str,
        selection: Range<usize>,
        selection_reversed: bool,
        now: Instant,
    ) {
        self.redo.clear();
        if let Some(last) = self.undo.last_mut()
            && last.composing
        {
            let span = last.start..last.start + last.new.len();
            if range.start >= span.start && range.end <= span.end {
                last.new
                    .replace_range(range.start - last.start..range.end - last.start, new_text);
                last.edited_at = now;
                return;
            }
            // A splice outside the open composition should not happen; close
            // the step rather than corrupt it.
            last.composing = false;
            last.sealed = true;
        }
        self.push(EditRecord {
            start: range.start,
            old: content[range.clone()].to_owned(),
            new: new_text.to_owned(),
            selection_before: selection,
            selection_reversed_before: selection_reversed,
            edited_at: now,
            sealed: false,
            composing: true,
        });
    }

    /// Close the open composition step, if any. A canceled composition that
    /// nets no change records nothing.
    pub fn finalize_composition(&mut self) {
        if let Some(last) = self.undo.last_mut()
            && last.composing
        {
            last.composing = false;
            last.sealed = true;
            if last.old == last.new {
                self.undo.pop();
            }
        }
    }

    /// Stop later edits from coalescing into the newest step — a gesture
    /// boundary such as cut, paste, or a completion insert.
    pub fn seal(&mut self) {
        if let Some(last) = self.undo.last_mut() {
            last.sealed = true;
        }
    }

    /// Forget everything, as replacing the whole text does.
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }

    /// Whether there is a step to undo.
    ///
    /// Not called by the editor yet: its key binding simply attempts the undo
    /// and does nothing when there is none. This exists for the tests that pin
    /// the history's bookkeeping, and for the menu items the panel will show
    /// once it has a per-file menu.
    #[allow(
        dead_code,
        reason = "pins the history's public bookkeeping for tests and future callers"
    )]
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    #[allow(dead_code, reason = "see can_undo")]
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    fn push(&mut self, record: EditRecord) {
        self.undo.push(record);
        if self.undo.len() > UNDO_HISTORY_CAP {
            let excess = self.undo.len() - UNDO_HISTORY_CAP;
            self.undo.drain(..excess);
        }
    }

    /// Apply the newest undo step to `content`, returning the restored
    /// content and the selection to show. The step must still verifiably
    /// describe `content`; on any mismatch the history is corrupt and is
    /// dropped whole rather than applied wrong.
    pub fn undo(&mut self, content: &str) -> Option<(String, Range<usize>, bool)> {
        let record = self.undo.pop()?;
        let span = record.start..record.start + record.new.len();
        if content.get(span.clone()) != Some(record.new.as_str()) {
            self.undo.clear();
            self.redo.clear();
            return None;
        }
        let restored = [&content[..span.start], &record.old, &content[span.end..]].concat();
        let selection = record.selection_before.start.min(restored.len())
            ..record.selection_before.end.min(restored.len());
        let selection_reversed = record.selection_reversed_before;
        self.redo.push(record);
        Some((restored, selection, selection_reversed))
    }

    /// Reapply the newest undone step, with the caret after the re-applied
    /// text — where the original edit left it.
    pub fn redo(&mut self, content: &str) -> Option<(String, Range<usize>, bool)> {
        let record = self.redo.pop()?;
        let span = record.start..record.start + record.old.len();
        if content.get(span.clone()) != Some(record.old.as_str()) {
            self.undo.clear();
            self.redo.clear();
            return None;
        }
        let applied = [&content[..span.start], &record.new, &content[span.end..]].concat();
        let caret = record.start + record.new.len();
        self.undo.push(record);
        Some((applied, caret..caret, false))
    }
}

/// The splice with its common affixes removed: where `range` in `content` is
/// replaced by `new_text`, the returned offset and (old, new) pair cover only
/// the bytes that actually differ. Keeps a whole-content replace from storing
/// two copies of the file when only a few spans changed.
pub fn trimmed_splice(
    content: &str,
    range: &Range<usize>,
    new_text: &str,
) -> (usize, String, String) {
    let range = range.start.min(content.len())..range.end.min(content.len());
    let old = &content[range.clone()];
    let prefix = common_prefix_len(old, new_text);
    let suffix = common_suffix_len(&old[prefix..], &new_text[prefix..]);
    let start = range.start + prefix;
    let old = old[prefix..old.len() - suffix].to_owned();
    let new = new_text[prefix..new_text.len() - suffix].to_owned();
    (start, old, new)
}

/// Bytes shared at the start of both strings, backed off to a character
/// boundary so a partially shared code point is not split.
fn common_prefix_len(a: &str, b: &str) -> usize {
    let mut len = a
        .as_bytes()
        .iter()
        .zip(b.as_bytes())
        .take_while(|(x, y)| x == y)
        .count();
    while !a.is_char_boundary(len) {
        len -= 1;
    }
    len
}

/// Bytes shared at the end of both strings. The shared bytes are identical,
/// so a boundary found in one is a boundary in the other.
fn common_suffix_len(a: &str, b: &str) -> usize {
    let mut len = a
        .as_bytes()
        .iter()
        .rev()
        .zip(b.as_bytes().iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    while !a.is_char_boundary(a.len() - len) {
        len -= 1;
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_at(
        history: &mut EditHistory,
        content: &mut String,
        at: usize,
        chunks: &[&str],
        start: Instant,
        gap: Duration,
    ) -> usize {
        let mut at = at;
        for (index, chunk) in chunks.iter().enumerate() {
            let when = start + gap * (index as u32 + 1);
            history.record(content, &(at..at), chunk, at..at, false, when);
            content.insert_str(at, chunk);
            at += chunk.len();
        }
        at
    }

    #[test]
    fn a_typing_run_undoes_as_one_step() {
        let mut history = EditHistory::default();
        let mut content = "fn main() {}".to_owned();
        let start = Instant::now();
        let at = type_at(
            &mut history,
            &mut content,
            3,
            &["a", "b", "c"],
            start,
            Duration::from_millis(50),
        );
        assert_eq!(content, "fn abcmain() {}");
        assert_eq!(at, 6);

        let (restored, selection, _) = history.undo(&content).unwrap();
        assert_eq!(restored, "fn main() {}");
        assert_eq!(selection, 3..3, "caret returns to where typing began");
        assert!(
            history.undo(&restored).is_none(),
            "the whole run is one step"
        );

        let (redone, selection, _) = history.redo(&restored).unwrap();
        assert_eq!(redone, "fn abcmain() {}");
        assert_eq!(selection, 6..6, "caret lands after the reapplied text");
    }

    #[test]
    fn a_pause_starts_a_new_undo_step() {
        let mut history = EditHistory::default();
        let mut content = String::new();
        let start = Instant::now();
        type_at(&mut history, &mut content, 0, &["a"], start, Duration::ZERO);
        type_at(
            &mut history,
            &mut content,
            1,
            &["b"],
            start + UNDO_GROUP_INTERVAL,
            Duration::ZERO,
        );
        assert_eq!(history.undo.len(), 2);

        let (restored, ..) = history.undo(&content).unwrap();
        assert_eq!(restored, "a");
    }

    #[test]
    fn a_deletion_run_coalesces_in_either_direction() {
        let mut history = EditHistory::default();
        let mut content = "abcdef".to_owned();
        let start = Instant::now();
        // Backspace twice: each deletes the character before the caret.
        history.record(&content, &(4..5), "", 4..5, false, start);
        content.replace_range(4..5, "");
        history.record(
            &content,
            &(3..4),
            "",
            3..4,
            false,
            start + Duration::from_millis(50),
        );
        content.replace_range(3..4, "");
        assert_eq!(content, "abcf");
        assert_eq!(history.undo.len(), 1, "one backspace run is one step");

        let (restored, selection, _) = history.undo(&content).unwrap();
        assert_eq!(restored, "abcdef");
        assert_eq!(
            selection,
            4..5,
            "the first deletion's selection returns, not the last"
        );
    }

    /// Typing over a selection and then continuing to type reads as one
    /// gesture, so one undo must restore the replaced text.
    #[test]
    fn typing_extends_a_replaced_selection_into_one_step() {
        let mut history = EditHistory::default();
        let mut content = String::from("abcdef");
        let start = Instant::now();
        history.record(&content, &(2..5), "x", 2..5, false, start);
        content.replace_range(2..5, "x");
        type_at(
            &mut history,
            &mut content,
            3,
            &["y"],
            start + Duration::from_millis(50),
            Duration::ZERO,
        );
        assert_eq!(content, "abxyf");

        let (restored, selection, _) = history.undo(&content).unwrap();
        assert_eq!(restored, "abcdef");
        assert_eq!(selection, 2..5, "the replaced selection comes back");
        assert!(history.undo(&restored).is_none());
    }

    /// Undo, then edit: the redo branch is gone. Keeping it would let a redo
    /// apply a step that no longer describes the text.
    #[test]
    fn an_edit_after_undo_drops_the_redo_branch() {
        let mut history = EditHistory::default();
        let mut content = String::new();
        let start = Instant::now();
        type_at(&mut history, &mut content, 0, &["a"], start, Duration::ZERO);
        let (restored, ..) = history.undo(&content).unwrap();
        let mut content = restored;
        assert!(history.can_redo());
        type_at(
            &mut history,
            &mut content,
            0,
            &["b"],
            start + UNDO_GROUP_INTERVAL * 2,
            Duration::ZERO,
        );
        assert!(!history.can_redo());
        assert!(history.redo(&content).is_none());
    }

    /// Replace All arrives as one whole-content splice; the record must keep
    /// only the span that changed, not two copies of the file.
    #[test]
    fn a_whole_content_replace_records_only_the_changed_span() {
        let mut history = EditHistory::default();
        let content = "xx aaa yy";
        let replaced = "xx bbb yy";
        history.record(
            content,
            &(0..content.len()),
            replaced,
            0..content.len(),
            false,
            Instant::now(),
        );
        let record = history.undo.last().unwrap();
        assert_eq!((record.start, record.old.as_str()), (3, "aaa"));
        assert_eq!(record.new, "bbb");

        let (restored, ..) = history.undo(replaced).unwrap();
        assert_eq!(restored, content);
    }

    #[test]
    fn a_sealed_step_does_not_absorb_later_typing() {
        let mut history = EditHistory::default();
        let mut content = String::new();
        let start = Instant::now();
        type_at(&mut history, &mut content, 0, &["a"], start, Duration::ZERO);
        history.seal();
        type_at(
            &mut history,
            &mut content,
            1,
            &["b"],
            start,
            Duration::from_millis(50),
        );
        assert_eq!(history.undo.len(), 2);
    }

    #[test]
    fn undo_against_unrelated_content_drops_history_instead_of_applying_wrong() {
        let mut history = EditHistory::default();
        let mut content = String::new();
        type_at(
            &mut history,
            &mut content,
            0,
            &["x"],
            Instant::now(),
            Duration::ZERO,
        );
        assert!(history.undo("unrelated content").is_none());
        assert!(history.undo.is_empty() && history.redo.is_empty());
    }

    #[test]
    fn trimmed_splices_respect_character_boundaries() {
        // "é" and "è" share their first UTF-8 byte; the shared byte must not
        // be trimmed off mid-character.
        assert_eq!(
            trimmed_splice("é", &(0..2), "è"),
            (0, "é".to_owned(), "è".to_owned())
        );
        assert_eq!(
            trimmed_splice("aé", &(0..3), "bé"),
            (0, "a".to_owned(), "b".to_owned())
        );
        assert_eq!(
            trimmed_splice("abc", &(0..3), "abcd"),
            (3, String::new(), "d".to_owned())
        );
    }

    /// The regression that motivates this module being shared rather than
    /// copied: a composition must undo as one step even when its intermediate
    /// revisions replaced ranges of different lengths.
    #[test]
    fn an_ime_composition_undoes_as_one_step() {
        let mut history = EditHistory::default();
        let start = Instant::now();
        let mut content = String::from("a");
        history.record_composition(&content, &(1..1), "k", 1..1, false, start);
        content.push('k');
        history.record_composition(&content, &(1..2), "か", 1..2, false, start);
        content = "aか".to_owned();
        history.record_composition(&content, &(1..4), "漢字", 1..4, false, start);
        content = "a漢字".to_owned();
        history.finalize_composition();
        assert_eq!(history.undo.len(), 1);

        let (restored, selection, _) = history.undo(&content).unwrap();
        assert_eq!(restored, "a");
        assert_eq!(selection, 1..1);
    }

    #[test]
    fn a_cancelled_composition_leaves_no_step() {
        let mut history = EditHistory::default();
        let start = Instant::now();
        let mut content = String::from("a");
        history.record_composition(&content, &(1..1), "k", 1..1, false, start);
        content.push('k');
        history.record_composition(&content, &(1..2), "", 1..2, false, start);
        history.finalize_composition();
        assert!(history.undo.is_empty());
    }

    #[test]
    fn the_history_is_capped() {
        let mut history = EditHistory::default();
        let start = Instant::now();
        let mut content = String::new();
        for step in 0..UNDO_HISTORY_CAP + 5 {
            history.record(
                &content,
                &(0..0),
                "x",
                0..0,
                false,
                start + UNDO_GROUP_INTERVAL * step as u32,
            );
            content.insert(0, 'x');
        }
        assert_eq!(history.undo.len(), UNDO_HISTORY_CAP);
    }

    #[test]
    fn clear_forgets_both_directions() {
        let mut history = EditHistory::default();
        let mut content = String::new();
        type_at(
            &mut history,
            &mut content,
            0,
            &["a"],
            Instant::now(),
            Duration::ZERO,
        );
        let _ = history.undo(&content);
        assert!(history.can_redo());
        history.clear();
        assert!(!history.can_undo() && !history.can_redo());
    }
}

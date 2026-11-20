//! Up/Down recall of submitted prompts in the composer.
//!
//! The rule is the shell's, adapted to a field that also needs its arrows: Up
//! recalls only from the field's first row, and Down only leaves history. That
//! keeps a multi-line prompt editable on its earlier lines instead of turning
//! Up into a history key that makes the caret unreachable.
//!
//! The state machine here is pure. The view decides *when* to step it (the
//! field's geometry says whether the arrow is free), so the questionable part —
//! what the next value should be — is testable without a window.

/// Whether a submission is worth remembering as a recallable prompt.
///
/// An attachment-only send is not: its text is the file names, and recalling
/// those would fill the field with words whose attachments are gone. A send
/// that had text *and* attachments is remembered, because the text alone is
/// still something the user can resend.
pub(super) fn is_recallable_prompt(has_visible_text: bool, attachment_count: usize) -> bool {
    has_visible_text || attachment_count == 0
}

/// Where a history walk has got to.
///
/// `stash` is what the user was writing before they started walking. It is
/// restored when they come back past the newest entry, which is what makes
/// "speak now, panic, up-arrow, then down-arrow" not lose the draft.
#[derive(Clone, Debug, Default)]
pub(crate) struct PromptHistoryWalk {
    /// Index into the newest-first view of the project's entries.
    cursor: Option<usize>,
    stash: Option<String>,
}

impl PromptHistoryWalk {
    pub(super) fn is_walking(&self) -> bool {
        self.cursor.is_some()
    }

    /// Forget the walk, keeping no stash. Called whenever the field stops being
    /// a history view: a submit, a project change, or the user typing.
    pub(super) fn reset(&mut self) {
        self.cursor = None;
        self.stash = None;
    }

    /// Leave history without losing what the walk stashed.
    ///
    /// Returns the text the caller should put back in the field, or `None` when
    /// there was no walk to leave. Any non-history edit the field takes after
    /// this is the user's own.
    pub(super) fn leave(&mut self) -> Option<String> {
        let stash = self.stash.take()?;
        self.cursor = None;
        Some(stash)
    }

    /// One step older. `current_text` is stashed on the first step so it can be
    /// recovered later.
    ///
    /// Returns the text to show, or `None` when there is nothing older — the
    /// caller leaves the arrow with the field.
    pub(super) fn step_back(&mut self, entries: &[String], current_text: &str) -> Option<String> {
        if entries.is_empty() {
            return None;
        }
        let next = match self.cursor {
            None => {
                // `entries` is oldest-first, so the newest is the last index.
                self.stash = Some(current_text.to_owned());
                entries.len() - 1
            }
            Some(0) => return None,
            Some(index) => index - 1,
        };
        self.cursor = Some(next);
        entries.get(next).cloned()
    }

    /// One step newer. Past the newest entry this restores the stash.
    ///
    /// Returns `None` when there is nothing newer and no stash to restore, so
    /// the caller can leave the arrow with the field.
    pub(super) fn step_forward(&mut self, entries: &[String]) -> Option<String> {
        let index = self.cursor?;
        if index + 1 < entries.len() {
            self.cursor = Some(index + 1);
            return entries.get(index + 1).cloned();
        }
        self.leave()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<String> {
        // Oldest first, as stored.
        vec!["first".into(), "second".into(), "third".into()]
    }

    #[test]
    fn up_walks_from_newest_to_oldest_then_stops() {
        let entries = entries();
        let mut walk = PromptHistoryWalk::default();
        assert_eq!(walk.step_back(&entries, "").as_deref(), Some("third"));
        assert_eq!(walk.step_back(&entries, "").as_deref(), Some("second"));
        assert_eq!(walk.step_back(&entries, "").as_deref(), Some("first"));
        // Nowhere older: the caller keeps the arrow.
        assert_eq!(walk.step_back(&entries, ""), None);
        assert_eq!(walk.cursor, Some(0));
    }

    #[test]
    fn down_past_the_newest_restores_what_was_being_written() {
        let entries = entries();
        let mut walk = PromptHistoryWalk::default();
        assert_eq!(
            walk.step_back(&entries, "half-written").as_deref(),
            Some("third")
        );
        assert_eq!(
            walk.step_back(&entries, "ignored").as_deref(),
            Some("second")
        );
        assert_eq!(walk.step_forward(&entries).as_deref(), Some("third"));
        // One more goes past the newest and brings the draft back.
        assert_eq!(walk.step_forward(&entries).as_deref(), Some("half-written"));
        assert!(!walk.is_walking());
        // And it is consumed: a second forward has nothing to hand back.
        assert_eq!(walk.step_forward(&entries), None);
    }

    #[test]
    fn the_stash_is_captured_only_on_the_first_step() {
        let entries = entries();
        let mut walk = PromptHistoryWalk::default();
        walk.step_back(&entries, "draft");
        // A later step must not overwrite the stash with recalled history text.
        walk.step_back(&entries, "third");
        walk.step_forward(&entries);
        assert_eq!(walk.step_forward(&entries).as_deref(), Some("draft"));
    }

    #[test]
    fn an_empty_history_never_starts_a_walk() {
        let mut walk = PromptHistoryWalk::default();
        assert_eq!(walk.step_back(&[], "draft"), None);
        assert!(!walk.is_walking());
        // Nothing was stashed, so a forward step has nothing to restore.
        assert_eq!(walk.step_forward(&[]), None);
    }

    #[test]
    fn leave_returns_the_stash_once() {
        let entries = entries();
        let mut walk = PromptHistoryWalk::default();
        walk.step_back(&entries, "draft");
        assert_eq!(walk.leave().as_deref(), Some("draft"));
        assert!(!walk.is_walking());
        assert_eq!(walk.leave(), None);
    }

    #[test]
    fn reset_drops_the_stash_too() {
        let entries = entries();
        let mut walk = PromptHistoryWalk::default();
        walk.step_back(&entries, "draft");
        walk.reset();
        assert!(!walk.is_walking());
        assert_eq!(walk.leave(), None);
    }

    #[test]
    fn a_single_entry_walks_up_and_back() {
        let entries = vec!["only".to_owned()];
        let mut walk = PromptHistoryWalk::default();
        assert_eq!(walk.step_back(&entries, "draft").as_deref(), Some("only"));
        assert_eq!(walk.step_back(&entries, "draft"), None);
        assert_eq!(walk.step_forward(&entries).as_deref(), Some("draft"));
    }

    #[test]
    fn forward_without_a_walk_does_nothing() {
        let entries = entries();
        let mut walk = PromptHistoryWalk::default();
        assert_eq!(walk.step_forward(&entries), None);
        assert!(!walk.is_walking());
    }

    #[test]
    fn only_a_send_with_something_to_resend_is_recallable() {
        // Plain text: yes.
        assert!(is_recallable_prompt(true, 0));
        // Text plus attachments: the text alone can be resent, so yes.
        assert!(is_recallable_prompt(true, 3));
        // Attachments with no text: the "prompt" is file names whose files are
        // gone, so no.
        assert!(!is_recallable_prompt(false, 1));
    }
}

use super::*;

const COMPOSER_DRAFT_SAVE_DELAY: Duration = Duration::from_millis(250);

impl From<&ComposerAttachment> for crate::persistence::ComposerDraftAttachment {
    fn from(attachment: &ComposerAttachment) -> Self {
        Self {
            path: attachment.path.clone(),
            mention: attachment.mention.clone(),
            name: attachment.name.to_string(),
            is_dir: attachment.is_dir,
            is_image: attachment.is_image,
            blob_reference: attachment.blob_reference.clone(),
        }
    }
}

impl From<crate::persistence::ComposerDraftAttachment> for ComposerAttachment {
    fn from(attachment: crate::persistence::ComposerDraftAttachment) -> Self {
        Self {
            path: attachment.path,
            client_preview_image: None,
            mention: attachment.mention,
            name: SharedString::from(attachment.name),
            is_dir: attachment.is_dir,
            is_image: attachment.is_image,
            blob_reference: attachment.blob_reference,
        }
    }
}

impl From<ComposerAttachment> for MessageAttachment {
    fn from(attachment: ComposerAttachment) -> Self {
        Self {
            path: attachment.path,
            mention: attachment.mention,
            name: attachment.name.to_string(),
            is_dir: attachment.is_dir,
            is_image: attachment.is_image,
            blob_reference: attachment.blob_reference,
        }
    }
}

impl From<MessageAttachment> for ComposerAttachment {
    fn from(attachment: MessageAttachment) -> Self {
        Self {
            path: attachment.path,
            client_preview_image: None,
            mention: attachment.mention,
            name: SharedString::from(attachment.name),
            is_dir: attachment.is_dir,
            is_image: attachment.is_image,
            blob_reference: attachment.blob_reference,
        }
    }
}

impl Waku {
    /// The submitted prompts of the project the composer is currently writing
    /// for, oldest first. Empty when no project is selected, which is why
    /// every caller degrades to "the arrow is not ours".
    pub(super) fn prompt_history_entries(&self) -> &[String] {
        match self.selected_project() {
            Some(project) => self.state.prompt_history.entries(project.id),
            None => &[],
        }
    }

    /// Remember a prompt that was actually sent.
    ///
    /// Recorded from the human-facing text rather than the transport prompt:
    /// a skill expansion or an injected attachment path would come back as a
    /// wall of text the user never typed.
    ///
    /// An attachment-only submission is skipped on purpose. Its "text" is the
    /// file names, and recalling those would put words in the field while the
    /// attachments they referred to stayed gone — a prompt that cannot be sent
    /// is worse than one that is not remembered.
    pub(super) fn record_prompt_history(&mut self, submission: &super::ComposerSubmission) {
        let has_visible_text = submission.display_content.is_some()
            || (submission.attachments.is_empty() && !submission.prompt.trim().is_empty());
        if !super::prompt_history::is_recallable_prompt(
            has_visible_text,
            submission.attachments.len(),
        ) {
            return;
        }
        let Some(project) = self.selected_project().map(|project| project.id) else {
            return;
        };
        let text = submission.human_prompt();
        if self.state.prompt_history.record(project, &text) {
            self.prompt_history_walk.reset();
            self.save();
        }
    }

    /// Up: recall one entry older, if the caret is on the field's first row.
    ///
    /// `cx.propagate()` hands the keystroke back to the field, which is what
    /// must happen on every row but the first — otherwise a multi-line prompt
    /// could no longer be edited on its earlier lines.
    pub(super) fn recall_previous_prompt(
        &mut self,
        _: &crate::ui::menu::PromptHistoryPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.composer.read(cx).caret_on_first_row(cx) {
            cx.propagate();
            return;
        }
        let entries = self.prompt_history_entries().to_vec();
        let current = self.composer.read(cx).content(cx).to_owned();
        let Some(text) = self.prompt_history_walk.step_back(&entries, &current) else {
            cx.propagate();
            return;
        };
        self.apply_recalled_prompt(text, window, cx);
    }

    /// Down: one entry newer while walking, or back to what was being written.
    ///
    /// Not a bound action: leaving history has to work from any row, and only
    /// the field's live geometry knows where the caret is when the key lands.
    pub(super) fn recall_next_prompt(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.prompt_history_walk.is_walking() {
            return false;
        }
        let entries = self.prompt_history_entries().to_vec();
        let Some(text) = self.prompt_history_walk.step_forward(&entries) else {
            return false;
        };
        self.apply_recalled_prompt(text, window, cx);
        true
    }

    fn apply_recalled_prompt(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        // Armed before the write, because `set_content` emits `Edited`
        // synchronously and the subscriber would otherwise read the recall as
        // the user typing.
        self.prompt_history_edit_from_recall = true;
        self.composer.update(cx, |input, cx| {
            input.set_content(text, cx);
            // Recall is only useful if it can be edited straight away.
            input.focus_handle(cx).focus(window, cx);
        });
        // Keep the visible draft in step with what the composer now holds, so
        // switching tasks mid-recall does not strand the old text.
        self.capture_and_save_current_composer_draft(cx);
        cx.notify();
    }

    pub(super) fn selected_composer_draft_key(
        &self,
    ) -> Option<crate::persistence::ComposerDraftKey> {
        self.state
            .selected_session
            .and_then(|selected| {
                self.state
                    .sessions
                    .iter()
                    .find(|session| session.id == selected)
            })
            .map(crate::persistence::ComposerDraftKey::for_session)
    }

    fn current_composer_draft(&self, cx: &App) -> crate::persistence::ComposerDraft {
        crate::persistence::ComposerDraft {
            text: self.composer.read(cx).content(cx).to_owned(),
            attachments: self
                .composer_attachments
                .iter()
                .map(crate::persistence::ComposerDraftAttachment::from)
                .collect(),
        }
    }

    /// Copy the visible composer into its in-memory slot. No I/O happens here;
    /// callers can use this on every real edit and on navigation boundaries.
    pub(super) fn capture_current_composer_draft(&mut self, cx: &App) -> bool {
        let Some(key) = self.selected_composer_draft_key() else {
            return false;
        };
        let draft = self.current_composer_draft(cx);
        self.composer_drafts.set(key, draft)
    }

    pub(super) fn capture_and_save_current_composer_draft(&mut self, cx: &mut Context<Self>) {
        if self.capture_current_composer_draft(cx) {
            self.schedule_composer_draft_save(cx);
        }
    }

    /// A project choice in the composer changes where the current unsent task
    /// will run; it is not ordinary task navigation. Carry its draft into a
    /// blank destination instead of letting session activation clear it.
    fn move_composer_draft_after_project_change(
        &mut self,
        source: Option<crate::persistence::ComposerDraftKey>,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = source else {
            return;
        };
        let Some(destination) = self.selected_composer_draft_key() else {
            return;
        };
        if self.composer_drafts.move_to_empty(source, destination) {
            self.restore_selected_composer_draft(cx);
            self.schedule_composer_draft_save(cx);
        }
    }

    pub(super) fn select_project_from_composer(
        &mut self,
        project_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        let source = self.selected_composer_draft_key();
        self.select_project(project_id, cx);
        self.move_composer_draft_after_project_change(source, cx);
    }

    pub(super) fn create_projectless_session_from_composer(&mut self, cx: &mut Context<Self>) {
        let source = self.selected_composer_draft_key();
        self.create_projectless_session(cx);
        self.move_composer_draft_after_project_change(source, cx);
    }

    /// Submission consumes the active draft before a blank session gains a
    /// durable session identity, so its project-scoped text cannot reappear
    /// the next time the user opens New Task.
    pub(super) fn discard_current_composer_draft(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.selected_composer_draft_key() else {
            return;
        };
        if self.composer_drafts.remove(key) {
            self.schedule_composer_draft_save(cx);
        }
    }

    pub(super) fn remove_composer_draft(
        &mut self,
        key: crate::persistence::ComposerDraftKey,
        cx: &mut Context<Self>,
    ) {
        if self.composer_drafts.remove(key) {
            self.schedule_composer_draft_save(cx);
        }
    }

    /// Replace the global input entity with the newly selected task's draft.
    /// The lookup is entirely in memory; attachments carry their cached file
    /// metadata so a session switch never stats their paths.
    pub(super) fn restore_selected_composer_draft(&mut self, cx: &mut Context<Self>) {
        let draft = self
            .selected_composer_draft_key()
            .and_then(|key| self.composer_drafts.get(key))
            .cloned()
            .unwrap_or_default();
        self.composer_attachments = draft
            .attachments
            .into_iter()
            .map(ComposerAttachment::from)
            .collect();
        self.composer
            .update(cx, |input, cx| input.set_content(draft.text, cx));
        cx.notify();
    }

    /// Debounce disk traffic while keeping serialization and filesystem I/O on
    /// the background executor. Generation checks also make detached timers
    /// and out-of-order writes harmless.
    pub(super) fn schedule_composer_draft_save(&mut self, cx: &mut Context<Self>) {
        self.composer_draft_save_generation = self.composer_draft_save_generation.saturating_add(1);
        let generation = self.composer_draft_save_generation;
        cx.spawn(async move |waku, cx| {
            cx.background_executor()
                .timer(COMPOSER_DRAFT_SAVE_DELAY)
                .await;
            let Some((store, drafts)) = waku
                .update(cx, |waku, cx| {
                    if waku.composer_draft_save_generation != generation {
                        return None;
                    }
                    waku.capture_current_composer_draft(cx);
                    Some((
                        waku.composer_draft_store.clone(),
                        waku.composer_drafts.clone(),
                    ))
                })
                .ok()
                .flatten()
            else {
                return;
            };
            let result = cx
                .background_executor()
                .spawn(async move { store.save(drafts, generation) })
                .await;
            if let Err(error) = result {
                let _ = waku.update(cx, |waku, cx| {
                    waku.show_toast(tr!("errors.save_local_state", error = error));
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

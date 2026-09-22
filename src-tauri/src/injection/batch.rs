use super::*;

const VERIFY_ATTEMPTS: usize = 50;

fn safe<B: Backend>(
    backend: &B,
    target: &TargetWindow,
    activity: Option<(&InputMonitor, u64)>,
    policy: SafetyPolicy,
) -> bool {
    activity.is_none_or(|(monitor, checkpoint)| monitor.unchanged_since(checkpoint))
        && backend.validate_target(target).is_ok()
        && backend
            .ime_composition_active(target)
            .is_ok_and(|active| policy.permits_ime(active))
}

fn copy_only<B: Backend>(backend: &B, text: &str) -> Result<InsertResult, InjectionError> {
    backend.clipboard_write(text, ClipboardExclusion::ExcludeFromHistory)?;
    Ok(InsertResult::ClipboardOnly)
}

fn select_verified<B: Backend>(
    backend: &B,
    target: &TargetWindow,
    before: &TargetText,
    text: &str,
    activity: Option<(&InputMonitor, u64)>,
) -> Option<TargetText> {
    let selected = before.select_recent(text)?;
    if !safe(backend, target, activity, SafetyPolicy::Destructive)
        || !backend.target_text(target).ok()?.same_content(before)
        || !backend.select_recent(target, before, text).ok()?
    {
        return None;
    }
    for attempt in 0..=VERIFY_ATTEMPTS {
        if attempt > 0 {
            backend.wait_for_target();
        }
        if !safe(backend, target, activity, SafetyPolicy::Destructive) {
            return None;
        }
        let actual = backend.target_text(target).ok()?;
        if actual.same_content(&selected) {
            return Some(selected);
        }
        // Only the unchanged caret state may precede the requested selection.
        // An unrelated edit or selection must never authorize replacement.
        if !actual.same_content(before) {
            return None;
        }
    }
    None
}

/// Once any paste input is queued, its clipboard payload must remain intact
/// until the target confirms the edit. Never substitute a retry or fallback.
fn paste<B: Backend>(
    backend: &B,
    options: InjectionOptions,
    text: &str,
    target: &TargetWindow,
    before: &TargetText,
    activity: Option<(&InputMonitor, u64)>,
    policy: SafetyPolicy,
) -> Result<InsertResult, InjectionError> {
    let previous = if options.restore_clipboard {
        Some(backend.clipboard_snapshot()?)
    } else {
        None
    };
    if !safe(backend, target, activity, policy)
        || !backend
            .target_text(target)
            .is_ok_and(|actual| actual.same_content(before))
    {
        return copy_only(backend, text);
    }
    let sequence = backend.clipboard_write(text, ClipboardExclusion::ExcludeFromHistory)?;
    if !safe(backend, target, activity, policy)
        || !backend
            .target_text(target)
            .is_ok_and(|actual| actual.same_content(before))
    {
        return copy_only(backend, text);
    }
    if !backend.paste(target, policy).unwrap_or(false) {
        return copy_only(backend, text);
    }
    let expected = before.replaced_with(text);
    for attempt in 0..=VERIFY_ATTEMPTS {
        if attempt > 0 {
            backend.wait_for_target();
        }
        if !safe(backend, target, activity, policy) {
            break;
        }
        if backend
            .target_text(target)
            .is_ok_and(|actual| actual.same_content(&expected))
        {
            if let Some(previous) = previous {
                // Restoration failure cannot undo a confirmed edit and must
                // never initiate a second insertion. New clipboard copies win.
                let _ = backend.clipboard_restore(previous, sequence);
            }
            return Ok(InsertResult::ClipboardPaste);
        }
    }
    Ok(InsertResult::PasteUnverified)
}

fn paste_unverified<B: Backend>(
    backend: &B,
    text: &str,
    target: &TargetWindow,
) -> Result<InsertResult, InjectionError> {
    backend.clipboard_write(text, ClipboardExclusion::ExcludeFromHistory)?;
    if backend
        .paste(target, SafetyPolicy::Additive)
        .unwrap_or(false)
    {
        Ok(InsertResult::PasteUnverified)
    } else {
        Ok(InsertResult::ClipboardOnly)
    }
}

pub(super) fn insert<B: Backend>(
    backend: &B,
    options: InjectionOptions,
    text: &str,
    target: &TargetWindow,
) -> Result<InsertResult, InjectionError> {
    if !safe(backend, target, None, SafetyPolicy::Additive) {
        return copy_only(backend, text);
    }
    let Ok(before) = backend.target_text(target) else {
        return paste_unverified(backend, text, target);
    };
    paste(
        backend,
        options,
        text,
        target,
        &before,
        None,
        SafetyPolicy::Additive,
    )
}

pub(super) fn replace_selection<B: Backend>(
    backend: &B,
    options: InjectionOptions,
    target: &TargetWindow,
    before: &TargetText,
    text: &str,
    monitor: &InputMonitor,
    checkpoint: u64,
) -> Result<InsertResult, InjectionError> {
    paste(
        backend,
        options,
        text,
        target,
        before,
        Some((monitor, checkpoint)),
        SafetyPolicy::Destructive,
    )
}

pub(super) fn begin<B: Backend>(
    backend: &B,
    options: InjectionOptions,
    draft: &str,
    target: &TargetWindow,
    monitor: &InputMonitor,
) -> Result<Option<ProvisionalInsertion>, InjectionError> {
    let checkpoint = monitor.start().then(|| monitor.checkpoint()).flatten();
    begin_with_checkpoint(backend, options, draft, target, monitor, checkpoint, true)
}

pub(super) fn begin_live<B: Backend>(
    backend: &B,
    options: InjectionOptions,
    draft: &str,
    target: &TargetWindow,
    monitor: &InputMonitor,
    checkpoint: u64,
) -> Result<Option<ProvisionalInsertion>, InjectionError> {
    begin_with_checkpoint(
        backend,
        options,
        draft,
        target,
        monitor,
        Some(checkpoint),
        false,
    )
}

fn begin_with_checkpoint<B: Backend>(
    backend: &B,
    options: InjectionOptions,
    draft: &str,
    target: &TargetWindow,
    monitor: &InputMonitor,
    checkpoint: Option<u64>,
    allow_untracked: bool,
) -> Result<Option<ProvisionalInsertion>, InjectionError> {
    if draft.is_empty() {
        return Ok(None);
    }
    let replacement = checkpoint
        .filter(|checkpoint| {
            safe(
                backend,
                target,
                Some((monitor, *checkpoint)),
                SafetyPolicy::Destructive,
            )
        })
        .and_then(|checkpoint| {
            backend
                .target_text(target)
                .ok()
                .map(|before| (checkpoint, before))
        });
    let (result, replacement) = if let Some((checkpoint, before)) = replacement {
        let result = paste(
            backend,
            options,
            draft,
            target,
            &before,
            Some((monitor, checkpoint)),
            SafetyPolicy::Destructive,
        )?;
        let range = (result != InsertResult::ClipboardOnly).then(|| ReplacementRange {
            after: before.replaced_with(draft),
            checkpoint,
        });
        (result, range)
    } else if allow_untracked {
        // Inserting the local result does not require permission to replace it
        // later. Reuse ordinary insertion guards, and never retry on completion.
        (insert(backend, options, draft, target)?, None)
    } else {
        // A live hypothesis must remain replaceable. Do not paste a fragment
        // into an unreadable target or bypass cancellation via additive input.
        (InsertResult::ClipboardOnly, None)
    };
    Ok(Some(ProvisionalInsertion {
        target: target.clone(),
        displayed: draft.to_owned(),
        replacement,
        result,
        finished: false,
    }))
}

pub(super) fn finish<B: Backend>(
    backend: &B,
    options: InjectionOptions,
    session: &mut ProvisionalInsertion,
    final_text: &str,
    monitor: &InputMonitor,
) -> Result<InsertResult, InjectionError> {
    let result = update(backend, options, session, final_text, monitor, true);
    session.finished = true;
    result
}

pub(super) fn update<B: Backend>(
    backend: &B,
    options: InjectionOptions,
    session: &mut ProvisionalInsertion,
    final_text: &str,
    monitor: &InputMonitor,
    finalizing: bool,
) -> Result<InsertResult, InjectionError> {
    if session.finished {
        return Ok(session.result);
    }
    let Some(range) = session.replacement.as_ref() else {
        if finalizing
            && session.result != InsertResult::PasteUnverified
            && (session.result == InsertResult::ClipboardOnly
                || normalize_text(&session.displayed) != normalize_text(final_text))
        {
            session.result = copy_only(backend, final_text)?;
        }
        return Ok(session.result);
    };
    if session.result == InsertResult::PasteUnverified {
        // The target may have processed the draft while correction was running.
        // Confirm the complete edit before touching a possibly pending payload.
        if !safe(
            backend,
            &session.target,
            Some((monitor, range.checkpoint)),
            SafetyPolicy::Destructive,
        ) || !backend
            .target_text(&session.target)
            .is_ok_and(|actual| actual.same_content(&range.after))
        {
            return Ok(session.result);
        }
        session.result = InsertResult::ClipboardPaste;
    }
    if normalize_text(&session.displayed) == normalize_text(final_text) {
        return Ok(session.result);
    }
    let activity = Some((monitor, range.checkpoint));
    let Some(selected) = select_verified(
        backend,
        &session.target,
        &range.after,
        &session.displayed,
        activity,
    ) else {
        session.replacement = None;
        session.result = if finalizing {
            copy_only(backend, final_text)?
        } else {
            InsertResult::ClipboardOnly
        };
        return Ok(session.result);
    };
    let checkpoint = range.checkpoint;
    // A failure after selecting must not authorize another target edit.
    session.replacement = None;
    session.result = paste(
        backend,
        options,
        final_text,
        &session.target,
        &selected,
        activity,
        SafetyPolicy::Destructive,
    )?;
    if session.result != InsertResult::ClipboardOnly {
        session.displayed = final_text.to_owned();
        session.replacement = Some(ReplacementRange {
            after: selected.replaced_with(final_text),
            checkpoint,
        });
    }
    Ok(session.result)
}

pub(super) fn cancel<B: Backend>(
    backend: &B,
    session: &mut ProvisionalInsertion,
    monitor: &InputMonitor,
) {
    if session.finished || session.result != InsertResult::ClipboardPaste {
        return;
    }
    session.finished = true;
    let Some(range) = session.replacement.as_ref() else {
        return;
    };
    let activity = Some((monitor, range.checkpoint));
    let Some(selected) = select_verified(
        backend,
        &session.target,
        &range.after,
        &session.displayed,
        activity,
    ) else {
        return;
    };
    if safe(
        backend,
        &session.target,
        activity,
        SafetyPolicy::Destructive,
    ) && backend
        .target_text(&session.target)
        .is_ok_and(|actual| actual.same_content(&selected))
    {
        let _ = backend.delete_selection(&session.target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[test]
    fn live_updates_replace_the_latest_range_and_finalization_is_idempotent() {
        let backend = MockBackend::new();
        let monitor = monitor();
        let mut session = begin(
            &backend,
            InjectionOptions::default(),
            "今日",
            &target(),
            &monitor,
        )
        .unwrap()
        .unwrap();
        for text in ["今日は雨", "今日は晴れです。", "今日は晴れです。"] {
            assert_eq!(
                update(
                    &backend,
                    InjectionOptions::default(),
                    &mut session,
                    text,
                    &monitor,
                    false
                )
                .unwrap(),
                InsertResult::ClipboardPaste
            );
            assert_eq!(backend.content(), format!("prefix {text} suffix"));
        }
        finish(
            &backend,
            InjectionOptions::default(),
            &mut session,
            "今日は晴れです！",
            &monitor,
        )
        .unwrap();
        finish(
            &backend,
            InjectionOptions::default(),
            &mut session,
            "duplicate",
            &monitor,
        )
        .unwrap();
        assert_eq!(backend.content(), "prefix 今日は晴れです！ suffix");
        assert_eq!(
            backend
                .calls
                .borrow()
                .iter()
                .filter(|c| **c == "paste")
                .count(),
            4
        );
    }

    #[test]
    fn cancel_after_live_revision_removes_only_the_latest_draft() {
        let backend = MockBackend::new();
        let monitor = monitor();
        let mut session = begin(
            &backend,
            InjectionOptions::default(),
            "draft",
            &target(),
            &monitor,
        )
        .unwrap()
        .unwrap();
        update(
            &backend,
            InjectionOptions::default(),
            &mut session,
            "revised draft",
            &monitor,
            false,
        )
        .unwrap();
        cancel(&backend, &mut session, &monitor);
        assert_eq!(backend.content(), "prefix  suffix");
    }

    #[test]
    fn cancellation_during_live_selection_wait_prevents_replacement_paste() {
        let mut backend = MockBackend::new();
        let monitor = monitor();
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        monitor.observe_cancellation(Some(cancelled));
        let mut session = begin_live(
            &backend,
            InjectionOptions::default(),
            "draft",
            &target(),
            &monitor,
            monitor.checkpoint().unwrap(),
        )
        .unwrap()
        .unwrap();
        backend.selection_settle_after = 2;
        backend.on_selection_wait = Some(Box::new(move |_| {
            cancel.send_replace(true);
        }));
        update(
            &backend,
            InjectionOptions::default(),
            &mut session,
            "late replacement",
            &monitor,
            false,
        )
        .unwrap();
        assert_eq!(backend.content(), "prefix draft suffix");
        assert_eq!(
            backend
                .calls
                .borrow()
                .iter()
                .filter(|c| **c == "paste")
                .count(),
            1
        );
    }

    #[test]
    fn live_begin_never_uses_untracked_input_or_refreshes_an_interrupted_checkpoint() {
        for reason in ["cancel", "input", "unreadable"] {
            let backend = MockBackend::new();
            let monitor = monitor();
            let checkpoint = monitor.checkpoint().unwrap();
            let (cancel, cancelled) = tokio::sync::watch::channel(false);
            monitor.observe_cancellation(Some(cancelled));
            match reason {
                "cancel" => {
                    cancel.send_replace(true);
                }
                "input" => monitor.test_record_input(),
                "unreadable" => backend.text_readable.set(false),
                _ => unreachable!(),
            }
            let mut session = begin_live(
                &backend,
                InjectionOptions::default(),
                "fragment",
                &target(),
                &monitor,
                checkpoint,
            )
            .unwrap()
            .unwrap();
            assert!(backend.calls.borrow().is_empty());
            finish(
                &backend,
                InjectionOptions::default(),
                &mut session,
                "complete",
                &monitor,
            )
            .unwrap();
            assert_eq!(backend.content(), "prefix  suffix");
            assert_eq!(&*backend.clipboard.borrow(), "complete");
        }
    }

    #[test]
    fn live_edit_interruption_freezes_target_and_only_final_text_is_copied() {
        let backend = MockBackend::new();
        let monitor = monitor();
        let mut session = begin(
            &backend,
            InjectionOptions::default(),
            "draft",
            &target(),
            &monitor,
        )
        .unwrap()
        .unwrap();
        monitor.test_record_input();
        for text in ["partial one", "partial two"] {
            assert_eq!(
                update(
                    &backend,
                    InjectionOptions::default(),
                    &mut session,
                    text,
                    &monitor,
                    false
                )
                .unwrap(),
                InsertResult::ClipboardOnly
            );
            assert_eq!(backend.content(), "prefix draft suffix");
            assert_eq!(&*backend.clipboard.borrow(), "original rich clipboard");
        }
        finish(
            &backend,
            InjectionOptions::default(),
            &mut session,
            "draft",
            &monitor,
        )
        .unwrap();
        assert_eq!(&*backend.clipboard.borrow(), "draft");
    }

    struct MockBackend {
        text: RefCell<TargetText>,
        clipboard: RefCell<String>,
        calls: RefCell<Vec<&'static str>>,
        sequence: Cell<u32>,
        target_valid: Cell<bool>,
        ime: Cell<Option<bool>>,
        text_readable: Cell<bool>,
        paste_accepted: Cell<bool>,
        pending: RefCell<Option<String>>,
        pending_selection: RefCell<Option<TargetText>>,
        selection_settle_after: usize,
        selection_waits: Cell<usize>,
        on_selection_wait: Option<Box<dyn Fn(&MockBackend)>>,
        exclusions: RefCell<Vec<ClipboardExclusion>>,
        settle_after: usize,
        waits: Cell<usize>,
        change_clipboard_on_paste: bool,
        change_identity_on_paste: bool,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                text: RefCell::new(TargetText {
                    identity: vec![1],
                    before: "prefix ".into(),
                    selected: String::new(),
                    after: " suffix".into(),
                }),
                clipboard: RefCell::new("original rich clipboard".into()),
                calls: RefCell::new(Vec::new()),
                sequence: Cell::new(0),
                target_valid: Cell::new(true),
                ime: Cell::new(Some(false)),
                text_readable: Cell::new(true),
                paste_accepted: Cell::new(true),
                pending: RefCell::new(None),
                pending_selection: RefCell::new(None),
                selection_settle_after: 0,
                selection_waits: Cell::new(0),
                on_selection_wait: None,
                exclusions: RefCell::new(Vec::new()),
                settle_after: 0,
                waits: Cell::new(0),
                change_clipboard_on_paste: false,
                change_identity_on_paste: false,
            }
        }
        fn apply_pending(&self) {
            if let Some(text) = self.pending.borrow_mut().take() {
                let next = self.text.borrow().replaced_with(&text);
                *self.text.borrow_mut() = next;
                if self.change_identity_on_paste {
                    self.text.borrow_mut().identity.push(2);
                }
            }
        }
        fn content(&self) -> String {
            let text = self.text.borrow();
            format!("{}{}{}", text.before, text.selected, text.after)
        }
    }

    impl Backend for MockBackend {
        type Clipboard = String;
        fn capture_target(&self) -> Result<TargetWindow, InjectionError> {
            Ok(target())
        }
        fn validate_target(&self, _: &TargetWindow) -> Result<(), InjectionError> {
            if self.target_valid.get() {
                Ok(())
            } else {
                Err(InjectionError::TargetChanged)
            }
        }
        fn ime_composition_active(&self, _: &TargetWindow) -> Result<Option<bool>, InjectionError> {
            Ok(self.ime.get())
        }
        fn target_text(&self, _: &TargetWindow) -> Result<TargetText, InjectionError> {
            if self.text_readable.get() {
                Ok(self.text.borrow().clone())
            } else {
                Err(InjectionError::BackendFailure("unreadable target text"))
            }
        }
        fn select_recent(
            &self,
            _: &TargetWindow,
            state: &TargetText,
            text: &str,
        ) -> Result<bool, InjectionError> {
            self.calls.borrow_mut().push("select");
            let Some(selected) = state.select_recent(text) else {
                return Ok(false);
            };
            if self.selection_settle_after == 0 {
                *self.text.borrow_mut() = selected;
            } else {
                *self.pending_selection.borrow_mut() = Some(selected);
            }
            Ok(true)
        }
        fn clipboard_snapshot(&self) -> Result<Self::Clipboard, InjectionError> {
            Ok(self.clipboard.borrow().clone())
        }
        fn clipboard_write(
            &self,
            text: &str,
            exclusion: ClipboardExclusion,
        ) -> Result<u32, InjectionError> {
            self.calls.borrow_mut().push("write");
            *self.clipboard.borrow_mut() = text.into();
            self.exclusions.borrow_mut().push(exclusion);
            self.sequence.set(self.sequence.get() + 1);
            Ok(self.sequence.get())
        }
        fn clipboard_restore(
            &self,
            snapshot: Self::Clipboard,
            expected: u32,
        ) -> Result<bool, InjectionError> {
            self.calls.borrow_mut().push("restore");
            if self.sequence.get() != expected {
                return Ok(false);
            }
            *self.clipboard.borrow_mut() = snapshot;
            Ok(true)
        }
        fn paste(&self, _: &TargetWindow, _: SafetyPolicy) -> Result<bool, InjectionError> {
            self.calls.borrow_mut().push("paste");
            if !self.paste_accepted.get() {
                return Ok(false);
            }
            *self.pending.borrow_mut() = Some(self.clipboard.borrow().clone());
            if self.settle_after == 0 {
                self.apply_pending();
            }
            if self.change_clipboard_on_paste {
                *self.clipboard.borrow_mut() = "new user copy".into();
                self.sequence.set(self.sequence.get() + 1);
            }
            Ok(true)
        }
        fn delete_selection(&self, _: &TargetWindow) -> Result<bool, InjectionError> {
            self.text.borrow_mut().selected.clear();
            self.calls.borrow_mut().push("delete");
            Ok(true)
        }
        fn wait_for_target(&self) {
            if self.pending_selection.borrow().is_some() {
                self.selection_waits.set(self.selection_waits.get() + 1);
                if self.selection_waits.get() == 1 {
                    if let Some(on_wait) = &self.on_selection_wait {
                        on_wait(self);
                    }
                }
                if self.selection_waits.get() == self.selection_settle_after {
                    *self.text.borrow_mut() = self.pending_selection.borrow_mut().take().unwrap();
                }
            }
            self.waits.set(self.waits.get() + 1);
            if self.waits.get() == self.settle_after {
                self.apply_pending();
            }
        }
    }
    fn target() -> TargetWindow {
        TargetWindow {
            window_handle: 1,
            control_handle: 2,
            process_id: 3,
            thread_id: 4,
            is_secure: false,
        }
    }
    fn monitor() -> InputMonitor {
        let monitor = InputMonitor::default();
        monitor.test_set_available(true);
        monitor
    }

    #[test]
    fn selection_verification_rejects_empty_source() {
        let state = TargetText {
            identity: vec![1],
            before: "prefix ".into(),
            selected: String::new(),
            after: " suffix".into(),
        };
        assert!(state.select_recent("").is_none());
    }

    fn selected_state() -> TargetText {
        TargetText {
            identity: vec![1],
            before: "prefix ".into(),
            selected: "original".into(),
            after: " suffix".into(),
        }
    }

    #[test]
    fn unchanged_captured_selection_is_replaced_once() {
        let backend = MockBackend::new();
        *backend.text.borrow_mut() = selected_state();
        let before = backend.text.borrow().clone();
        let monitor = monitor();
        let checkpoint = monitor.checkpoint().unwrap();

        assert_eq!(
            replace_selection(
                &backend,
                InjectionOptions::default(),
                &target(),
                &before,
                "edited",
                &monitor,
                checkpoint,
            )
            .unwrap(),
            InsertResult::ClipboardPaste
        );
        assert_eq!(backend.content(), "prefix edited suffix");
        assert_eq!(
            backend
                .calls
                .borrow()
                .iter()
                .filter(|call| **call == "paste")
                .count(),
            1
        );
    }

    #[test]
    fn changed_selection_context_falls_back_to_clipboard_without_target_edits() {
        for change in [
            "focus",
            "selection",
            "input",
            "shortcut",
            "ime_active",
            "ime_unknown",
        ] {
            let backend = MockBackend::new();
            *backend.text.borrow_mut() = selected_state();
            let before = backend.text.borrow().clone();
            let monitor = monitor();
            let checkpoint = monitor.checkpoint().unwrap();
            match change {
                "focus" => backend.target_valid.set(false),
                "selection" => backend.text.borrow_mut().selected.push('!'),
                "input" => monitor.test_record_input(),
                "shortcut" => monitor.test_set_shortcut_pending(true),
                "ime_active" => backend.ime.set(Some(true)),
                "ime_unknown" => backend.ime.set(None),
                _ => unreachable!(),
            }
            let expected = backend.content();

            assert_eq!(
                replace_selection(
                    &backend,
                    InjectionOptions::default(),
                    &target(),
                    &before,
                    "edited",
                    &monitor,
                    checkpoint,
                )
                .unwrap(),
                InsertResult::ClipboardOnly,
                "{change}"
            );
            assert_eq!(backend.content(), expected, "{change}");
            assert_eq!(&*backend.clipboard.borrow(), "edited", "{change}");
            assert!(!backend.calls.borrow().contains(&"paste"), "{change}");
        }
    }

    #[test]
    fn unconfirmed_selection_paste_is_not_retried_or_restored() {
        let mut backend = MockBackend::new();
        *backend.text.borrow_mut() = selected_state();
        backend.settle_after = VERIFY_ATTEMPTS + 2;
        let before = backend.text.borrow().clone();
        let monitor = monitor();
        let checkpoint = monitor.checkpoint().unwrap();

        assert_eq!(
            replace_selection(
                &backend,
                InjectionOptions::default(),
                &target(),
                &before,
                "edited",
                &monitor,
                checkpoint,
            )
            .unwrap(),
            InsertResult::PasteUnverified
        );
        assert_eq!(&*backend.clipboard.borrow(), "edited");
        assert_eq!(backend.content(), "prefix original suffix");
        assert_eq!(
            backend
                .calls
                .borrow()
                .iter()
                .filter(|call| **call == "paste")
                .count(),
            1
        );
        assert!(!backend.calls.borrow().contains(&"restore"));
    }

    #[test]
    fn a_failed_paste_is_not_retried_as_character_input() {
        let backend = MockBackend::new();
        backend.paste_accepted.set(false);
        assert_eq!(
            insert(&backend, InjectionOptions::default(), "batch", &target()).unwrap(),
            InsertResult::ClipboardOnly
        );
        assert_eq!(backend.content(), "prefix  suffix");
        assert_eq!(
            backend
                .calls
                .borrow()
                .iter()
                .filter(|c| **c == "paste")
                .count(),
            1
        );
    }
    #[test]
    fn unknown_ime_and_unreadable_text_uses_one_unverified_paste_without_restore() {
        let backend = MockBackend::new();
        backend.ime.set(None);
        backend.text_readable.set(false);

        assert_eq!(
            insert(&backend, InjectionOptions::default(), "batch", &target()).unwrap(),
            InsertResult::PasteUnverified
        );
        assert_eq!(&*backend.clipboard.borrow(), "batch");
        assert_eq!(backend.calls.borrow().as_slice(), ["write", "paste"]);
        assert_eq!(
            backend.exclusions.borrow().as_slice(),
            [ClipboardExclusion::ExcludeFromHistory]
        );
    }
    #[test]
    fn clipboard_only_excludes_transcript_from_history() {
        let backend = MockBackend::new();
        backend.target_valid.set(false);

        assert_eq!(
            insert(&backend, InjectionOptions::default(), "batch", &target()).unwrap(),
            InsertResult::ClipboardOnly
        );
        assert_eq!(
            backend.exclusions.borrow().as_slice(),
            [ClipboardExclusion::ExcludeFromHistory]
        );
    }
    #[test]
    fn unreadable_target_receives_draft_before_correction_finishes() {
        let backend = MockBackend::new();
        backend.text_readable.set(false);
        let monitor = monitor();
        let session = begin(
            &backend,
            InjectionOptions::default(),
            "draft",
            &target(),
            &monitor,
        )
        .unwrap();
        assert_eq!(backend.content(), "prefix draft suffix");
        let mut session = session.expect("queued draft must prevent a second insertion");
        assert_eq!(
            finish(
                &backend,
                InjectionOptions::default(),
                &mut session,
                "corrected",
                &monitor,
            )
            .unwrap(),
            InsertResult::PasteUnverified,
        );
        assert_eq!(backend.calls.borrow().as_slice(), ["write", "paste"]);
        assert_eq!(&*backend.clipboard.borrow(), "draft");
    }

    #[test]
    fn untracked_draft_is_attempted_once_and_never_replaced_or_retried() {
        for condition in [
            "unknown_ime",
            "monitor",
            "focus",
            "active_ime",
            "paste_rejected",
        ] {
            for corrected in ["draft", "corrected"] {
                let backend = MockBackend::new();
                let monitor = monitor();
                match condition {
                    "unknown_ime" => backend.ime.set(None),
                    "monitor" => monitor.test_set_available(false),
                    "focus" => backend.target_valid.set(false),
                    "active_ime" => backend.ime.set(Some(true)),
                    "paste_rejected" => backend.paste_accepted.set(false),
                    _ => unreachable!(),
                }
                let mut session = begin(
                    &backend,
                    InjectionOptions::default(),
                    "draft",
                    &target(),
                    &monitor,
                )
                .unwrap()
                .unwrap();
                let inserted = matches!(condition, "unknown_ime" | "monitor");
                assert_eq!(session.paste_was_queued(), inserted, "{condition}");
                let expected = if inserted {
                    "prefix draft suffix"
                } else {
                    "prefix  suffix"
                };
                assert_eq!(backend.content(), expected, "{condition}");
                if !inserted {
                    assert_eq!(&*backend.clipboard.borrow(), "draft");
                }
                let paste_count = backend
                    .calls
                    .borrow()
                    .iter()
                    .filter(|c| **c == "paste")
                    .count();
                // Recovery of tracking/target access must not authorize a late paste.
                backend.ime.set(Some(false));
                backend.target_valid.set(true);
                backend.paste_accepted.set(true);
                monitor.test_set_available(true);
                let result = finish(
                    &backend,
                    InjectionOptions::default(),
                    &mut session,
                    corrected,
                    &monitor,
                )
                .unwrap();
                assert_eq!(
                    result,
                    if inserted && corrected == "draft" {
                        InsertResult::ClipboardPaste
                    } else {
                        InsertResult::ClipboardOnly
                    },
                    "{condition}"
                );
                let calls = backend.calls.borrow().clone();
                finish(
                    &backend,
                    InjectionOptions::default(),
                    &mut session,
                    corrected,
                    &monitor,
                )
                .unwrap();
                cancel(&backend, &mut session, &monitor);
                assert_eq!(*backend.calls.borrow(), calls);
                assert_eq!(backend.content(), expected);
                assert_eq!(
                    backend
                        .calls
                        .borrow()
                        .iter()
                        .filter(|c| **c == "paste")
                        .count(),
                    paste_count
                );
                assert!(!backend.calls.borrow().contains(&"select"));
                if corrected != "draft" {
                    assert_eq!(&*backend.clipboard.borrow(), corrected);
                }
            }
        }
    }

    #[test]
    fn delayed_selection_is_confirmed_before_corrected_paste() {
        for delay in [1, VERIFY_ATTEMPTS] {
            let mut backend = MockBackend::new();
            backend.selection_settle_after = delay;
            backend.on_selection_wait = Some(Box::new(|backend| {
                assert_eq!(&*backend.clipboard.borrow(), "original rich clipboard");
                assert_eq!(
                    backend
                        .calls
                        .borrow()
                        .iter()
                        .filter(|call| **call == "paste")
                        .count(),
                    1
                );
            }));
            let monitor = monitor();
            let mut session = begin(
                &backend,
                InjectionOptions::default(),
                "draft",
                &target(),
                &monitor,
            )
            .unwrap()
            .unwrap();
            assert_eq!(backend.content(), "prefix draft suffix");
            finish(
                &backend,
                InjectionOptions::default(),
                &mut session,
                "corrected",
                &monitor,
            )
            .unwrap();
            assert_eq!(backend.content(), "prefix corrected suffix");
            assert_eq!(
                backend
                    .calls
                    .borrow()
                    .iter()
                    .filter(|call| **call == "select")
                    .count(),
                1
            );
            let calls = backend.calls.borrow().clone();
            finish(
                &backend,
                InjectionOptions::default(),
                &mut session,
                "corrected",
                &monitor,
            )
            .unwrap();
            assert_eq!(*backend.calls.borrow(), calls);
        }
    }

    #[test]
    fn unconfirmed_or_interrupted_selection_never_replaces_or_deletes_text() {
        for interruption in [
            "timeout",
            "input",
            "monitor",
            "focus",
            "text",
            "selection",
            "active_ime",
            "unknown_ime",
            "unreadable",
        ] {
            for cancelling in [false, true] {
                let monitor = std::sync::Arc::new(monitor());
                let observed_monitor = monitor.clone();
                let mut backend = MockBackend::new();
                backend.selection_settle_after = VERIFY_ATTEMPTS + 1;
                backend.on_selection_wait = Some(Box::new(move |backend| match interruption {
                    "input" => observed_monitor.test_record_input(),
                    "monitor" => observed_monitor.test_set_available(false),
                    "focus" => backend.target_valid.set(false),
                    "text" => backend.text.borrow_mut().after.push('!'),
                    "selection" => {
                        backend.text.borrow_mut().before.pop();
                    }
                    "active_ime" => backend.ime.set(Some(true)),
                    "unknown_ime" => backend.ime.set(None),
                    "unreadable" => backend.text_readable.set(false),
                    _ => {}
                }));
                let mut session = begin(
                    &backend,
                    InjectionOptions::default(),
                    "draft",
                    &target(),
                    &monitor,
                )
                .unwrap()
                .unwrap();
                if cancelling {
                    cancel(&backend, &mut session, &monitor);
                } else {
                    assert_eq!(
                        finish(
                            &backend,
                            InjectionOptions::default(),
                            &mut session,
                            "corrected",
                            &monitor
                        )
                        .unwrap(),
                        InsertResult::ClipboardOnly
                    );
                    assert_eq!(&*backend.clipboard.borrow(), "corrected");
                }
                assert_eq!(
                    backend
                        .calls
                        .borrow()
                        .iter()
                        .filter(|call| **call == "select")
                        .count(),
                    1,
                    "{interruption}"
                );
                assert_eq!(
                    backend
                        .calls
                        .borrow()
                        .iter()
                        .filter(|call| **call == "paste")
                        .count(),
                    1,
                    "{interruption}"
                );
                assert!(
                    !backend.calls.borrow().contains(&"delete"),
                    "{interruption}"
                );
                assert_eq!(
                    backend.selection_waits.get(),
                    if interruption == "timeout" {
                        VERIFY_ATTEMPTS
                    } else {
                        1
                    }
                );
            }
        }
    }

    #[test]
    fn provisional_text_is_pasted_once_and_finalized_once() {
        let backend = MockBackend::new();
        let monitor = monitor();
        let mut session = begin(
            &backend,
            InjectionOptions::default(),
            "仮入力",
            &target(),
            &monitor,
        )
        .unwrap()
        .unwrap();
        assert_eq!(backend.content(), "prefix 仮入力 suffix");
        assert_eq!(&*backend.clipboard.borrow(), "original rich clipboard");
        assert_eq!(
            finish(
                &backend,
                InjectionOptions::default(),
                &mut session,
                "補正済み\n文章",
                &monitor
            )
            .unwrap(),
            InsertResult::ClipboardPaste
        );
        finish(
            &backend,
            InjectionOptions::default(),
            &mut session,
            "補正済み\n文章",
            &monitor,
        )
        .unwrap();
        assert_eq!(backend.content(), "prefix 補正済み\n文章 suffix");
        assert_eq!(
            backend
                .calls
                .borrow()
                .iter()
                .filter(|c| **c == "paste")
                .count(),
            2
        );
        assert!(!backend.calls.borrow().contains(&"delete"));
    }
    #[test]
    fn changed_element_identity_after_paste_still_confirms_and_replaces_draft() {
        let mut backend = MockBackend::new();
        backend.change_identity_on_paste = true;
        let monitor = monitor();

        let mut session = begin(
            &backend,
            InjectionOptions::default(),
            "draft",
            &target(),
            &monitor,
        )
        .unwrap()
        .unwrap();
        assert_eq!(session.result, InsertResult::ClipboardPaste);

        assert_eq!(
            finish(
                &backend,
                InjectionOptions::default(),
                &mut session,
                "corrected",
                &monitor,
            )
            .unwrap(),
            InsertResult::ClipboardPaste
        );
        assert_eq!(backend.content(), "prefix corrected suffix");
    }
    #[test]
    fn unchanged_correction_or_api_failure_keeps_the_original_batch() {
        let backend = MockBackend::new();
        let monitor = monitor();
        let mut session = begin(
            &backend,
            InjectionOptions::default(),
            "draft",
            &target(),
            &monitor,
        )
        .unwrap()
        .unwrap();
        finish(
            &backend,
            InjectionOptions::default(),
            &mut session,
            "draft",
            &monitor,
        )
        .unwrap();
        assert_eq!(backend.content(), "prefix draft suffix");
        assert_eq!(
            backend
                .calls
                .borrow()
                .iter()
                .filter(|c| **c == "paste")
                .count(),
            1
        );
    }
    #[test]
    fn clipboard_restoration_waits_for_the_target_edit() {
        let mut backend = MockBackend::new();
        backend.settle_after = VERIFY_ATTEMPTS;
        assert_eq!(
            insert(&backend, InjectionOptions::default(), "batch", &target()).unwrap(),
            InsertResult::ClipboardPaste
        );
        assert_eq!(backend.waits.get(), VERIFY_ATTEMPTS);
        assert_eq!(backend.content(), "prefix batch suffix");
        assert_eq!(&*backend.clipboard.borrow(), "original rich clipboard");
    }
    #[test]
    fn draft_confirmed_after_initial_timeout_is_replaced_once() {
        for (final_text, newer_copy) in
            [("corrected", false), ("draft", false), ("corrected", true)]
        {
            let mut backend = MockBackend::new();
            backend.settle_after = usize::MAX;
            let monitor = monitor();
            let mut session = begin(
                &backend,
                InjectionOptions::default(),
                "draft",
                &target(),
                &monitor,
            )
            .unwrap()
            .unwrap();
            assert_eq!(session.result, InsertResult::PasteUnverified);
            // The target processes the original paste while correction is running.
            backend.apply_pending();
            backend.settle_after = 0;
            if newer_copy {
                backend
                    .clipboard_write("new user copy", ClipboardExclusion::ExcludeFromHistory)
                    .unwrap();
            }
            assert_eq!(backend.content(), "prefix draft suffix");
            assert_eq!(
                finish(
                    &backend,
                    InjectionOptions::default(),
                    &mut session,
                    final_text,
                    &monitor,
                )
                .unwrap(),
                InsertResult::ClipboardPaste
            );
            finish(
                &backend,
                InjectionOptions::default(),
                &mut session,
                final_text,
                &monitor,
            )
            .unwrap();
            assert_eq!(backend.content(), format!("prefix {final_text} suffix"));
            // The original snapshot expired with the unverified paste. Restoration
            // preserves the clipboard captured at replacement time, including newer copies.
            assert_eq!(
                &*backend.clipboard.borrow(),
                if newer_copy { "new user copy" } else { "draft" }
            );
            assert_eq!(
                backend
                    .calls
                    .borrow()
                    .iter()
                    .filter(|call| **call == "paste")
                    .count(),
                if final_text == "draft" { 1 } else { 2 }
            );
        }
    }

    #[test]
    fn late_confirmation_requires_unchanged_input_target_text_and_inactive_ime() {
        for change in [
            "input",
            "monitor",
            "focus",
            "text",
            "selection",
            "ime",
            "unknown_ime",
            "unreadable",
        ] {
            let mut backend = MockBackend::new();
            backend.settle_after = usize::MAX;
            let monitor = monitor();
            let mut session = begin(
                &backend,
                InjectionOptions::default(),
                "draft",
                &target(),
                &monitor,
            )
            .unwrap()
            .unwrap();
            backend.apply_pending();
            match change {
                "input" => monitor.test_record_input(),
                "monitor" => monitor.test_set_available(false),
                "focus" => backend.target_valid.set(false),
                "text" => backend.text.borrow_mut().before.push('!'),
                "selection" => backend.text.borrow_mut().selected.push('!'),
                "ime" => backend.ime.set(Some(true)),
                "unknown_ime" => backend.ime.set(None),
                "unreadable" => backend.text_readable.set(false),
                _ => unreachable!(),
            }
            let expected = backend.content();
            assert_eq!(
                finish(
                    &backend,
                    InjectionOptions::default(),
                    &mut session,
                    "corrected",
                    &monitor,
                )
                .unwrap(),
                InsertResult::PasteUnverified,
                "{change}"
            );
            assert_eq!(backend.content(), expected, "{change}");
            assert_eq!(&*backend.clipboard.borrow(), "draft", "{change}");
            assert_eq!(
                backend.calls.borrow().as_slice(),
                ["write", "paste"],
                "{change}"
            );
        }
    }

    #[test]
    fn an_unconfirmed_paste_is_not_restored_retried_or_overwritten_by_completion() {
        let mut backend = MockBackend::new();
        backend.settle_after = usize::MAX;
        let monitor = monitor();
        let mut session = begin(
            &backend,
            InjectionOptions::default(),
            "draft",
            &target(),
            &monitor,
        )
        .unwrap()
        .unwrap();
        for text in ["partial one", "partial two"] {
            assert_eq!(
                update(
                    &backend,
                    InjectionOptions::default(),
                    &mut session,
                    text,
                    &monitor,
                    false
                )
                .unwrap(),
                InsertResult::PasteUnverified
            );
        }
        assert_eq!(
            finish(
                &backend,
                InjectionOptions::default(),
                &mut session,
                "final",
                &monitor
            )
            .unwrap(),
            InsertResult::PasteUnverified
        );
        assert_eq!(&*backend.clipboard.borrow(), "draft");
        assert_eq!(backend.content(), "prefix  suffix");
        assert_eq!(backend.calls.borrow().as_slice(), ["write", "paste"]);
    }
    #[test]
    fn newer_clipboard_content_wins_over_restoration() {
        let mut backend = MockBackend::new();
        backend.change_clipboard_on_paste = true;
        insert(&backend, InjectionOptions::default(), "batch", &target()).unwrap();
        assert_eq!(&*backend.clipboard.borrow(), "new user copy");
    }
    #[test]
    fn unsafe_or_changed_targets_are_not_replaced() {
        for change in ["input", "focus", "text", "selection", "ime"] {
            let backend = MockBackend::new();
            let monitor = monitor();
            let mut session = begin(
                &backend,
                InjectionOptions::default(),
                "draft",
                &target(),
                &monitor,
            )
            .unwrap()
            .unwrap();
            match change {
                "input" => monitor.test_record_input(),
                "focus" => backend.target_valid.set(false),
                "text" => backend.text.borrow_mut().before.push('!'),
                "selection" => backend.text.borrow_mut().selected.push('!'),
                "ime" => backend.ime.set(Some(true)),
                _ => unreachable!(),
            }
            let expected = backend.content();
            assert_eq!(
                finish(
                    &backend,
                    InjectionOptions::default(),
                    &mut session,
                    "final",
                    &monitor
                )
                .unwrap(),
                InsertResult::ClipboardOnly
            );
            assert_eq!(backend.content(), expected);
            assert!(!backend.calls.borrow().contains(&"select"));
        }
    }
    #[test]
    fn destructive_replacement_with_unknown_ime_is_not_applied() {
        let backend = MockBackend::new();
        let monitor = monitor();
        let mut session = begin(
            &backend,
            InjectionOptions::default(),
            "draft",
            &target(),
            &monitor,
        )
        .unwrap()
        .unwrap();
        backend.ime.set(None);
        let expected = backend.content();

        assert_eq!(
            finish(
                &backend,
                InjectionOptions::default(),
                &mut session,
                "final",
                &monitor
            )
            .unwrap(),
            InsertResult::ClipboardOnly
        );
        assert_eq!(backend.content(), expected);
        assert!(!backend.calls.borrow().contains(&"select"));
    }
    #[test]
    fn cancellation_removes_only_a_verified_unedited_provisional_range() {
        let backend = MockBackend::new();
        let monitor = monitor();
        let mut session = begin(
            &backend,
            InjectionOptions::default(),
            "draft",
            &target(),
            &monitor,
        )
        .unwrap()
        .unwrap();
        cancel(&backend, &mut session, &monitor);
        cancel(&backend, &mut session, &monitor);
        assert_eq!(backend.content(), "prefix  suffix");
        assert_eq!(
            backend
                .calls
                .borrow()
                .iter()
                .filter(|c| **c == "delete")
                .count(),
            1
        );
    }
}

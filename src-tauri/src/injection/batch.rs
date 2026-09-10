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
        || backend.target_text(target).ok().as_ref() != Some(before)
    {
        return copy_only(backend, text);
    }
    let sequence = backend.clipboard_write(text, ClipboardExclusion::ExcludeFromHistory)?;
    if !safe(backend, target, activity, policy)
        || backend.target_text(target).ok().as_ref() != Some(before)
    {
        return copy_only(backend, text);
    }
    if !backend.paste(target, policy).unwrap_or(false) {
        return copy_only(backend, text);
    }
    let expected = before.replaced_with(text);
    for _ in 0..VERIFY_ATTEMPTS {
        if !safe(backend, target, activity, policy) {
            break;
        }
        if backend.target_text(target).ok().as_ref() == Some(&expected) {
            if let Some(previous) = previous {
                // Restoration failure cannot undo a confirmed edit and must
                // never initiate a second insertion. New clipboard copies win.
                let _ = backend.clipboard_restore(previous, sequence);
            }
            return Ok(InsertResult::ClipboardPaste);
        }
        backend.wait_for_target();
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
    if draft.is_empty() || !monitor.start() {
        return Ok(None);
    }
    let Some(checkpoint) = monitor.checkpoint() else {
        return Ok(None);
    };
    if !safe(
        backend,
        target,
        Some((monitor, checkpoint)),
        SafetyPolicy::Destructive,
    ) {
        return Ok(None);
    }
    let Ok(before) = backend.target_text(target) else {
        return Ok(None);
    };
    let result = paste(
        backend,
        options,
        draft,
        target,
        &before,
        Some((monitor, checkpoint)),
        SafetyPolicy::Destructive,
    )?;
    if result == InsertResult::ClipboardOnly {
        return Ok(None);
    }
    Ok(Some(ProvisionalInsertion {
        target: target.clone(),
        displayed: draft.to_owned(),
        after: before.replaced_with(draft),
        checkpoint,
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
    if session.finished {
        return Ok(session.result);
    }
    session.finished = true;
    if session.result == InsertResult::PasteUnverified {
        // The original Ctrl+V may still be pending. Keep its payload unchanged;
        // the completed correction remains available in the application preview.
        return Ok(session.result);
    }
    if normalize_text(&session.displayed) == normalize_text(final_text) {
        return Ok(session.result);
    }
    let activity = Some((monitor, session.checkpoint));
    if !safe(
        backend,
        &session.target,
        activity,
        SafetyPolicy::Destructive,
    ) || backend.target_text(&session.target).ok().as_ref() != Some(&session.after)
        || !backend
            .select_recent(&session.target, &session.after, &session.displayed)
            .unwrap_or(false)
    {
        session.result = copy_only(backend, final_text)?;
        return Ok(session.result);
    }
    let selected =
        session
            .after
            .select_recent(&session.displayed)
            .ok_or(InjectionError::BackendFailure(
                "the provisional range is no longer available",
            ))?;
    session.result = paste(
        backend,
        options,
        final_text,
        &session.target,
        &selected,
        activity,
        SafetyPolicy::Destructive,
    )?;
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
    let activity = Some((monitor, session.checkpoint));
    if safe(
        backend,
        &session.target,
        activity,
        SafetyPolicy::Destructive,
    ) && backend.target_text(&session.target).ok().as_ref() == Some(&session.after)
        && backend
            .select_recent(&session.target, &session.after, &session.displayed)
            .unwrap_or(false)
        && safe(
            backend,
            &session.target,
            activity,
            SafetyPolicy::Destructive,
        )
    {
        if let Some(selected) = session.after.select_recent(&session.displayed) {
            if backend.target_text(&session.target).ok().as_ref() == Some(&selected) {
                let _ = backend.delete_selection(&session.target);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

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
        exclusions: RefCell<Vec<ClipboardExclusion>>,
        settle_after: usize,
        waits: Cell<usize>,
        change_clipboard_on_paste: bool,
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
                exclusions: RefCell::new(Vec::new()),
                settle_after: 0,
                waits: Cell::new(0),
                change_clipboard_on_paste: false,
            }
        }
        fn apply_pending(&self) {
            if let Some(text) = self.pending.borrow_mut().take() {
                let next = self.text.borrow().replaced_with(&text);
                *self.text.borrow_mut() = next;
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
            *self.text.borrow_mut() = selected;
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
        backend.settle_after = 20;
        assert_eq!(
            insert(&backend, InjectionOptions::default(), "batch", &target()).unwrap(),
            InsertResult::ClipboardPaste
        );
        assert_eq!(backend.waits.get(), 20);
        assert_eq!(backend.content(), "prefix batch suffix");
        assert_eq!(&*backend.clipboard.borrow(), "original rich clipboard");
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

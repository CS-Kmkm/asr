use super::*;
use tokio::sync::watch;

const VERIFY_ATTEMPTS: usize = 50;

#[derive(Clone, Copy, PartialEq, Eq)]
enum GuardMoment {
    BeforePaste,
    VerifyPaste,
    /// A queued paste can no longer be undone. An unavailable IME query on
    /// an additive target does not explain whether that paste was applied.
    ExplainQueued,
}

/// The ordered safety checks shared by insertion and its diagnostic report.
/// Reading clipboard state and sending input remain outside this evaluator.
fn evaluate_guard<B: Backend>(
    backend: &B,
    target: &TargetWindow,
    activity: Option<(&InputMonitor, u64)>,
    policy: SafetyPolicy,
    moment: GuardMoment,
    expected: Option<&TargetText>,
) -> Result<(), InsertionDetail> {
    if target.is_this_app() {
        return Err(InsertionDetail::NoExternalTarget);
    }
    if let Some((monitor, checkpoint)) = activity {
        let unchanged = if moment == GuardMoment::BeforePaste {
            monitor.unchanged_since(checkpoint)
        } else {
            monitor.unchanged_since_injected_paste(checkpoint)
        };
        if !unchanged {
            return Err(InsertionDetail::UserActivity);
        }
    }
    backend
        .validate_target(target)
        .map_err(|error| match error {
            InjectionError::SecureTarget | InjectionError::PrivilegeMismatch => {
                InsertionDetail::TargetProtected
            }
            _ => InsertionDetail::TargetChanged,
        })?;
    match backend.ime_composition_active(target) {
        Ok(Some(true)) => return Err(InsertionDetail::ImeActive),
        Ok(active) if !policy.permits_ime(active) => return Err(InsertionDetail::ImeUnknown),
        Err(_) if moment != GuardMoment::ExplainQueued || policy != SafetyPolicy::Additive => {
            return Err(InsertionDetail::ImeUnknown)
        }
        _ => {}
    }
    if moment == GuardMoment::BeforePaste && policy == SafetyPolicy::FinalAdditive {
        match backend.selection_is_empty(target) {
            Ok(true) => {}
            Ok(false) => return Err(InsertionDetail::SelectionActive),
            Err(_) => return Err(InsertionDetail::SelectionUnknown),
        }
    }
    if let Some(expected) = expected {
        match backend.target_text(target) {
            Ok(actual) if actual.same_content(expected) => {}
            Ok(_) => return Err(InsertionDetail::TargetChanged),
            Err(_) => return Err(InsertionDetail::TargetUnreadable),
        }
    }
    Ok(())
}

fn safe<B: Backend>(
    backend: &B,
    target: &TargetWindow,
    activity: Option<(&InputMonitor, u64)>,
    policy: SafetyPolicy,
) -> bool {
    evaluate_guard(
        backend,
        target,
        activity,
        policy,
        GuardMoment::BeforePaste,
        None,
    )
    .is_ok()
}

/// Waiting longer could not change the paste decision: the captured target is
/// foreground and focused, and no IME composition is known to be open. An IME
/// state or selection that cannot be read stays unreadable however long we
/// wait, so the paste guard decides those immediately. Input is checked
/// separately. This app's own window is never pasted into, so nothing is
/// worth waiting for.
pub(super) fn ready<B: Backend>(backend: &B, target: &TargetWindow) -> bool {
    target.is_this_app()
        || backend.validate_target(target).is_ok()
            && !backend
                .ime_composition_active(target)
                .is_ok_and(|active| active == Some(true))
}

/// How a final additive paste is guarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FinalGuard {
    /// Ask's monitored insert: never pastes unverified into unreadable targets.
    Monitored,
    /// No input since recording started, so the recording-start state still
    /// holds: an existing selection is replaced and an unknown IME state is
    /// accepted, as for any additive paste.
    Untouched,
    /// Input arrived after recording started. The user may have selected text
    /// or left an IME composition open while waiting, so the selection must be
    /// empty and the IME explicitly inactive.
    AfterInput,
}

impl FinalGuard {
    fn policy(self) -> SafetyPolicy {
        if self == Self::AfterInput {
            SafetyPolicy::FinalAdditive
        } else {
            SafetyPolicy::Additive
        }
    }
}

fn safe_after_injected_paste<B: Backend>(
    backend: &B,
    target: &TargetWindow,
    activity: Option<(&InputMonitor, u64)>,
    policy: SafetyPolicy,
) -> bool {
    evaluate_guard(
        backend,
        target,
        activity,
        policy,
        GuardMoment::VerifyPaste,
        None,
    )
    .is_ok()
}

/// The user's clipboard during one insertion attempt.
///
/// With Restore clipboard on, the clipboard is snapshotted before the first
/// write. Data that cannot be snapshotted is never overwritten: the attempt
/// fails with `ClipboardProtected` before anything is written or pasted, and
/// the caller keeps the text in the app. A clipboard that stays locked fails
/// with `ClipboardUnavailable` as well. After a write, the snapshot is put back
/// once a paste is confirmed, or when the attempt ends before any paste was
/// queued (cancellation or an error). When the text is left on the clipboard
/// on purpose (a copy-only result or a queued but unconfirmed paste) or the
/// restore fails, the user's clipboard is lost and the attempt records
/// `clipboard_not_restored`. A newer copy made meanwhile is the user's own
/// and is never replaced.
///
/// With Restore clipboard off the user accepts that insertion replaces the
/// clipboard: nothing is snapshotted, restored or reported.
struct UserClipboard<'a, B: Backend> {
    backend: &'a B,
    restore: bool,
    previous: Option<B::Clipboard>,
    /// Sequence of our last write while it replaces the user's clipboard.
    written: Option<u32>,
    /// The written text is the outcome and stays on the clipboard.
    kept: bool,
}

impl<'a, B: Backend> UserClipboard<'a, B> {
    fn new(backend: &'a B, options: InjectionOptions) -> Self {
        Self {
            backend,
            restore: options.restore_clipboard,
            previous: None,
            written: None,
            kept: false,
        }
    }

    fn write(&mut self, text: &str) -> Result<u32, InjectionError> {
        if self.restore && self.previous.is_none() {
            self.previous = Some(match self.backend.clipboard_snapshot() {
                Ok(snapshot) => snapshot,
                Err(InjectionError::ClipboardUnpreservable) => {
                    return Err(InjectionError::ClipboardProtected)
                }
                Err(error) => return Err(error),
            });
        }
        match self
            .backend
            .clipboard_write(text, ClipboardExclusion::ExcludeFromHistory)
        {
            Ok(sequence) => {
                self.written = Some(sequence);
                Ok(sequence)
            }
            // The write emptied the clipboard: restore or report it as ours.
            Err(WriteError::AfterEmpty { error, sequence }) => {
                self.written = Some(sequence);
                Err(error)
            }
            // Nothing changed; a newer copy by another process stays.
            Err(WriteError::Untouched(error)) => Err(error),
        }
    }

    /// Leaves `text` on the clipboard as the outcome.
    fn copy(&mut self, text: &str) -> Result<InsertResult, InjectionError> {
        self.write(text)?;
        self.kept = true;
        Ok(InsertResult::ClipboardOnly)
    }

    /// A paste was queued: its payload must stay until the target confirms it.
    fn keep(&mut self) {
        self.kept = true;
    }

    /// Puts the user's clipboard back unless a newer copy replaced ours.
    fn restore(&mut self) {
        if let (Some(previous), Some(sequence)) = (self.previous.take(), self.written) {
            if self.backend.clipboard_restore(previous, sequence).is_ok() {
                self.written = None;
            }
        }
    }
}

impl<B: Backend> Drop for UserClipboard<'_, B> {
    fn drop(&mut self) {
        if !self.restore || self.written.is_none() {
            return;
        }
        if !self.kept {
            // Nothing was queued, so nothing can still read our text.
            self.restore();
        }
        if self.written.is_some() {
            self.backend.clipboard_not_restored();
        }
    }
}

/// Copies `text` as an outcome, under the same clipboard policy.
pub(super) fn copy<B: Backend>(
    backend: &B,
    options: InjectionOptions,
    text: &str,
) -> Result<(), InjectionError> {
    UserClipboard::new(backend, options).copy(text).map(|_| ())
}

enum Selection {
    /// The target shows exactly the requested selection.
    Confirmed(TargetText),
    /// Select() was issued but never confirmed. The target may still apply
    /// it, so the caller may need to collapse it again.
    Unconfirmed(TargetText),
    /// Nothing was selected in the target.
    NotRequested,
}

fn select_verified<B: Backend>(
    backend: &B,
    target: &TargetWindow,
    before: &TargetText,
    text: &str,
    activity: Option<(&InputMonitor, u64)>,
) -> Selection {
    let Some(selected) = before.select_recent(text) else {
        return Selection::NotRequested;
    };
    if !safe(backend, target, activity, SafetyPolicy::Destructive)
        || !backend
            .target_text(target)
            .is_ok_and(|actual| actual.same_content(before))
        || !backend.select_recent(target, before, text).unwrap_or(false)
    {
        return Selection::NotRequested;
    }
    for attempt in 0..=VERIFY_ATTEMPTS {
        if attempt > 0 {
            backend.wait_for_target();
        }
        if !safe(backend, target, activity, SafetyPolicy::Destructive) {
            return Selection::Unconfirmed(selected);
        }
        let Ok(actual) = backend.target_text(target) else {
            return Selection::Unconfirmed(selected);
        };
        if actual.same_content(&selected) {
            return Selection::Confirmed(selected);
        }
        // Only the unchanged caret state may precede the requested selection.
        // An unrelated edit or selection must never authorize replacement.
        if !actual.same_content(before) {
            return Selection::Unconfirmed(selected);
        }
    }
    Selection::Unconfirmed(selected)
}

/// Returns a selection this module requested to a caret at its end, where
/// the caret was before Select(), when nothing else has changed the field.
/// A selected draft left behind would be replaced whole by the user's next
/// keystroke. Collapsing edits no text, so recent input does not block it.
/// The empty caret a retracted draft left was never selected, so it is left
/// alone.
fn collapse_selected<B: Backend>(backend: &B, target: &TargetWindow, selected: &TargetText) {
    if !selected.selected.is_empty()
        && backend
            .target_text(target)
            .is_ok_and(|actual| actual.same_content(selected))
    {
        let _ = backend.collapse_selection(target, selected);
    }
}

/// Some editors (for example VS Code) expose only a proxy or a window of their
/// text, so an exact before/after match never succeeds there. For an additive
/// paste, the inserted text ending right at the caret, as one more occurrence
/// than before, shows the target processed the paste. Text appearing elsewhere
/// (terminal output, a shifted window) does not count, so the old clipboard is
/// never restored under a still-pending paste. Replacements keep the exact
/// match their ranges depend on.
struct InsertedTextEvidence {
    text: String,
    prefix: Vec<usize>,
    previous_count: usize,
}

impl InsertedTextEvidence {
    fn new(before: &TargetText, text: &str) -> Option<Self> {
        let text = normalize_text(text).trim().to_owned();
        if text.is_empty() {
            return None;
        }
        let bytes = text.as_bytes();
        let mut prefix = vec![0; bytes.len()];
        for index in 1..bytes.len() {
            let mut matched = prefix[index - 1];
            while matched > 0 && bytes[index] != bytes[matched] {
                matched = prefix[matched - 1];
            }
            if bytes[index] == bytes[matched] {
                matched += 1;
            }
            prefix[index] = matched;
        }
        let mut evidence = Self {
            text,
            prefix,
            previous_count: 0,
        };
        evidence.previous_count = evidence.count(before);
        Some(evidence)
    }

    /// Match the three adjacent ranges as one byte stream, without copying
    /// the document. Reset after a match to preserve str::matches' disjoint
    /// counting, including occurrences across selection/caret boundaries.
    fn count(&self, state: &TargetText) -> usize {
        let needle = self.text.as_bytes();
        let mut matched = 0;
        let mut count = 0;
        for byte in state
            .before
            .bytes()
            .chain(state.selected.bytes())
            .chain(state.after.bytes())
        {
            while matched > 0 && byte != needle[matched] {
                matched = self.prefix[matched - 1];
            }
            if byte == needle[matched] {
                matched += 1;
            }
            if matched == needle.len() {
                count += 1;
                matched = 0;
            }
        }
        count
    }

    fn confirms(&self, actual: &TargetText) -> bool {
        actual.before.trim_end().ends_with(&self.text) && self.count(actual) > self.previous_count
    }
}

/// A bounded window is a substring of the full readable state. More window
/// occurrences than the *full* baseline therefore also satisfies the original
/// full-state evidence rule. An ambiguous window requests the full reader;
/// it never substitutes a local exact match for an exact document comparison.
fn probe_confirmation(
    probe: &TextProbe,
    before: &TargetText,
    expected: &TargetText,
    inserted: &str,
    evidence: &std::cell::OnceCell<Option<InsertedTextEvidence>>,
) -> Option<bool> {
    if probe.complete && probe.state.same_content(expected) {
        return Some(true);
    }
    if !probe.state.selected.is_empty() {
        return None;
    }
    if probe.state.before.trim_end().len() < inserted.len() {
        return probe.complete.then_some(false);
    }
    if !probe.state.before.trim_end().ends_with(inserted) {
        return Some(false);
    }
    if evidence
        .get_or_init(|| InsertedTextEvidence::new(before, inserted))
        .as_ref()
        .is_some_and(|evidence| evidence.confirms(&probe.state))
    {
        return Some(true);
    }
    probe.complete.then_some(false)
}

/// Once any paste input is queued, its clipboard payload must remain intact
/// until the target confirms the edit. Never substitute a retry or fallback.
#[allow(clippy::too_many_arguments)]
fn paste<B: Backend>(
    backend: &B,
    clipboard: &mut UserClipboard<'_, B>,
    text: &str,
    target: &TargetWindow,
    before: &TargetText,
    activity: Option<(&InputMonitor, u64)>,
    policy: SafetyPolicy,
    cancel: Option<&watch::Receiver<bool>>,
) -> Result<InsertResult, InjectionError> {
    if cancel.is_some_and(|receiver| *receiver.borrow()) {
        return Err(InjectionError::Cancelled);
    }
    if evaluate_guard(
        backend,
        target,
        activity,
        policy,
        GuardMoment::BeforePaste,
        Some(before),
    )
    .is_err()
    {
        if cancel.is_some_and(|receiver| *receiver.borrow()) {
            return Err(InjectionError::Cancelled);
        }
        return clipboard.copy(text);
    }
    // Snapshots first; an unpreservable or locked clipboard stops here.
    clipboard.write(text)?;
    if cancel.is_some_and(|receiver| *receiver.borrow()) {
        return Err(InjectionError::Cancelled);
    }
    if evaluate_guard(
        backend,
        target,
        activity,
        policy,
        GuardMoment::BeforePaste,
        Some(before),
    )
    .is_err()
    {
        if cancel.is_some_and(|receiver| *receiver.borrow()) {
            return Err(InjectionError::Cancelled);
        }
        return clipboard.copy(text);
    }
    if !backend.paste(target, policy).unwrap_or(false) {
        return clipboard.copy(text);
    }
    clipboard.keep();
    let expected = before.replaced_with(text);
    // Exact matches need no scan. Proxy confirmation builds its matcher and
    // scans the unchanged baseline lazily once, rather than on every poll.
    let evidence = std::cell::OnceCell::new();
    let probe_needle = std::cell::OnceCell::new();
    let mut probe_enabled = policy != SafetyPolicy::Destructive
        && before.selected.is_empty()
        && text
            .encode_utf16()
            .take(TEXT_PROBE_UNITS as usize + 1)
            .count()
            <= TEXT_PROBE_UNITS as usize
        && !text.trim().is_empty();
    for attempt in 0..=VERIFY_ATTEMPTS {
        if attempt > 0 {
            backend.wait_for_target();
        }
        if cancel.is_some_and(|receiver| *receiver.borrow())
            || !safe_after_injected_paste(backend, target, activity, policy)
        {
            break;
        }
        let confirmed = if probe_enabled {
            match backend.target_text_probe(target) {
                Ok(Some(probe)) if probe.complete && probe.state.same_content(&expected) => {
                    Some(true)
                }
                Ok(Some(probe)) => {
                    let inserted =
                        probe_needle.get_or_init(|| normalize_text(text).trim().to_owned());
                    let decision =
                        probe_confirmation(&probe, before, &expected, inserted, &evidence);
                    if decision.is_none() {
                        probe_enabled = false;
                    }
                    decision
                }
                // Unsupported, noncaret, truncated, timed-out or ambiguous
                // probes cost at most one attempt before using the old reader.
                _ => {
                    probe_enabled = false;
                    None
                }
            }
        } else {
            None
        };
        if confirmed.unwrap_or_else(|| {
            backend.target_text(target).is_ok_and(|actual| {
                actual.same_content(&expected)
                    || (policy != SafetyPolicy::Destructive
                        && evidence
                            .get_or_init(|| InsertedTextEvidence::new(before, text))
                            .as_ref()
                            .is_some_and(|evidence| evidence.confirms(&actual)))
            })
        }) {
            // Restoration failure cannot undo a confirmed edit and must never
            // initiate a second insertion; it is reported instead.
            clipboard.restore();
            return Ok(InsertResult::ClipboardPaste);
        }
    }
    Ok(InsertResult::PasteUnverified)
}

fn paste_unverified<B: Backend>(
    backend: &B,
    clipboard: &mut UserClipboard<'_, B>,
    text: &str,
    target: &TargetWindow,
) -> Result<InsertResult, InjectionError> {
    // Either the paste is queued or the text is the copy-only outcome.
    clipboard.copy(text)?;
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
    let mut clipboard = UserClipboard::new(backend, options);
    if !safe(backend, target, None, SafetyPolicy::Additive) {
        return clipboard.copy(text);
    }
    let Ok(before) = backend.target_text(target) else {
        return paste_unverified(backend, &mut clipboard, text, target);
    };
    paste(
        backend,
        &mut clipboard,
        text,
        target,
        &before,
        None,
        SafetyPolicy::Additive,
        None,
    )
}

pub(super) fn insert_monitored<B: Backend>(
    backend: &B,
    options: InjectionOptions,
    text: &str,
    target: &TargetWindow,
    monitor: &InputMonitor,
    checkpoint: u64,
    cancel: &watch::Receiver<bool>,
) -> Result<InsertResult, InjectionError> {
    insert_final(
        backend,
        options,
        text,
        target,
        monitor,
        checkpoint,
        cancel,
        FinalGuard::Monitored,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn insert_final<B: Backend>(
    backend: &B,
    options: InjectionOptions,
    text: &str,
    target: &TargetWindow,
    monitor: &InputMonitor,
    checkpoint: u64,
    cancel: &watch::Receiver<bool>,
    guard: FinalGuard,
) -> Result<InsertResult, InjectionError> {
    // The input monitor also fails its guard once cancelled; recheck before
    // every clipboard write so a cancel is never reported as a fallback copy.
    let cancelled = || *cancel.borrow();
    // Only a deferred paste after input needs the stronger guards. Monitored
    // Ask insertion keeps its original activity checkpoint and policy.
    let policy = guard.policy();
    let allow_unverified = guard != FinalGuard::Monitored;
    let mut clipboard = UserClipboard::new(backend, options);
    if cancelled() {
        return Err(InjectionError::Cancelled);
    }
    if !safe(backend, target, Some((monitor, checkpoint)), policy) {
        if cancelled() {
            return Err(InjectionError::Cancelled);
        }
        return clipboard.copy(text);
    }
    let Ok(before) = backend.target_text(target) else {
        if cancelled() {
            return Err(InjectionError::Cancelled);
        }
        if allow_unverified {
            clipboard.write(text)?;
            if cancelled() {
                return Err(InjectionError::Cancelled);
            }
            // Recheck after clipboard access, before queueing exactly one paste.
            if safe(backend, target, Some((monitor, checkpoint)), policy) {
                // Queued or not, the text is now the outcome on the clipboard.
                clipboard.keep();
                return Ok(if backend.paste(target, policy).unwrap_or(false) {
                    InsertResult::PasteUnverified
                } else {
                    InsertResult::ClipboardOnly
                });
            }
            if cancelled() {
                return Err(InjectionError::Cancelled);
            }
        }
        return clipboard.copy(text);
    };
    paste(
        backend,
        &mut clipboard,
        text,
        target,
        &before,
        Some((monitor, checkpoint)),
        policy,
        Some(cancel),
    )
}

/// Explains a non-confirmed outcome after the fact, in guard order. It only
/// reads target state; it never edits the target or the clipboard.
pub(super) fn diagnose<B: Backend>(
    backend: &B,
    target: &TargetWindow,
    monitor: &InputMonitor,
    checkpoint: Option<u64>,
    after_paste: bool,
    guard: FinalGuard,
) -> InsertionDetail {
    if target.is_this_app() {
        return InsertionDetail::NoExternalTarget;
    }
    let Some(checkpoint) = checkpoint.filter(|_| monitor.checkpoint().is_some()) else {
        return InsertionDetail::InputMonitorUnavailable;
    };
    let moment = if after_paste {
        GuardMoment::ExplainQueued
    } else {
        GuardMoment::BeforePaste
    };
    if let Err(detail) = evaluate_guard(
        backend,
        target,
        Some((monitor, checkpoint)),
        guard.policy(),
        moment,
        None,
    ) {
        return detail;
    }
    if backend.target_text(target).is_err() {
        return InsertionDetail::TargetUnreadable;
    }
    InsertionDetail::NotConfirmed
}

/// The focused field and its selected text. An empty selection in a
/// readable field is the typed `NoSelection`, distinct from a field that
/// cannot be read.
pub(super) fn capture_selection<B: Backend>(
    backend: &B,
) -> Result<(TargetWindow, TargetText), InjectionError> {
    let target = backend.capture_target()?;
    let state = backend.target_text(&target)?;
    if state.selected.is_empty() {
        return Err(InjectionError::NoSelection);
    }
    Ok((target, state))
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
        &mut UserClipboard::new(backend, options),
        text,
        target,
        before,
        Some((monitor, checkpoint)),
        SafetyPolicy::Destructive,
        None,
    )
}

pub(super) fn replace_selection_monitored<B: Backend>(
    backend: &B,
    options: InjectionOptions,
    target: &TargetWindow,
    before: &TargetText,
    text: &str,
    activity: (&InputMonitor, u64),
    cancel: &watch::Receiver<bool>,
) -> Result<InsertResult, InjectionError> {
    paste(
        backend,
        &mut UserClipboard::new(backend, options),
        text,
        target,
        before,
        Some(activity),
        SafetyPolicy::Destructive,
        Some(cancel),
    )
}

/// Legacy additive-draft entry used to verify fallback behavior. Production
/// live sessions always use begin_live with a recording-start checkpoint.
#[cfg(test)]
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
    // UIA text reads can block: do not inspect a field after its guard fails.
    let replacement = match checkpoint.filter(|checkpoint| {
        safe(
            backend,
            target,
            Some((monitor, *checkpoint)),
            SafetyPolicy::Destructive,
        )
    }) {
        Some(checkpoint) => backend
            .target_text(target)
            .ok()
            .map(|text| (checkpoint, text)),
        None => None,
    };
    let (result, replacement) = if let Some((checkpoint, before)) = replacement {
        let result = paste(
            backend,
            &mut UserClipboard::new(backend, options),
            draft,
            target,
            &before,
            Some((monitor, checkpoint)),
            SafetyPolicy::Destructive,
            None,
        )?;
        let range = (result != InsertResult::ClipboardOnly).then(|| ReplacementRange {
            after: before.replaced_with(draft),
            checkpoint,
        });
        (result, range)
    } else if allow_untracked {
        // Only the test-only legacy begin entry enables this branch. The
        // production begin_live path never pastes an untracked hypothesis.
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
    let mut clipboard = UserClipboard::new(backend, options);
    if session.replacement.is_none() {
        if finalizing
            && session.result != InsertResult::PasteUnverified
            && (session.result == InsertResult::ClipboardOnly
                || normalize_text(&session.displayed) != normalize_text(final_text))
        {
            session.result = clipboard.copy(final_text)?;
        }
        return Ok(session.result);
    }
    if !confirm_queued_draft(backend, session, monitor) {
        return Ok(session.result);
    }
    if normalize_text(&session.displayed) == normalize_text(final_text) {
        return Ok(session.result);
    }
    let range = session.replacement.as_ref().expect("range was checked");
    let activity = Some((monitor, range.checkpoint));
    let selected = match revision_range(
        backend,
        &session.target,
        range,
        &session.displayed,
        activity,
    ) {
        Selection::Confirmed(selected) => Some(selected),
        // The target may have applied the selection after verification gave up.
        Selection::Unconfirmed(requested) => {
            collapse_selected(backend, &session.target, &requested);
            None
        }
        Selection::NotRequested => None,
    };
    let Some(selected) = selected else {
        session.replacement = None;
        session.result = if finalizing {
            clipboard.copy(final_text)?
        } else {
            InsertResult::ClipboardOnly
        };
        return Ok(session.result);
    };
    let checkpoint = range.checkpoint;
    // A failure after selecting must not authorize another target edit.
    session.replacement = None;
    let result = paste(
        backend,
        &mut clipboard,
        final_text,
        &session.target,
        &selected,
        activity,
        SafetyPolicy::Destructive,
        None,
    );
    // Without a queued paste, the previous draft would stay selected.
    if !matches!(
        result,
        Ok(InsertResult::ClipboardPaste | InsertResult::PasteUnverified)
    ) {
        collapse_selected(backend, &session.target, &selected);
    }
    session.result = result?;
    if session.result != InsertResult::ClipboardOnly {
        session.displayed = final_text.to_owned();
        session.replacement = Some(ReplacementRange {
            after: selected.replaced_with(final_text),
            checkpoint,
        });
    }
    Ok(session.result)
}

/// A draft whose paste was queued but not yet confirmed may have been
/// processed since. Confirm the complete edit before touching a possibly
/// pending payload; `false` leaves the session untouched.
fn confirm_queued_draft<B: Backend>(
    backend: &B,
    session: &mut ProvisionalInsertion,
    monitor: &InputMonitor,
) -> bool {
    let Some(range) = session.replacement.as_ref() else {
        return false;
    };
    if session.result != InsertResult::PasteUnverified {
        return true;
    }
    if !safe(
        backend,
        &session.target,
        Some((monitor, range.checkpoint)),
        SafetyPolicy::Destructive,
    ) || !backend
        .target_text(&session.target)
        .is_ok_and(|actual| actual.same_content(&range.after))
    {
        return false;
    }
    session.result = InsertResult::ClipboardPaste;
    true
}

/// The target state a revision replaces: the verified selection of the
/// current draft or, after the draft was retracted, the unchanged empty caret
/// where it began. Either way the target must still show exactly the state
/// this session left, so nothing outside the draft is ever replaced. The
/// empty caret needs no Select(), so it is never reported as unconfirmed.
fn revision_range<B: Backend>(
    backend: &B,
    target: &TargetWindow,
    range: &ReplacementRange,
    displayed: &str,
    activity: Option<(&InputMonitor, u64)>,
) -> Selection {
    if !displayed.is_empty() {
        return select_verified(backend, target, &range.after, displayed, activity);
    }
    if safe(backend, target, activity, SafetyPolicy::Destructive)
        && backend
            .target_text(target)
            .is_ok_and(|actual| actual.same_content(&range.after))
    {
        Selection::Confirmed(range.after.clone())
    } else {
        Selection::NotRequested
    }
}

/// Withdraws the whole live draft from the target, for an utterance that
/// ended without text. The draft is selected and deleted under the same
/// guards as a replacement; a later revision is pasted at the caret it left.
/// Any failed guard freezes the session as a failed replacement does: the
/// target is never edited again and finalization only copies the text. As
/// for cancellation, a selection it requested but queued no deletion for is
/// collapsed back to a caret so the next keystroke cannot replace the draft.
pub(super) fn retract<B: Backend>(
    backend: &B,
    session: &mut ProvisionalInsertion,
    monitor: &InputMonitor,
) -> Result<InsertResult, InjectionError> {
    if session.finished || !confirm_queued_draft(backend, session, monitor) {
        return Ok(session.result);
    }
    if session.displayed.is_empty() {
        return Ok(session.result);
    }
    let range = session.replacement.as_ref().expect("range was confirmed");
    let checkpoint = range.checkpoint;
    let activity = Some((monitor, checkpoint));
    let selected = select_verified(
        backend,
        &session.target,
        &range.after,
        &session.displayed,
        activity,
    );
    // A failure after selecting must not authorize another target edit.
    session.replacement = None;
    session.result = InsertResult::ClipboardOnly;
    let selected = match selected {
        Selection::Confirmed(selected) => selected,
        // The target may have applied the selection after verification gave up.
        Selection::Unconfirmed(requested) => {
            collapse_selected(backend, &session.target, &requested);
            return Ok(session.result);
        }
        Selection::NotRequested => return Ok(session.result),
    };
    let queued = safe(
        backend,
        &session.target,
        activity,
        SafetyPolicy::Destructive,
    ) && backend
        .target_text(&session.target)
        .is_ok_and(|actual| actual.same_content(&selected))
        // A partially queued deletion may still remove the draft, so only a
        // deletion that queued nothing leaves the selection to collapse.
        && backend.delete_selection(&session.target).unwrap_or(false);
    if !queued {
        collapse_selected(backend, &session.target, &selected);
        return Ok(session.result);
    }
    let expected = selected.replaced_with("");
    for attempt in 0..=VERIFY_ATTEMPTS {
        if attempt > 0 {
            backend.wait_for_target();
        }
        if !safe(
            backend,
            &session.target,
            activity,
            SafetyPolicy::Destructive,
        ) {
            break;
        }
        if backend
            .target_text(&session.target)
            .is_ok_and(|actual| actual.same_content(&expected))
        {
            session.displayed.clear();
            session.replacement = Some(ReplacementRange {
                after: expected,
                checkpoint,
            });
            session.result = InsertResult::ClipboardPaste;
            break;
        }
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
    let selected = match select_verified(
        backend,
        &session.target,
        &range.after,
        &session.displayed,
        activity,
    ) {
        Selection::Confirmed(selected) => selected,
        Selection::Unconfirmed(requested) => {
            collapse_selected(backend, &session.target, &requested);
            return;
        }
        Selection::NotRequested => return,
    };
    let deleted = safe(
        backend,
        &session.target,
        activity,
        SafetyPolicy::Destructive,
    ) && backend
        .target_text(&session.target)
        .is_ok_and(|actual| actual.same_content(&selected))
        // A partially queued deletion may still remove the draft, so only a
        // deletion that queued nothing leaves the selection to collapse.
        && backend.delete_selection(&session.target).unwrap_or(false);
    if !deleted {
        collapse_selected(backend, &session.target, &selected);
    }
}

#[cfg(test)]
pub(super) mod tests {
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

    fn live_draft(
        backend: &MockBackend,
        monitor: &InputMonitor,
        text: &str,
    ) -> ProvisionalInsertion {
        let session = begin_live(
            backend,
            InjectionOptions::default(),
            text,
            &target(),
            monitor,
            monitor.checkpoint().unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(backend.content(), format!("prefix {text} suffix"));
        session
    }

    pub(crate) fn calls(backend: &MockBackend, name: &str) -> usize {
        backend
            .calls
            .borrow()
            .iter()
            .filter(|c| **c == name)
            .count()
    }

    #[test]
    fn retraction_removes_the_whole_live_draft_and_a_revision_resumes_at_its_caret() {
        let backend = MockBackend::new();
        let monitor = monitor();
        let mut session = live_draft(&backend, &monitor, "途中");
        assert_eq!(
            retract(&backend, &mut session, &monitor).unwrap(),
            InsertResult::ClipboardPaste
        );
        assert_eq!(backend.content(), "prefix  suffix");
        assert_eq!(calls(&backend, "delete"), 1);
        // Nothing is left to retract, and dropping an empty draft deletes nothing.
        retract(&backend, &mut session, &monitor).unwrap();
        assert_eq!(calls(&backend, "delete"), 1);
        // The next utterance is pasted exactly where the draft began.
        assert_eq!(
            update(
                &backend,
                InjectionOptions::default(),
                &mut session,
                "次の文",
                &monitor,
                false
            )
            .unwrap(),
            InsertResult::ClipboardPaste
        );
        assert_eq!(backend.content(), "prefix 次の文 suffix");
        finish(
            &backend,
            InjectionOptions::default(),
            &mut session,
            "次の文。",
            &monitor,
        )
        .unwrap();
        assert_eq!(backend.content(), "prefix 次の文。 suffix");
    }

    #[test]
    fn a_retracted_draft_finishes_empty_and_cancels_without_deleting() {
        let backend = MockBackend::new();
        let monitor = monitor();
        let mut session = live_draft(&backend, &monitor, "draft");
        retract(&backend, &mut session, &monitor).unwrap();
        assert_eq!(
            finish(
                &backend,
                InjectionOptions::default(),
                &mut session,
                "",
                &monitor
            )
            .unwrap(),
            InsertResult::ClipboardPaste
        );
        let mut session = live_draft(&backend, &monitor, "draft");
        retract(&backend, &mut session, &monitor).unwrap();
        cancel(&backend, &mut session, &monitor);
        assert_eq!(backend.content(), "prefix  suffix");
        assert_eq!(calls(&backend, "delete"), 2);
        assert_eq!(calls(&backend, "paste"), 2);
    }

    #[test]
    fn retraction_never_deletes_once_a_guard_fails_and_then_freezes_the_target() {
        for reason in ["input", "ime", "edited", "target"] {
            let backend = MockBackend::new();
            let monitor = monitor();
            let mut session = live_draft(&backend, &monitor, "draft");
            match reason {
                "input" => monitor.test_record_input(),
                "ime" => backend.ime.set(None),
                "edited" => backend.text.borrow_mut().after = " suffix typed".into(),
                "target" => backend.target_valid.set(false),
                _ => unreachable!(),
            }
            let content = backend.content();
            assert_eq!(
                retract(&backend, &mut session, &monitor).unwrap(),
                InsertResult::ClipboardOnly,
                "{reason}"
            );
            assert_eq!(backend.content(), content, "{reason}");
            assert_eq!(calls(&backend, "delete"), 0, "{reason}");
            // Even once the guard holds again, the target is never edited.
            backend.ime.set(Some(false));
            backend.target_valid.set(true);
            update(
                &backend,
                InjectionOptions::default(),
                &mut session,
                "later",
                &monitor,
                false,
            )
            .unwrap();
            assert_eq!(backend.content(), content, "{reason}");
            finish(
                &backend,
                InjectionOptions::default(),
                &mut session,
                "final",
                &monitor,
            )
            .unwrap();
            assert_eq!(backend.content(), content, "{reason}");
            assert_eq!(calls(&backend, "paste"), 1, "{reason}");
            assert_eq!(&*backend.clipboard.borrow(), "final", "{reason}");
        }
    }

    #[test]
    fn a_moved_caret_after_retraction_blocks_the_next_revision() {
        let backend = MockBackend::new();
        let monitor = monitor();
        let mut session = live_draft(&backend, &monitor, "draft");
        retract(&backend, &mut session, &monitor).unwrap();
        // The caret moved (for example by an untracked edit) after the draft
        // was withdrawn, so its position no longer marks the draft's range.
        backend.text.borrow_mut().before = "pre".into();
        let clipboard = backend.clipboard.borrow().clone();
        assert_eq!(
            update(
                &backend,
                InjectionOptions::default(),
                &mut session,
                "next",
                &monitor,
                false
            )
            .unwrap(),
            InsertResult::ClipboardOnly
        );
        assert_eq!(backend.content(), "pre suffix");
        assert_eq!(calls(&backend, "paste"), 1);
        // A partial hypothesis never replaces the user's clipboard.
        assert_eq!(*backend.clipboard.borrow(), clipboard);
    }

    type MockHook = Box<dyn Fn(&MockBackend) + Send>;

    /// Shared with `live_dictation` tests through `SystemTextInjector::with_mock`.
    pub(crate) struct MockBackend {
        text: RefCell<TargetText>,
        clipboard: RefCell<String>,
        calls: RefCell<Vec<&'static str>>,
        sequence: Cell<u32>,
        target_valid: Cell<bool>,
        pub(crate) ime: Cell<Option<bool>>,
        ime_query_fails: Cell<bool>,
        snapshot_error: RefCell<Option<InjectionError>>,
        restore_fails: Cell<bool>,
        /// The write empties the clipboard, then fails.
        write_fails_after_empty: Cell<bool>,
        /// Another app holds the clipboard to place a newer copy, so the
        /// write times out opening it.
        write_blocked_by_copy: Cell<bool>,
        not_restored: Cell<usize>,
        /// SendInput queued only part of the deletion; the target has not
        /// processed it yet.
        delete_partial: Cell<bool>,
        /// False when SendInput queued none of the deletion.
        delete_accepted: Cell<bool>,
        pub(crate) text_readable: Cell<bool>,
        text_reads: Cell<usize>,
        probe_reads: Cell<usize>,
        probe_state: RefCell<Option<TextProbe>>,
        probe_fails: Cell<bool>,
        selection_readable: Cell<bool>,
        paste_accepted: Cell<bool>,
        pending: RefCell<Option<String>>,
        pending_selection: RefCell<Option<TargetText>>,
        selection_settle_after: usize,
        selection_waits: Cell<usize>,
        on_selection_wait: Option<MockHook>,
        exclusions: RefCell<Vec<ClipboardExclusion>>,
        settle_after: usize,
        waits: Cell<usize>,
        change_clipboard_on_paste: bool,
        change_identity_on_paste: bool,
        on_paste: Option<Box<dyn Fn() + Send>>,
        on_validate: Option<Box<dyn Fn() + Send>>,
        on_clipboard_write: Option<MockHook>,
        on_snapshot: Option<MockHook>,
        /// What the target exposes after processing a paste, for editors
        /// whose readable text is a proxy or a window of the document.
        readback_after_paste: RefCell<Option<TargetText>>,
    }

    impl MockBackend {
        pub(crate) fn new() -> Self {
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
                ime_query_fails: Cell::new(false),
                snapshot_error: RefCell::new(None),
                restore_fails: Cell::new(false),
                write_fails_after_empty: Cell::new(false),
                write_blocked_by_copy: Cell::new(false),
                not_restored: Cell::new(0),
                delete_partial: Cell::new(false),
                delete_accepted: Cell::new(true),
                text_readable: Cell::new(true),
                text_reads: Cell::new(0),
                probe_reads: Cell::new(0),
                probe_state: RefCell::new(None),
                probe_fails: Cell::new(false),
                selection_readable: Cell::new(true),
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
                on_paste: None,
                on_validate: None,
                on_clipboard_write: None,
                on_snapshot: None,
                readback_after_paste: RefCell::new(None),
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
        pub(crate) fn content(&self) -> String {
            let text = self.text.borrow();
            format!("{}{}{}", text.before, text.selected, text.after)
        }
        pub(crate) fn set_selected_text(&self, selected: &str) {
            self.text.borrow_mut().selected = selected.into();
        }
        pub(crate) fn set_text_readable(&self, readable: bool) {
            self.text_readable.set(readable);
        }
        /// Consumes the unrestored-clipboard record, as the platform backends do.
        pub(crate) fn take_clipboard_not_restored(&self) -> bool {
            self.not_restored.replace(0) > 0
        }
    }

    impl Backend for MockBackend {
        type Clipboard = String;
        fn capture_target(&self) -> Result<TargetWindow, InjectionError> {
            Ok(target())
        }
        fn validate_target(&self, target: &TargetWindow) -> Result<(), InjectionError> {
            if target.is_secure {
                return Err(InjectionError::SecureTarget);
            }
            if let Some(on_validate) = &self.on_validate {
                on_validate();
            }
            if self.target_valid.get() {
                Ok(())
            } else {
                Err(InjectionError::TargetChanged)
            }
        }
        fn ime_composition_active(&self, _: &TargetWindow) -> Result<Option<bool>, InjectionError> {
            if self.ime_query_fails.get() {
                return Err(InjectionError::BackendFailure(
                    "failed to query IME composition",
                ));
            }
            Ok(self.ime.get())
        }
        fn focus_identity(&self, _: &TargetWindow) -> Result<Vec<i32>, InjectionError> {
            Ok(self.text.borrow().identity.clone())
        }
        fn target_text_probe(&self, _: &TargetWindow) -> Result<Option<TextProbe>, InjectionError> {
            self.probe_reads.set(self.probe_reads.get() + 1);
            if self.probe_fails.get() {
                return Err(InjectionError::BackendFailure("probe unavailable"));
            }
            Ok(self.probe_state.borrow().clone())
        }
        fn target_text(&self, _: &TargetWindow) -> Result<TargetText, InjectionError> {
            self.text_reads.set(self.text_reads.get() + 1);
            if self.text_readable.get() {
                Ok(self.text.borrow().clone())
            } else {
                Err(InjectionError::BackendFailure("unreadable target text"))
            }
        }
        fn selection_is_empty(&self, _: &TargetWindow) -> Result<bool, InjectionError> {
            if self.selection_readable.get() {
                Ok(self.text.borrow().selected.is_empty())
            } else {
                Err(InjectionError::BackendFailure("unreadable selection"))
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
            if let Some(on_snapshot) = &self.on_snapshot {
                on_snapshot(self);
            }
            if let Some(error) = self.snapshot_error.borrow().clone() {
                return Err(error);
            }
            Ok(self.clipboard.borrow().clone())
        }
        fn clipboard_write(
            &self,
            text: &str,
            exclusion: ClipboardExclusion,
        ) -> Result<u32, WriteError> {
            self.calls.borrow_mut().push("write");
            if self.write_blocked_by_copy.get() {
                *self.clipboard.borrow_mut() = "new user copy".into();
                self.sequence.set(self.sequence.get() + 1);
                return Err(WriteError::Untouched(InjectionError::ClipboardUnavailable));
            }
            if self.write_fails_after_empty.get() {
                self.clipboard.borrow_mut().clear();
                self.sequence.set(self.sequence.get() + 1);
                return Err(WriteError::AfterEmpty {
                    error: InjectionError::ClipboardUnavailable,
                    sequence: self.sequence.get(),
                });
            }
            *self.clipboard.borrow_mut() = text.into();
            self.exclusions.borrow_mut().push(exclusion);
            self.sequence.set(self.sequence.get() + 1);
            if let Some(on_write) = &self.on_clipboard_write {
                on_write(self);
            }
            Ok(self.sequence.get())
        }
        fn clipboard_restore(
            &self,
            snapshot: Self::Clipboard,
            expected: u32,
        ) -> Result<bool, InjectionError> {
            self.calls.borrow_mut().push("restore");
            if self.restore_fails.get() {
                return Err(InjectionError::ClipboardUnavailable);
            }
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
                if let Some(readback) = self.readback_after_paste.borrow_mut().take() {
                    *self.text.borrow_mut() = readback;
                }
            }
            if let Some(on_paste) = &self.on_paste {
                on_paste();
            }
            if self.change_clipboard_on_paste {
                *self.clipboard.borrow_mut() = "new user copy".into();
                self.sequence.set(self.sequence.get() + 1);
            }
            Ok(true)
        }
        fn delete_selection(&self, _: &TargetWindow) -> Result<bool, InjectionError> {
            self.calls.borrow_mut().push("delete");
            if !self.delete_accepted.get() {
                return Ok(false);
            }
            if !self.delete_partial.get() {
                self.text.borrow_mut().selected.clear();
            }
            Ok(true)
        }
        fn collapse_selection(
            &self,
            _: &TargetWindow,
            expected: &TargetText,
        ) -> Result<bool, InjectionError> {
            if self.ime.get() != Some(false) || !self.text.borrow().same_content(expected) {
                return Ok(false);
            }
            self.calls.borrow_mut().push("collapse");
            let mut text = self.text.borrow_mut();
            let selected = std::mem::take(&mut text.selected);
            text.before.push_str(&selected);
            Ok(true)
        }
        fn clipboard_not_restored(&self) {
            self.not_restored.set(self.not_restored.get() + 1);
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
    pub(crate) fn target() -> TargetWindow {
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
    fn rejected_provisional_guard_never_reads_target_text() {
        for tracked in [false, true] {
            let backend = MockBackend::new();
            backend.target_valid.set(false);
            let monitor = monitor();
            let checkpoint = tracked.then(|| monitor.checkpoint().unwrap());
            begin_with_checkpoint(
                &backend,
                InjectionOptions::default(),
                "draft",
                &target(),
                &monitor,
                checkpoint,
                false,
            )
            .unwrap();
            assert_eq!(backend.text_reads.get(), 0);
            assert!(!backend.calls.borrow().contains(&"paste"));
        }
    }

    #[test]
    fn final_text_pastes_once_after_an_unreadable_live_target() {
        let backend = MockBackend::new();
        backend.text_readable.set(false);
        let monitor = monitor();
        let checkpoint = monitor.checkpoint().unwrap();
        let session = begin_live(
            &backend,
            InjectionOptions::default(),
            "draft",
            &target(),
            &monitor,
            checkpoint,
        )
        .unwrap()
        .unwrap();
        assert!(!session.paste_was_queued());
        assert!(!backend.calls.borrow().contains(&"paste"));
        let (_sender, cancel) = watch::channel(false);
        assert_eq!(
            insert_final(
                &backend,
                InjectionOptions::default(),
                "final",
                &target(),
                &monitor,
                checkpoint,
                &cancel,
                FinalGuard::AfterInput
            )
            .unwrap(),
            InsertResult::PasteUnverified
        );
        assert_eq!(backend.content(), "prefix final suffix");
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
    fn diagnosis_reports_the_first_failing_guard_without_side_effects() {
        let cases: [(&str, InsertionDetail); 10] = [
            ("monitor", InsertionDetail::InputMonitorUnavailable),
            ("no_checkpoint", InsertionDetail::InputMonitorUnavailable),
            ("input", InsertionDetail::UserActivity),
            ("target", InsertionDetail::TargetChanged),
            ("ime", InsertionDetail::ImeActive),
            ("ime_unknown", InsertionDetail::ImeUnknown),
            ("selection", InsertionDetail::SelectionActive),
            ("selection_unreadable", InsertionDetail::SelectionUnknown),
            ("unreadable", InsertionDetail::TargetUnreadable),
            ("none", InsertionDetail::NotConfirmed),
        ];
        for (case, expected) in cases {
            let backend = MockBackend::new();
            let monitor = monitor();
            let mut checkpoint = monitor.checkpoint();
            match case {
                "monitor" => monitor.test_set_available(false),
                "no_checkpoint" => checkpoint = None,
                "input" => monitor.test_record_input(),
                "target" => backend.target_valid.set(false),
                "ime" => backend.ime.set(Some(true)),
                "ime_unknown" => backend.ime.set(None),
                "selection" => backend.text.borrow_mut().selected = "user text".into(),
                "selection_unreadable" => backend.selection_readable.set(false),
                "unreadable" => backend.text_readable.set(false),
                "none" => {}
                _ => unreachable!(),
            }
            assert_eq!(
                diagnose(
                    &backend,
                    &target(),
                    &monitor,
                    checkpoint,
                    false,
                    FinalGuard::AfterInput
                ),
                expected,
                "{case}"
            );
            assert!(backend.calls.borrow().is_empty(), "{case}");
            assert_eq!(*backend.clipboard.borrow(), "original rich clipboard");
        }
        // Deferred pastes require an explicitly inactive IME.
        let backend = MockBackend::new();
        backend.ime.set(None);
        let monitor = monitor();
        assert_eq!(
            diagnose(
                &backend,
                &target(),
                &monitor,
                monitor.checkpoint(),
                false,
                FinalGuard::AfterInput
            ),
            InsertionDetail::ImeUnknown
        );
        // After our own paste, a still-visible injected modifier is not the
        // user's activity; before a paste it blocks insertion and is reported.
        backend.ime.set(Some(false));
        monitor.test_set_async_modifier_pending(true);
        assert_eq!(
            diagnose(
                &backend,
                &target(),
                &monitor,
                monitor.checkpoint(),
                true,
                FinalGuard::AfterInput
            ),
            InsertionDetail::NotConfirmed
        );
        assert_eq!(
            diagnose(
                &backend,
                &target(),
                &monitor,
                monitor.checkpoint(),
                false,
                FinalGuard::AfterInput
            ),
            InsertionDetail::UserActivity
        );
    }

    fn final_paste(backend: &MockBackend, guard: FinalGuard) -> InsertResult {
        let monitor = monitor();
        let checkpoint = monitor.checkpoint().unwrap();
        let (_sender, cancel) = watch::channel(false);
        insert_final(
            backend,
            InjectionOptions::default(),
            "new",
            &target(),
            &monitor,
            checkpoint,
            &cancel,
            guard,
        )
        .unwrap()
    }

    fn pastes(backend: &MockBackend) -> usize {
        backend
            .calls
            .borrow()
            .iter()
            .filter(|call| **call == "paste")
            .count()
    }

    #[test]
    fn a_failed_ime_query_is_reported_as_unknown_not_unreadable() {
        let backend = MockBackend::new();
        backend.ime_query_fails.set(true);
        backend.text_readable.set(false);
        // Every paste guard requires an IME answer, so nothing is sent.
        assert_eq!(
            final_paste(&backend, FinalGuard::Untouched),
            InsertResult::ClipboardOnly
        );
        assert_eq!(pastes(&backend), 0);
        let monitor = monitor();
        for guard in [FinalGuard::Untouched, FinalGuard::AfterInput] {
            assert_eq!(
                diagnose(
                    &backend,
                    &target(),
                    &monitor,
                    monitor.checkpoint(),
                    false,
                    guard
                ),
                InsertionDetail::ImeUnknown
            );
        }
        // After a sent paste, the unreadable field explains the outcome.
        assert_eq!(
            diagnose(
                &backend,
                &target(),
                &monitor,
                monitor.checkpoint(),
                true,
                FinalGuard::Untouched
            ),
            InsertionDetail::TargetUnreadable
        );
    }

    #[test]
    fn a_recording_started_in_this_app_is_copied_without_pasting() {
        let monitor = monitor();
        let checkpoint = monitor.checkpoint().unwrap();
        let (_sender, cancel) = watch::channel(false);
        let app = TargetWindow::this_app();
        assert!(app.is_this_app());
        assert!(!target().is_this_app());
        for guard in [FinalGuard::Untouched, FinalGuard::AfterInput] {
            let backend = MockBackend::new();
            assert!(ready(&backend, &app), "nothing to wait for");
            assert_eq!(
                insert_final(
                    &backend,
                    InjectionOptions::default(),
                    "result",
                    &app,
                    &monitor,
                    checkpoint,
                    &cancel,
                    guard
                )
                .unwrap(),
                InsertResult::ClipboardOnly
            );
            assert_eq!(*backend.calls.borrow(), ["write"]);
            assert_eq!(*backend.clipboard.borrow(), "result");
            assert_eq!(backend.content(), "prefix  suffix");
            assert_eq!(
                diagnose(&backend, &app, &monitor, Some(checkpoint), false, guard),
                InsertionDetail::NoExternalTarget
            );
        }
        // Opt-in live text never reaches this app's window either.
        let backend = MockBackend::new();
        let mut session = begin(
            &backend,
            InjectionOptions::default(),
            "draft",
            &app,
            &monitor,
        )
        .unwrap()
        .unwrap();
        assert!(!session.paste_was_queued());
        finish(
            &backend,
            InjectionOptions::default(),
            &mut session,
            "final",
            &monitor,
        )
        .unwrap();
        assert_eq!(pastes(&backend), 0);
        assert_eq!(*backend.clipboard.borrow(), "final");
    }

    #[test]
    fn only_a_recording_started_from_this_apps_ui_loses_its_target() {
        let mut own = target();
        own.process_id = std::process::id();
        assert!(recording_target(own.clone(), false).is_this_app());
        // A hotkey keeps the captured foreground window, even this app's.
        assert_eq!(recording_target(own.clone(), true), own);
        // Starting from the tray while another app is foreground keeps it.
        assert_eq!(recording_target(target(), false), target());
    }

    fn restore_on(
        backend: &MockBackend,
        guard: FinalGuard,
    ) -> Result<InsertResult, InjectionError> {
        let monitor = monitor();
        let (_sender, cancel) = watch::channel(false);
        insert_final(
            backend,
            InjectionOptions::default(),
            "new",
            &target(),
            &monitor,
            monitor.checkpoint().unwrap(),
            &cancel,
            guard,
        )
    }

    #[test]
    fn restore_on_never_overwrites_an_unpreservable_clipboard() {
        for case in ["readable", "unreadable", "copy_only"] {
            let backend = MockBackend::new();
            *backend.snapshot_error.borrow_mut() = Some(InjectionError::ClipboardUnpreservable);
            match case {
                "unreadable" => backend.text_readable.set(false),
                "copy_only" => backend.target_valid.set(false),
                _ => {}
            }
            assert_eq!(
                restore_on(&backend, FinalGuard::Untouched),
                Err(InjectionError::ClipboardProtected),
                "{case}"
            );
            assert!(
                backend.calls.borrow().is_empty(),
                "{case}: no write or paste"
            );
            assert_eq!(
                *backend.clipboard.borrow(),
                "original rich clipboard",
                "{case}"
            );
            assert_eq!(backend.content(), "prefix  suffix", "{case}");
            assert_eq!(backend.not_restored.get(), 0, "{case}");
        }
        // A copy requested by the caller follows the same rule.
        let backend = MockBackend::new();
        *backend.snapshot_error.borrow_mut() = Some(InjectionError::ClipboardUnpreservable);
        assert_eq!(
            copy(&backend, InjectionOptions::default(), "result"),
            Err(InjectionError::ClipboardProtected)
        );
        assert_eq!(*backend.clipboard.borrow(), "original rich clipboard");
        assert_eq!(
            InsertionDetail::from_error(&InjectionError::ClipboardProtected),
            Some(InsertionDetail::ClipboardProtected)
        );
    }

    #[test]
    fn restore_off_overwrites_without_a_snapshot_or_report() {
        let backend = MockBackend::new();
        *backend.snapshot_error.borrow_mut() = Some(InjectionError::ClipboardUnpreservable);
        let monitor = monitor();
        let (_sender, cancel) = watch::channel(false);
        let off = InjectionOptions {
            restore_clipboard: false,
        };
        assert_eq!(
            insert_final(
                &backend,
                off,
                "new",
                &target(),
                &monitor,
                monitor.checkpoint().unwrap(),
                &cancel,
                FinalGuard::Untouched,
            ),
            Ok(InsertResult::ClipboardPaste)
        );
        assert_eq!(*backend.clipboard.borrow(), "new");
        backend.target_valid.set(false);
        copy(&backend, off, "copied").unwrap();
        assert_eq!(*backend.clipboard.borrow(), "copied");
        assert_eq!(backend.not_restored.get(), 0);
    }

    #[test]
    fn a_clipboard_left_with_our_text_is_reported_unless_restore_is_off() {
        // A queued paste that is never confirmed keeps its payload.
        let mut backend = MockBackend::new();
        backend.settle_after = VERIFY_ATTEMPTS + 5;
        assert_eq!(
            restore_on(&backend, FinalGuard::Untouched),
            Ok(InsertResult::PasteUnverified)
        );
        assert!(!backend.calls.borrow().contains(&"restore"));
        assert_eq!(backend.not_restored.get(), 1);
        // An unreadable field gets one unverified paste, never restored.
        let backend = MockBackend::new();
        backend.text_readable.set(false);
        assert_eq!(
            restore_on(&backend, FinalGuard::Untouched),
            Ok(InsertResult::PasteUnverified)
        );
        assert_eq!(backend.not_restored.get(), 1);
        // A copy-only outcome replaces the clipboard on purpose.
        let backend = MockBackend::new();
        backend.target_valid.set(false);
        assert_eq!(
            restore_on(&backend, FinalGuard::Untouched),
            Ok(InsertResult::ClipboardOnly)
        );
        assert_eq!(*backend.clipboard.borrow(), "new");
        assert_eq!(backend.not_restored.get(), 1);
        // A confirmed paste restores and reports nothing.
        let backend = MockBackend::new();
        assert_eq!(
            restore_on(&backend, FinalGuard::Untouched),
            Ok(InsertResult::ClipboardPaste)
        );
        assert_eq!(*backend.clipboard.borrow(), "original rich clipboard");
        assert_eq!(backend.not_restored.get(), 0);
    }

    #[test]
    fn cancellation_after_the_write_restores_the_clipboard() {
        for restore_fails in [false, true] {
            let backend = MockBackend::new();
            backend.restore_fails.set(restore_fails);
            backend.text_readable.set(false);
            let monitor = monitor();
            let (cancel, cancelled) = watch::channel(false);
            let mut backend = backend;
            backend.on_clipboard_write = Some(Box::new(move |_| {
                cancel.send_replace(true);
            }));
            assert_eq!(
                insert_final(
                    &backend,
                    InjectionOptions::default(),
                    "new",
                    &target(),
                    &monitor,
                    monitor.checkpoint().unwrap(),
                    &cancelled,
                    FinalGuard::Untouched,
                ),
                Err(InjectionError::Cancelled)
            );
            assert_eq!(pastes(&backend), 0);
            assert!(backend.calls.borrow().ends_with(&["write", "restore"]));
            if restore_fails {
                // The user's clipboard is lost: reported.
                assert_eq!(backend.not_restored.get(), 1);
            } else {
                assert_eq!(*backend.clipboard.borrow(), "original rich clipboard");
                assert_eq!(backend.not_restored.get(), 0);
            }
        }
    }

    #[test]
    fn a_write_that_empties_the_clipboard_and_fails_is_restored_or_reported() {
        for restore_fails in [false, true] {
            let backend = MockBackend::new();
            backend.write_fails_after_empty.set(true);
            backend.restore_fails.set(restore_fails);
            assert_eq!(
                restore_on(&backend, FinalGuard::Untouched),
                Err(InjectionError::ClipboardUnavailable)
            );
            assert_eq!(pastes(&backend), 0);
            assert!(backend.calls.borrow().ends_with(&["write", "restore"]));
            if restore_fails {
                assert_eq!(*backend.clipboard.borrow(), "");
                assert_eq!(backend.not_restored.get(), 1);
            } else {
                assert_eq!(*backend.clipboard.borrow(), "original rich clipboard");
                assert_eq!(backend.not_restored.get(), 0);
            }
        }
        // With Restore clipboard off the user accepted replacement.
        let backend = MockBackend::new();
        backend.write_fails_after_empty.set(true);
        let off = InjectionOptions {
            restore_clipboard: false,
        };
        assert!(copy(&backend, off, "text").is_err());
        assert!(!backend.calls.borrow().contains(&"restore"));
        assert_eq!(backend.not_restored.get(), 0);
    }

    #[test]
    fn a_write_that_never_opened_the_clipboard_leaves_a_newer_copy_alone() {
        let backend = MockBackend::new();
        backend.write_blocked_by_copy.set(true);
        assert_eq!(
            restore_on(&backend, FinalGuard::Untouched),
            Err(InjectionError::ClipboardUnavailable)
        );
        assert_eq!(pastes(&backend), 0);
        assert!(!backend.calls.borrow().contains(&"restore"));
        assert_eq!(*backend.clipboard.borrow(), "new user copy");
        assert_eq!(backend.not_restored.get(), 0);
    }

    #[test]
    fn a_failed_restore_after_a_confirmed_paste_is_reported() {
        let backend = MockBackend::new();
        backend.restore_fails.set(true);
        assert_eq!(
            restore_on(&backend, FinalGuard::Untouched),
            Ok(InsertResult::ClipboardPaste)
        );
        assert!(backend.calls.borrow().contains(&"restore"));
        assert_eq!(*backend.clipboard.borrow(), "new");
        assert_eq!(backend.not_restored.get(), 1);
        // A newer copy made meanwhile wins and is the user's own.
        let mut backend = MockBackend::new();
        backend.change_clipboard_on_paste = true;
        assert_eq!(
            restore_on(&backend, FinalGuard::Untouched),
            Ok(InsertResult::ClipboardPaste)
        );
        assert_eq!(*backend.clipboard.borrow(), "new user copy");
        assert_eq!(backend.not_restored.get(), 0);
    }

    #[test]
    fn an_empty_selection_is_a_typed_outcome_unlike_an_unreadable_field() {
        let backend = MockBackend::new();
        assert_eq!(
            capture_selection(&backend).err(),
            Some(InjectionError::NoSelection)
        );
        backend.text_readable.set(false);
        assert!(matches!(
            capture_selection(&backend).err(),
            Some(InjectionError::BackendFailure(_))
        ));
        backend.text_readable.set(true);
        *backend.text.borrow_mut() = selected_state();
        let (target, state) = capture_selection(&backend).unwrap();
        assert_eq!(target, self::target());
        assert_eq!(state.selected, "original");
    }

    #[test]
    fn an_aborted_live_update_does_not_leave_the_draft_selected() {
        let monitor = std::sync::Arc::new(monitor());
        let mut backend = MockBackend::new();
        let mut session = begin(
            &backend,
            InjectionOptions::default(),
            "draft",
            &target(),
            &monitor,
        )
        .unwrap()
        .unwrap();
        // A click after the draft was selected, before its replacement.
        let interrupting = std::sync::Arc::clone(&monitor);
        backend.on_snapshot = Some(Box::new(move |_| interrupting.test_record_input()));
        assert_eq!(
            update(
                &backend,
                InjectionOptions::default(),
                &mut session,
                "revised draft",
                &monitor,
                false,
            )
            .unwrap(),
            InsertResult::ClipboardOnly
        );
        assert!(backend
            .calls
            .borrow()
            .ends_with(&["select", "write", "write", "collapse"]));
        let text = backend.text.borrow().clone();
        assert_eq!(text.before, "prefix draft");
        assert!(text.selected.is_empty());
        assert_eq!(text.after, " suffix");
    }

    #[test]
    fn a_selection_the_user_already_changed_is_left_alone() {
        let monitor = monitor();
        let mut backend = MockBackend::new();
        let mut session = begin(
            &backend,
            InjectionOptions::default(),
            "draft",
            &target(),
            &monitor,
        )
        .unwrap()
        .unwrap();
        // The user typed over the selected draft before the replacement.
        backend.on_snapshot = Some(Box::new(|backend| {
            let mut text = backend.text.borrow_mut();
            text.selected.clear();
            text.before.push('x');
        }));
        update(
            &backend,
            InjectionOptions::default(),
            &mut session,
            "revised draft",
            &monitor,
            false,
        )
        .unwrap();
        assert!(!backend.calls.borrow().contains(&"collapse"));
        assert_eq!(backend.content(), "prefix x suffix");
    }

    #[test]
    fn cancellation_that_cannot_delete_restores_the_caret() {
        for case in ["input", "ime"] {
            let monitor = std::sync::Arc::new(monitor());
            let mut backend = MockBackend::new();
            let mut session = begin(
                &backend,
                InjectionOptions::default(),
                "draft",
                &target(),
                &monitor,
            )
            .unwrap()
            .unwrap();
            // The selection settles after a wait, during which the user acts.
            backend.selection_settle_after = 1;
            let interrupting = std::sync::Arc::clone(&monitor);
            backend.on_selection_wait = Some(Box::new(move |backend| match case {
                "input" => interrupting.test_record_input(),
                _ => backend.ime.set(Some(true)),
            }));
            cancel(&backend, &mut session, &monitor);
            assert!(!backend.calls.borrow().contains(&"delete"), "{case}");
            assert_eq!(backend.content(), "prefix draft suffix", "{case}");
            // Moving the caret could disturb an open IME conversion.
            assert_eq!(
                backend.calls.borrow().contains(&"collapse"),
                case == "input",
                "{case}"
            );
            assert_eq!(
                backend.text.borrow().selected.is_empty(),
                case == "input",
                "{case}"
            );
        }
    }

    #[test]
    fn a_clipboard_that_stays_locked_aborts_before_any_target_input() {
        let backend = MockBackend::new();
        *backend.snapshot_error.borrow_mut() = Some(InjectionError::ClipboardUnavailable);
        let monitor = monitor();
        let (_sender, cancel) = watch::channel(false);
        assert_eq!(
            insert_final(
                &backend,
                InjectionOptions::default(),
                "new",
                &target(),
                &monitor,
                monitor.checkpoint().unwrap(),
                &cancel,
                FinalGuard::Untouched,
            ),
            Err(InjectionError::ClipboardUnavailable)
        );
        assert!(backend.calls.borrow().is_empty());
        assert_eq!(*backend.clipboard.borrow(), "original rich clipboard");
        assert_eq!(backend.content(), "prefix  suffix");
        assert_eq!(backend.not_restored.get(), 0);
    }

    #[test]
    fn collapse_is_limited_to_a_selection_this_attempt_requested() {
        for case in ["input_before_select", "ime_unknown"] {
            let watcher = monitor();
            let mut backend = MockBackend::new();
            let mut session = begin(
                &backend,
                InjectionOptions::default(),
                "draft",
                &target(),
                &watcher,
            )
            .unwrap()
            .unwrap();
            if case == "input_before_select" {
                // Nothing is selected for us; the user selected exactly the
                // draft themselves, and that selection is theirs.
                watcher.test_record_input();
                backend.text.borrow_mut().before = "prefix ".into();
                backend.text.borrow_mut().selected = "draft".into();
            } else {
                // The IME state becomes unknown after Select().
                backend.on_snapshot = Some(Box::new(|backend| backend.ime.set(None)));
            }
            update(
                &backend,
                InjectionOptions::default(),
                &mut session,
                "revised draft",
                &watcher,
                false,
            )
            .unwrap();
            assert_eq!(
                backend.calls.borrow().contains(&"select"),
                case == "ime_unknown",
                "{case}"
            );
            assert!(!backend.calls.borrow().contains(&"collapse"), "{case}");
            assert_eq!(backend.text.borrow().selected, "draft", "{case}");
        }
    }

    #[test]
    fn a_partially_queued_deletion_is_not_followed_by_a_collapse() {
        let watcher = monitor();
        let backend = MockBackend::new();
        let mut session = begin(
            &backend,
            InjectionOptions::default(),
            "draft",
            &target(),
            &watcher,
        )
        .unwrap()
        .unwrap();
        backend.delete_partial.set(true);
        cancel(&backend, &mut session, &watcher);
        assert!(backend.calls.borrow().ends_with(&["select", "delete"]));
        assert!(!backend.calls.borrow().contains(&"collapse"));
    }

    #[test]
    fn an_aborted_revision_after_retraction_does_not_touch_the_unselected_caret() {
        let monitor = std::sync::Arc::new(monitor());
        let mut backend = MockBackend::new();
        let mut session = live_draft(&backend, &monitor, "draft");
        retract(&backend, &mut session, &monitor).unwrap();
        // Input arrives after the caret was confirmed, before the paste.
        let interrupting = std::sync::Arc::clone(&monitor);
        backend.on_snapshot = Some(Box::new(move |_| interrupting.test_record_input()));
        assert_eq!(
            update(
                &backend,
                InjectionOptions::default(),
                &mut session,
                "next",
                &monitor,
                false,
            )
            .unwrap(),
            InsertResult::ClipboardOnly
        );
        assert_eq!(backend.content(), "prefix  suffix");
        assert_eq!(calls(&backend, "collapse"), 0);
    }

    #[test]
    fn retraction_collapses_only_a_selection_it_requested_and_could_not_delete() {
        for case in ["input", "ime", "nothing_queued", "partial"] {
            let monitor = std::sync::Arc::new(monitor());
            let mut backend = MockBackend::new();
            let mut session = live_draft(&backend, &monitor, "draft");
            match case {
                // The selection settles after a wait, during which the user acts.
                "input" | "ime" => {
                    backend.selection_settle_after = 1;
                    let interrupting = std::sync::Arc::clone(&monitor);
                    backend.on_selection_wait = Some(Box::new(move |backend| match case {
                        "input" => interrupting.test_record_input(),
                        _ => backend.ime.set(Some(true)),
                    }));
                }
                "nothing_queued" => backend.delete_accepted.set(false),
                "partial" => backend.delete_partial.set(true),
                _ => unreachable!(),
            }
            assert_eq!(
                retract(&backend, &mut session, &monitor).unwrap(),
                InsertResult::ClipboardOnly,
                "{case}"
            );
            assert_eq!(backend.content(), "prefix draft suffix", "{case}");
            assert_eq!(
                calls(&backend, "delete"),
                usize::from(matches!(case, "nothing_queued" | "partial")),
                "{case}"
            );
            // Moving the caret could disturb an open IME conversion, and a
            // partially queued deletion may still remove the draft.
            let collapsed = matches!(case, "input" | "nothing_queued");
            assert_eq!(
                calls(&backend, "collapse"),
                usize::from(collapsed),
                "{case}"
            );
            assert_eq!(
                backend.text.borrow().selected.is_empty(),
                collapsed,
                "{case}"
            );
            // The session stays frozen: a later revision never edits the target.
            update(
                &backend,
                InjectionOptions::default(),
                &mut session,
                "later",
                &monitor,
                false,
            )
            .unwrap();
            assert_eq!(backend.content(), "prefix draft suffix", "{case}");
        }
    }

    #[test]
    fn untouched_final_paste_keeps_recording_start_semantics() {
        // No input since recording started: a selection made before recording
        // is replaced, and an unknown IME state is accepted.
        let backend = MockBackend::new();
        backend.text.borrow_mut().selected = "old".into();
        backend.ime.set(None);
        assert_eq!(
            final_paste(&backend, FinalGuard::Untouched),
            InsertResult::ClipboardPaste
        );
        assert_eq!(backend.content(), "prefix new suffix");
        // An unreadable target still receives exactly one unverified paste.
        let backend = MockBackend::new();
        backend.ime.set(None);
        backend.text_readable.set(false);
        assert_eq!(
            final_paste(&backend, FinalGuard::Untouched),
            InsertResult::PasteUnverified
        );
        assert_eq!(pastes(&backend), 1);
    }

    #[test]
    fn a_final_paste_after_input_refuses_selections_and_unknown_ime_states() {
        for case in ["selection", "ime_unknown", "selection_unreadable"] {
            let backend = MockBackend::new();
            match case {
                "selection" => backend.text.borrow_mut().selected = "user text".into(),
                "ime_unknown" => backend.ime.set(None),
                "selection_unreadable" => backend.selection_readable.set(false),
                _ => unreachable!(),
            }
            assert_eq!(
                final_paste(&backend, FinalGuard::AfterInput),
                InsertResult::ClipboardOnly,
                "{case}"
            );
            assert_eq!(pastes(&backend), 0, "{case}");
            assert_eq!(*backend.clipboard.borrow(), "new", "{case}");
        }
    }

    #[test]
    fn readiness_waits_only_for_conditions_that_waiting_can_change() {
        let backend = MockBackend::new();
        assert!(ready(&backend, &target()));
        // Unreadable IME states and selections stay unreadable: decide now.
        backend.ime.set(None);
        assert!(ready(&backend, &target()));
        backend.text.borrow_mut().selected = "user text".into();
        assert!(ready(&backend, &target()));
        // An open composition or another focused window can still change.
        backend.ime.set(Some(true));
        assert!(!ready(&backend, &target()));
        backend.ime.set(Some(false));
        backend.target_valid.set(false);
        assert!(!ready(&backend, &target()));
    }

    #[test]
    fn diagnosis_names_selection_and_ime_only_for_a_paste_after_input() {
        let backend = MockBackend::new();
        backend.ime.set(None);
        backend.text.borrow_mut().selected = "user text".into();
        let monitor = monitor();
        let checkpoint = monitor.checkpoint();
        assert_eq!(
            diagnose(
                &backend,
                &target(),
                &monitor,
                checkpoint,
                false,
                FinalGuard::AfterInput
            ),
            InsertionDetail::ImeUnknown
        );
        assert_eq!(
            diagnose(
                &backend,
                &target(),
                &monitor,
                checkpoint,
                false,
                FinalGuard::Untouched
            ),
            InsertionDetail::NotConfirmed
        );
    }

    #[test]
    fn final_unverified_paste_respects_target_activity_modifiers_and_cancel() {
        for reason in ["target", "input", "modifiers", "cancel", "ime"] {
            let backend = MockBackend::new();
            backend.text_readable.set(false);
            let monitor = monitor();
            let checkpoint = monitor.checkpoint().unwrap();
            let (sender, cancel) = watch::channel(false);
            match reason {
                "target" => backend.target_valid.set(false),
                "input" => monitor.test_record_input(),
                "modifiers" => monitor.test_set_async_modifier_pending(true),
                "cancel" => {
                    sender.send_replace(true);
                }
                "ime" => backend.ime.set(Some(true)),
                _ => unreachable!(),
            }
            let result = insert_final(
                &backend,
                InjectionOptions::default(),
                "final",
                &target(),
                &monitor,
                checkpoint,
                &cancel,
                FinalGuard::AfterInput,
            );
            if reason == "cancel" {
                assert_eq!(result, Err(InjectionError::Cancelled));
            } else {
                assert_eq!(result.unwrap(), InsertResult::ClipboardOnly);
            }
            assert!(!backend.calls.borrow().contains(&"paste"), "{reason}");
        }
    }

    #[test]
    fn final_paste_after_renewed_input_preserves_selected_text_and_requires_known_ime() {
        for readable in [true, false] {
            for guard in [
                "selection",
                "selection_unreadable",
                "ime_active",
                "ime_unknown",
            ] {
                let backend = MockBackend::new();
                backend.text_readable.set(readable);
                match guard {
                    "selection" => backend.text.borrow_mut().selected = "user paragraph".into(),
                    "selection_unreadable" => backend.selection_readable.set(false),
                    "ime_active" => backend.ime.set(Some(true)),
                    "ime_unknown" => backend.ime.set(None),
                    _ => unreachable!(),
                }
                let original = backend.content();
                let monitor = monitor();
                // Recording-time input was ignored by renewing the checkpoint.
                monitor.test_record_input();
                let checkpoint = monitor.checkpoint().unwrap();
                let (_sender, cancel) = watch::channel(false);
                // Only an open composition can change by waiting; the other
                // states are refused immediately by the paste guard below.
                assert_eq!(
                    ready(&backend, &target()),
                    guard != "ime_active",
                    "{guard} / {readable}"
                );
                assert_eq!(
                    insert_final(
                        &backend,
                        InjectionOptions::default(),
                        "final",
                        &target(),
                        &monitor,
                        checkpoint,
                        &cancel,
                        FinalGuard::AfterInput
                    )
                    .unwrap(),
                    InsertResult::ClipboardOnly,
                    "{guard} / {readable}"
                );
                assert_eq!(backend.content(), original);
                assert_eq!(*backend.clipboard.borrow(), "final");
                assert!(!backend.calls.borrow().contains(&"paste"));
            }
        }
    }

    #[test]
    fn final_paste_rechecks_selection_and_ime_after_clipboard_access() {
        for readable in [true, false] {
            for guard in [
                "selection",
                "selection_unreadable",
                "ime_active",
                "ime_unknown",
            ] {
                let mut backend = MockBackend::new();
                backend.text_readable.set(readable);
                backend.on_clipboard_write = Some(Box::new(move |backend| match guard {
                    "selection" => backend.text.borrow_mut().selected = "user paragraph".into(),
                    "selection_unreadable" => backend.selection_readable.set(false),
                    "ime_active" => backend.ime.set(Some(true)),
                    "ime_unknown" => backend.ime.set(None),
                    _ => unreachable!(),
                }));
                let monitor = monitor();
                let (_sender, cancel) = watch::channel(false);
                assert_eq!(
                    insert_final(
                        &backend,
                        InjectionOptions::default(),
                        "final",
                        &target(),
                        &monitor,
                        monitor.checkpoint().unwrap(),
                        &cancel,
                        FinalGuard::AfterInput
                    )
                    .unwrap(),
                    InsertResult::ClipboardOnly,
                    "{guard} / {readable}"
                );
                assert!(!backend.calls.borrow().contains(&"paste"));
                assert!(!backend.calls.borrow().contains(&"restore"));
            }
        }
    }

    #[test]
    fn final_paste_after_pointer_activity_still_inserts_once_when_guards_hold() {
        let backend = MockBackend::new();
        let monitor = monitor();
        monitor.test_record_input();
        let checkpoint = monitor.checkpoint().unwrap();
        let (_sender, cancel) = watch::channel(false);
        assert!(ready(&backend, &target()));
        assert_eq!(
            insert_final(
                &backend,
                InjectionOptions::default(),
                "final",
                &target(),
                &monitor,
                checkpoint,
                &cancel,
                FinalGuard::AfterInput
            )
            .unwrap(),
            InsertResult::ClipboardPaste
        );
        assert_eq!(backend.content(), "prefix final suffix");
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

    fn insert_final_into_proxy(before: &str, readback: TargetText) -> (MockBackend, InsertResult) {
        let backend = MockBackend::new();
        backend.text.borrow_mut().before = before.into();
        *backend.readback_after_paste.borrow_mut() = Some(readback);
        let monitor = monitor();
        let checkpoint = monitor.checkpoint().unwrap();
        let (_sender, cancel) = watch::channel(false);
        let result = insert_final(
            &backend,
            InjectionOptions::default(),
            "音声入力のテスト",
            &target(),
            &monitor,
            checkpoint,
            &cancel,
            FinalGuard::AfterInput,
        )
        .unwrap();
        (backend, result)
    }

    fn proxy_text(before: &str, after: &str) -> TargetText {
        TargetText {
            identity: vec![1],
            before: before.into(),
            selected: String::new(),
            after: after.into(),
        }
    }

    #[test]
    fn unknown_password_state_copies_without_pasting_even_when_text_is_unreadable() {
        let backend = MockBackend::new();
        backend.text_readable.set(false);
        let mut captured = target();
        captured.is_secure =
            crate::injection::target::secure_after_password_query(false, false, Err(()));
        assert_eq!(
            insert(
                &backend,
                InjectionOptions::default(),
                "dictated text",
                &captured
            )
            .unwrap(),
            InsertResult::ClipboardOnly
        );
        assert_eq!(calls(&backend, "paste"), 0);
        assert_eq!(&*backend.clipboard.borrow(), "dictated text");
        captured.is_secure =
            crate::injection::target::secure_after_password_query(false, false, Ok(false));
        assert_eq!(
            insert(
                &backend,
                InjectionOptions::default(),
                "dictated text",
                &captured
            )
            .unwrap(),
            InsertResult::PasteUnverified
        );
        assert_eq!(calls(&backend, "paste"), 1);
    }

    #[test]
    fn bounded_probe_never_uses_a_partial_exact_match_to_restore_clipboard() {
        // Insertion can retain the same disjoint count when it splits an old
        // match. A coincidentally identical *window* must request the full read.
        let before = proxy_text("a", "ba");
        let expected = before.replaced_with("aba");
        let probe = TextProbe {
            state: expected.clone(),
            complete: false,
        };
        assert_eq!(
            probe_confirmation(
                &probe,
                &before,
                &expected,
                "aba",
                &std::cell::OnceCell::new()
            ),
            None
        );
        let complete = TextProbe {
            complete: true,
            ..probe
        };
        assert_eq!(
            probe_confirmation(
                &complete,
                &before,
                &expected,
                "aba",
                &std::cell::OnceCell::new()
            ),
            Some(true)
        );
    }

    #[test]
    fn every_decisive_partial_probe_agrees_with_the_previous_full_reader_rule() {
        for needle in ["aba", "かな", "😀e\u{301}"] {
            for (prefix, suffix) in [("prefix ", " suffix"), ("aba ", "aba")] {
                let before = proxy_text(prefix, suffix);
                let expected = before.replaced_with(needle);
                let evidence = std::cell::OnceCell::new();
                for actual_before in [
                    format!("prefix {needle}"),
                    format!("{needle}x{needle}{needle} \n"),
                    "waiting prefix".into(),
                ] {
                    for actual_after in ["", " suffix", needle] {
                        let full = proxy_text(&actual_before, actual_after);
                        let baseline_count = format!("{prefix}{suffix}").matches(needle).count();
                        let old_rule = full.same_content(&expected)
                            || (full.before.trim_end().ends_with(needle)
                                && format!("{actual_before}{actual_after}")
                                    .matches(needle)
                                    .count()
                                    > baseline_count);
                        for start in actual_before
                            .char_indices()
                            .map(|(index, _)| index)
                            .chain(std::iter::once(actual_before.len()))
                        {
                            for end in actual_after
                                .char_indices()
                                .map(|(index, _)| index)
                                .chain(std::iter::once(actual_after.len()))
                            {
                                let window = TextProbe {
                                    state: proxy_text(
                                        &actual_before[start..],
                                        &actual_after[..end],
                                    ),
                                    complete: false,
                                };
                                if let Some(decision) = probe_confirmation(
                                    &window, &before, &expected, needle, &evidence,
                                ) {
                                    assert_eq!(decision, old_rule, "{needle:?}, window {start}/{end}, full {actual_before:?}/{actual_after:?}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn bounded_confirmation_saves_a_full_read_and_ambiguous_counts_fall_back() {
        let plain = MockBackend::new();
        assert_eq!(
            insert(&plain, InjectionOptions::default(), "novel text", &target()).unwrap(),
            InsertResult::ClipboardPaste
        );
        let bounded = MockBackend::new();
        *bounded.probe_state.borrow_mut() = Some(TextProbe {
            state: proxy_text("fix novel text", " suf"),
            complete: false,
        });
        assert_eq!(
            insert(
                &bounded,
                InjectionOptions::default(),
                "novel text",
                &target()
            )
            .unwrap(),
            InsertResult::ClipboardPaste
        );
        assert_eq!(bounded.content(), plain.content());
        assert_eq!(bounded.text_reads.get() + 1, plain.text_reads.get());
        assert_eq!(bounded.probe_reads.get(), 1);

        let ambiguous = MockBackend::new();
        *ambiguous.text.borrow_mut() = proxy_text("a", "ba");
        *ambiguous.probe_state.borrow_mut() = Some(TextProbe {
            state: proxy_text("aaba", "ba"),
            complete: false,
        });
        assert_eq!(
            insert(&ambiguous, InjectionOptions::default(), "aba", &target()).unwrap(),
            InsertResult::ClipboardPaste
        );
        assert_eq!(ambiguous.content(), "aababa");
        assert!(ambiguous.calls.borrow().contains(&"restore"));
    }

    #[test]
    fn pending_probe_without_suffix_never_restores_or_polls_full_text() {
        let backend = MockBackend::new();
        *backend.probe_state.borrow_mut() = Some(TextProbe {
            state: proxy_text("prefix still pending", " suffix"),
            complete: false,
        });
        assert_eq!(
            insert(
                &backend,
                InjectionOptions::default(),
                "novel text",
                &target()
            )
            .unwrap(),
            InsertResult::PasteUnverified
        );
        assert_eq!(backend.probe_reads.get(), VERIFY_ATTEMPTS + 1);
        assert!(backend.text_reads.get() < backend.probe_reads.get());
        assert!(!backend.calls.borrow().contains(&"restore"));
        assert_eq!(&*backend.clipboard.borrow(), "novel text");
    }

    #[test]
    fn unavailable_or_ambiguous_probes_are_attempted_only_once_per_paste() {
        for failure in [false, true] {
            let mut backend = MockBackend::new();
            backend.probe_fails.set(failure);
            backend.settle_after = VERIFY_ATTEMPTS + 2;
            assert_eq!(
                insert(
                    &backend,
                    InjectionOptions::default(),
                    "novel text",
                    &target()
                )
                .unwrap(),
                InsertResult::PasteUnverified
            );
            assert_eq!(backend.probe_reads.get(), 1);
        }
        let mut backend = MockBackend::new();
        backend.settle_after = VERIFY_ATTEMPTS + 2;
        *backend.text.borrow_mut() = proxy_text("a", "ba");
        *backend.probe_state.borrow_mut() = Some(TextProbe {
            state: proxy_text("aaba", "ba"),
            complete: false,
        });
        assert_eq!(
            insert(&backend, InjectionOptions::default(), "aba", &target()).unwrap(),
            InsertResult::PasteUnverified
        );
        assert_eq!(backend.probe_reads.get(), 1);
        assert!(!backend.calls.borrow().contains(&"restore"));
    }

    #[test]
    fn short_or_whitespace_probe_tails_request_the_full_reader() {
        let before = proxy_text("prefix", "suffix");
        let expected = before.replaced_with("long sample");
        for tail in ["ple", "    "] {
            let probe = TextProbe {
                state: proxy_text(tail, "suffix"),
                complete: false,
            };
            assert_eq!(
                probe_confirmation(
                    &probe,
                    &before,
                    &expected,
                    "long sample",
                    &std::cell::OnceCell::new()
                ),
                None
            );
        }
        let backend = MockBackend::new();
        *backend.probe_state.borrow_mut() = Some(TextProbe {
            state: proxy_text("unused", "unused"),
            complete: false,
        });
        assert_eq!(
            insert(&backend, InjectionOptions::default(), " \r\n ", &target()).unwrap(),
            InsertResult::ClipboardPaste
        );
        assert_eq!(backend.probe_reads.get(), 0);
    }

    #[test]
    fn streaming_occurrence_counts_match_disjoint_string_matches_across_all_range_boundaries() {
        for whole in [
            "aaaaa",
            "ababababa",
            "かなかなかな",
            "A😀e\u{301}日本😀",
            "a\nb\na\nb",
            "",
        ] {
            let boundaries: Vec<_> = whole
                .char_indices()
                .map(|(index, _)| index)
                .chain(std::iter::once(whole.len()))
                .collect();
            for &first in &boundaries {
                for &second in boundaries.iter().filter(|&&index| index >= first) {
                    let state = TargetText {
                        identity: vec![1],
                        before: whole[..first].into(),
                        selected: whole[first..second].into(),
                        after: whole[second..].into(),
                    };
                    for needle in [
                        "a", "aa", "aba", "かな", "😀", "e\u{301}", "日本", "a\r\nb", "missing",
                    ] {
                        let evidence = InsertedTextEvidence::new(&state, needle).unwrap();
                        assert_eq!(
                            evidence.count(&state),
                            whole.matches(evidence.text.as_str()).count(),
                            "{whole:?} split {first}/{second}, needle {needle:?}"
                        );
                        assert!(
                            !evidence.confirms(&state),
                            "unchanged text must never confirm a paste"
                        );
                    }
                }
            }
        }
        assert!(InsertedTextEvidence::new(&proxy_text("", ""), " \r\n\t").is_none());
    }

    #[test]
    fn additive_paste_is_confirmed_when_a_windowed_editor_shows_one_more_occurrence() {
        // The editor exposes a shifted window around the caret, not the
        // exact before + text + after document.
        let (backend, result) =
            insert_final_into_proxy("prefix ", proxy_text("fix 音声入力のテスト", " suf"));
        assert_eq!(result, InsertResult::ClipboardPaste);
        assert!(backend.calls.borrow().contains(&"restore"));
    }

    #[test]
    fn additive_paste_stays_unverified_without_a_new_occurrence() {
        // A proxy that never reflects the document, and a field that already
        // contained the same text, both leave the payload on the clipboard.
        // Text appearing away from the caret (for example terminal output)
        // must not confirm a paste that may still be pending.
        for (before, readback) in [
            ("prefix ", proxy_text("", "")),
            (
                "音声入力のテスト ",
                proxy_text("音声入力のテスト ", " suffix"),
            ),
            ("prefix ", proxy_text("prefix ", " 音声入力のテスト suffix")),
        ] {
            let (backend, result) = insert_final_into_proxy(before, readback);
            assert_eq!(result, InsertResult::PasteUnverified, "{before}");
            assert!(!backend.calls.borrow().contains(&"restore"), "{before}");
            assert_eq!(*backend.clipboard.borrow(), "音声入力のテスト");
        }
    }

    #[test]
    fn final_insert_cancelled_during_guards_never_writes_the_clipboard() {
        for case in ["unreadable", "target"] {
            let mut backend = MockBackend::new();
            let monitor = monitor();
            let checkpoint = monitor.checkpoint().unwrap();
            let (sender, cancel) = watch::channel(false);
            // The cancel lands after the initial check, while guards run.
            backend.on_validate = Some(Box::new(move || {
                sender.send_replace(true);
            }));
            match case {
                "unreadable" => backend.text_readable.set(false),
                "target" => backend.target_valid.set(false),
                _ => unreachable!(),
            }
            let result = insert_final(
                &backend,
                InjectionOptions::default(),
                "final",
                &target(),
                &monitor,
                checkpoint,
                &cancel,
                FinalGuard::AfterInput,
            );
            assert_eq!(result, Err(InjectionError::Cancelled), "{case}");
            assert_eq!(*backend.clipboard.borrow(), "original rich clipboard");
            assert!(!backend.calls.borrow().contains(&"paste"), "{case}");
        }
    }

    #[test]
    fn diagnosis_reports_guards_in_insertion_order() {
        let backend = MockBackend::new();
        backend.target_valid.set(false);
        backend.ime.set(Some(true));
        backend.text_readable.set(false);
        let monitor = monitor();
        let checkpoint = monitor.checkpoint();
        assert_eq!(
            diagnose(
                &backend,
                &target(),
                &monitor,
                checkpoint,
                false,
                FinalGuard::AfterInput
            ),
            InsertionDetail::TargetChanged
        );
        monitor.test_record_input();
        assert_eq!(
            diagnose(
                &backend,
                &target(),
                &monitor,
                checkpoint,
                false,
                FinalGuard::AfterInput
            ),
            InsertionDetail::UserActivity
        );
        backend.target_valid.set(true);
        let monitor = self::monitor();
        assert_eq!(
            diagnose(
                &backend,
                &target(),
                &monitor,
                monitor.checkpoint(),
                false,
                FinalGuard::AfterInput
            ),
            InsertionDetail::ImeActive
        );
    }

    #[test]
    fn monitored_insert_cancellation_does_not_copy_or_overwrite_clipboard() {
        let backend = MockBackend::new();
        let monitor = monitor();
        let (_sender, cancel) = watch::channel(true);
        let result = insert_monitored(
            &backend,
            InjectionOptions::default(),
            "cancelled",
            &target(),
            &monitor,
            monitor.checkpoint().unwrap(),
            &cancel,
        );
        assert_eq!(result, Err(InjectionError::Cancelled));
        assert_eq!(&*backend.clipboard.borrow(), "original rich clipboard");
        assert!(!backend.calls.borrow().contains(&"write"));
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
    fn cancelled_ask_selection_never_copies_or_pastes() {
        let backend = MockBackend::new();
        *backend.text.borrow_mut() = selected_state();
        let before = backend.text.borrow().clone();
        let monitor = monitor();
        let (_sender, cancel) = watch::channel(true);
        let result = replace_selection_monitored(
            &backend,
            InjectionOptions::default(),
            &target(),
            &before,
            "answer",
            (&monitor, monitor.checkpoint().unwrap()),
            &cancel,
        );
        assert_eq!(result, Err(InjectionError::Cancelled));
        assert_eq!(backend.content(), "prefix original suffix");
        assert_eq!(&*backend.clipboard.borrow(), "original rich clipboard");
        assert!(!backend.calls.borrow().contains(&"write"));
        assert!(!backend.calls.borrow().contains(&"paste"));
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
    fn injected_ctrl_during_paste_does_not_invalidate_confirmed_selection_edit() {
        let mut backend = MockBackend::new();
        *backend.text.borrow_mut() = selected_state();
        let before = backend.text.borrow().clone();
        let monitor = std::sync::Arc::new(monitor());
        let checkpoint = monitor.checkpoint().unwrap();
        let during_paste = std::sync::Arc::clone(&monitor);
        backend.on_paste = Some(Box::new(move || {
            during_paste.test_set_async_modifier_pending(true);
        }));

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
        assert!(backend.calls.borrow().contains(&"restore"));
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

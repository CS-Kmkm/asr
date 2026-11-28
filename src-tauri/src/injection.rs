//! Batch text insertion with verified provisional-range replacement.
//! Target text is inspected transiently for verification, never logged.

use crate::input_monitor::InputMonitor;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::sync::watch;

#[cfg(all(test, target_os = "windows"))]
mod native_tests;

#[cfg(target_os = "windows")]
mod accessibility;
mod batch;
#[cfg(target_os = "windows")]
mod clipboard;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
mod keys;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
mod target;

pub(crate) const INJECTION_MARKER: usize = 0x4C56_494A;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetWindow {
    pub window_handle: isize,
    pub control_handle: isize,
    pub process_id: u32,
    pub thread_id: u32,
    pub is_secure: bool,
}

impl TargetWindow {
    /// Stands for this app's own window, which is never an insertion target.
    /// It has no window handle, so it never matches a captured foreground
    /// window; every insertion copies the result instead.
    pub(crate) fn this_app() -> Self {
        Self {
            window_handle: 0,
            control_handle: 0,
            process_id: std::process::id(),
            thread_id: 0,
            is_secure: false,
        }
    }

    pub(crate) fn is_this_app(&self) -> bool {
        self.window_handle == 0 && self.process_id == std::process::id()
    }
}

/// The target of a recording. Starting from this app's own UI (its Record
/// button) leaves this app in the foreground, so there is no external field
/// to insert into and the result is delivered on the clipboard. A hotkey
/// keeps whatever foreground window it captured.
/// The overlay notice for an insertion outcome. When Restore clipboard was on
/// and the attempt left this app's text in place of the user's clipboard, the
/// notice says the previous clipboard was not restored. With Restore
/// clipboard off nothing is reported: the user accepted the replacement.
pub(crate) fn notice_outcome(outcome: &str, clipboard_not_restored: bool) -> &str {
    if !clipboard_not_restored {
        return outcome;
    }
    match outcome {
        "clipboard_paste" | "provisional_replace" => "clipboard_not_restored",
        "clipboard_only" => "clipboard_only_not_restored",
        "paste_unverified" => "paste_unverified_not_restored",
        "draft_kept" => "draft_kept_not_restored",
        "draft_unverified" => "draft_unverified_not_restored",
        "insertion_failed" => "insertion_failed_not_restored",
        other => other,
    }
}

pub(crate) fn recording_target(captured: TargetWindow, from_shortcut: bool) -> TargetWindow {
    if !from_shortcut && captured.process_id == std::process::id() {
        TargetWindow::this_app()
    } else {
        captured
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertResult {
    ClipboardPaste,
    DirectInsert,
    ClipboardOnly,
    /// Input was queued, but the target edit was not confirmed. Do not retry,
    /// restore the old clipboard, or overwrite a possibly pending payload.
    PasteUnverified,
}

impl InsertResult {
    pub(crate) fn confirmed(self) -> bool {
        matches!(self, Self::ClipboardPaste | Self::DirectInsert)
    }
}

/// Fixed reason for an insertion that was not confirmed. The code is stored
/// in History and shown to the user; it never contains target or text content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertionDetail {
    InputMonitorUnavailable,
    UserActivity,
    TargetChanged,
    TargetProtected,
    ImeActive,
    ImeUnknown,
    SelectionActive,
    SelectionUnknown,
    TargetUnreadable,
    NotConfirmed,
    ClipboardUnavailable,
    BackendFailure,
    /// The recording was started from this app's own window.
    NoExternalTarget,
    /// The paste was confirmed, but the previous clipboard could not be
    /// restored, so the pasted text replaced it.
    ClipboardNotRestored,
    /// Restore clipboard is on and the clipboard could not be saved, so it was
    /// left untouched and the result was kept in the app.
    ClipboardProtected,
}

impl InsertionDetail {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::InputMonitorUnavailable => "input_monitor_unavailable",
            Self::UserActivity => "user_activity",
            Self::TargetChanged => "target_changed",
            Self::TargetProtected => "target_protected",
            Self::ImeActive => "ime_active",
            Self::ImeUnknown => "ime_unknown",
            Self::SelectionActive => "selection_active",
            Self::SelectionUnknown => "selection_unknown",
            Self::TargetUnreadable => "target_unreadable",
            Self::NotConfirmed => "not_confirmed",
            Self::ClipboardUnavailable => "clipboard_unavailable",
            Self::BackendFailure => "backend_failure",
            Self::NoExternalTarget => "no_external_target",
            Self::ClipboardNotRestored => "clipboard_not_restored",
            Self::ClipboardProtected => "clipboard_protected",
        }
    }

    /// The only detail a confirmed paste carries: it left the clipboard
    /// unrestored.
    pub(crate) fn for_confirmed_paste(
        result: InsertResult,
        clipboard_not_restored: bool,
    ) -> Option<Self> {
        (result == InsertResult::ClipboardPaste && clipboard_not_restored)
            .then_some(Self::ClipboardNotRestored)
    }

    /// Cancellation is not an insertion outcome and has no detail.
    pub(crate) fn from_error(error: &InjectionError) -> Option<Self> {
        match error {
            InjectionError::Cancelled => None,
            InjectionError::NoForegroundWindow | InjectionError::TargetChanged => {
                Some(Self::TargetChanged)
            }
            InjectionError::SecureTarget | InjectionError::PrivilegeMismatch => {
                Some(Self::TargetProtected)
            }
            InjectionError::ClipboardUnavailable | InjectionError::ClipboardChanged => {
                Some(Self::ClipboardUnavailable)
            }
            InjectionError::ClipboardUnpreservable | InjectionError::ClipboardProtected => {
                Some(Self::ClipboardProtected)
            }
            InjectionError::UnsupportedPlatform
            | InjectionError::NoSelection
            | InjectionError::BackendFailure(_) => Some(Self::BackendFailure),
        }
    }
}

#[cfg(test)]
mod outcome_tests {
    use super::*;

    fn reporting_injector(
        options: InjectionOptions,
    ) -> (
        SystemTextInjector,
        std::sync::Arc<std::sync::Mutex<test_backend::MockBackend>>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let mock = std::sync::Arc::new(std::sync::Mutex::new(test_backend::MockBackend::new()));
        let reports = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut injector = SystemTextInjector::with_mock(std::sync::Arc::clone(&mock), options);
        let counter = std::sync::Arc::clone(&reports);
        injector.report_clipboard_loss_on_drop(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        (injector, mock, reports)
    }

    #[test]
    fn an_early_return_after_a_copy_still_reports_the_lost_clipboard() {
        // Ask's clarification or a missing translation language copies the
        // text, then returns without an insertion notice.
        let (injector, _mock, reports) = reporting_injector(InjectionOptions::default());
        injector.copy_to_clipboard("clarification").unwrap();
        drop(injector);
        assert_eq!(reports.load(Ordering::SeqCst), 1);
        // A completion that reported the loss itself is not repeated.
        let (injector, _mock, reports) = reporting_injector(InjectionOptions::default());
        injector.copy_to_clipboard("result").unwrap();
        assert!(injector.take_clipboard_not_restored());
        drop(injector);
        assert_eq!(reports.load(Ordering::SeqCst), 0);
        // With Restore clipboard off nothing was lost.
        let (injector, _mock, reports) = reporting_injector(InjectionOptions {
            restore_clipboard: false,
        });
        injector.copy_to_clipboard("result").unwrap();
        drop(injector);
        assert_eq!(reports.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_session_dropped_after_an_unconfirmed_paste_reports_the_lost_clipboard() {
        // Cancelling or failing after a live or final paste was queued.
        let (injector, mock, reports) = reporting_injector(InjectionOptions::default());
        mock.lock().unwrap().text_readable.set(false);
        assert_eq!(
            injector.insert("draft", &test_backend::target()),
            Ok(InsertResult::PasteUnverified)
        );
        drop(injector);
        assert_eq!(reports.load(Ordering::SeqCst), 1);
        // A confirmed paste that restored the clipboard has nothing to report.
        let (injector, _mock, reports) = reporting_injector(InjectionOptions::default());
        assert_eq!(
            injector.insert("draft", &test_backend::target()),
            Ok(InsertResult::ClipboardPaste)
        );
        drop(injector);
        assert_eq!(reports.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn every_outcome_notice_names_a_lost_clipboard() {
        for outcome in [
            "clipboard_paste",
            "provisional_replace",
            "clipboard_only",
            "paste_unverified",
            "draft_kept",
            "draft_unverified",
            "insertion_failed",
        ] {
            assert_eq!(notice_outcome(outcome, false), outcome);
            assert!(
                notice_outcome(outcome, true).ends_with("not_restored"),
                "{outcome}"
            );
        }
    }

    #[test]
    fn only_a_confirmed_paste_reports_an_unrestored_clipboard() {
        assert_eq!(
            InsertionDetail::for_confirmed_paste(InsertResult::ClipboardPaste, true),
            Some(InsertionDetail::ClipboardNotRestored)
        );
        assert_eq!(
            InsertionDetail::ClipboardNotRestored.code(),
            "clipboard_not_restored"
        );
        assert_eq!(
            InsertionDetail::for_confirmed_paste(InsertResult::ClipboardPaste, false),
            None
        );
        // Other outcomes leave the text on the clipboard by design.
        for result in [InsertResult::ClipboardOnly, InsertResult::PasteUnverified] {
            assert_eq!(InsertionDetail::for_confirmed_paste(result, true), None);
        }
    }
}

pub(crate) struct ProvisionalInsertion {
    target: TargetWindow,
    displayed: String,
    replacement: Option<ReplacementRange>,
    result: InsertResult,
    finished: bool,
}

struct ReplacementRange {
    after: TargetText,
    checkpoint: u64,
}

impl ProvisionalInsertion {
    pub(crate) fn paste_was_queued(&self) -> bool {
        self.result != InsertResult::ClipboardOnly
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InjectionError {
    // Non-Windows builds return this from their platform backend.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    UnsupportedPlatform,
    NoForegroundWindow,
    TargetChanged,
    SecureTarget,
    PrivilegeMismatch,
    /// The clipboard is locked by another process.
    ClipboardUnavailable,
    /// Ownership of the clipboard payload changed or could not be established.
    ClipboardChanged,
    /// The clipboard could be opened, but holds data that cannot be copied
    /// (too many or too large formats, private or owner-drawn formats, or a
    /// delay-rendering owner that fails to render).
    ClipboardUnpreservable,
    /// Restore clipboard is on and the clipboard cannot be snapshotted, so it
    /// was left untouched: nothing was written or pasted.
    ClipboardProtected,
    Cancelled,
    /// The focused field is readable but nothing is selected in it.
    NoSelection,
    BackendFailure(&'static str),
}

impl fmt::Display for InjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedPlatform => "text injection is unsupported on this platform",
            Self::NoForegroundWindow => "no foreground target is available",
            Self::TargetChanged => "the foreground target changed before insertion",
            Self::SecureTarget => "text insertion into secure controls is disabled",
            Self::PrivilegeMismatch => "the target cannot be accessed at this privilege level",
            Self::ClipboardUnavailable => "the clipboard is unavailable",
            Self::ClipboardChanged => "clipboard ownership could not be verified before insertion",
            Self::ClipboardUnpreservable => "the clipboard holds data that cannot be preserved",
            Self::ClipboardProtected => {
                "the clipboard was left unchanged because it could not be saved for restoring"
            }
            Self::Cancelled => "text insertion was cancelled",
            Self::NoSelection => "no text is selected",
            Self::BackendFailure(message) => message,
        })
    }
}
impl std::error::Error for InjectionError {}

pub trait TextInjector: Send + Sync {
    fn capture_target(&self) -> Result<TargetWindow, InjectionError>;
    #[cfg(test)]
    fn insert(&self, text: &str, target: &TargetWindow) -> Result<InsertResult, InjectionError>;
}

#[derive(Clone, Copy, Debug)]
pub struct InjectionOptions {
    pub restore_clipboard: bool,
}
impl Default for InjectionOptions {
    fn default() -> Self {
        Self {
            restore_clipboard: true,
        }
    }
}

pub struct SystemTextInjector {
    backend: InjectorBackend,
    options: InjectionOptions,
    /// Reports a lost user clipboard that no caller took before drop.
    report_loss: Option<Box<dyn FnOnce() + Send + Sync>>,
}

#[cfg(not(test))]
type InjectorBackend = PlatformBackend;
#[cfg(test)]
type InjectorBackend = test_backend::Switchable;

pub(crate) struct SelectedText {
    target: TargetWindow,
    text: String,
    state: TargetText,
}
/// Why a Smart Dictate/Edit press could not tell whether text is selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnknownSelection {
    /// No foreground field, or it changed while being read.
    NoTarget,
    /// This app's own window is in the foreground.
    ThisApp,
    /// A password field or a target at a higher privilege level.
    Protected,
    /// An IME conversion is open.
    ImeComposing,
    /// The field's text and selection cannot be read.
    Unreadable,
}

impl UnknownSelection {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::NoTarget => "no_target",
            Self::ThisApp => "this_app",
            Self::Protected => "protected",
            Self::ImeComposing => "ime_composing",
            Self::Unreadable => "unreadable",
        }
    }
}

/// The original target and selection state, read once at a Smart
/// Dictate/Edit press.
pub(crate) enum SelectionProbe {
    /// A readable, nonempty selection in an external, unprotected field.
    Selected(SelectedText),
    /// An external, unprotected field whose selection is collapsed, with the
    /// UI Automation identity of the focused field (always known: a field
    /// that cannot be identified is `Unknown`).
    Empty {
        target: TargetWindow,
        focus_identity: Vec<i32>,
    },
    Unknown(UnknownSelection),
}

impl SelectedText {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn target(&self) -> &TargetWindow {
        &self.target
    }
}
impl SystemTextInjector {
    pub(crate) fn is_terminal_target(&self, target: &TargetWindow) -> bool {
        self.backend.is_terminal(target)
    }

    pub fn new(options: InjectionOptions) -> Self {
        Self {
            backend: InjectorBackend::new(),
            options,
            report_loss: None,
        }
    }
    /// Drives the batch mock target, so callers such as `LiveDraft` can be
    /// tested over the same injection paths as production.
    #[cfg(test)]
    pub(crate) fn with_mock(
        mock: std::sync::Arc<std::sync::Mutex<test_backend::MockBackend>>,
        options: InjectionOptions,
    ) -> Self {
        Self {
            backend: test_backend::Switchable {
                platform: PlatformBackend::new(),
                mock: Some(mock),
            },
            options,
            report_loss: None,
        }
    }
    pub(crate) fn begin_live_provisional(
        &self,
        draft: &str,
        target: &TargetWindow,
        monitor: &InputMonitor,
        checkpoint: u64,
    ) -> Result<Option<ProvisionalInsertion>, InjectionError> {
        batch::begin_live(
            &self.backend,
            self.options,
            draft,
            target,
            monitor,
            checkpoint,
        )
    }
    pub(crate) fn finish_provisional(
        &self,
        session: &mut ProvisionalInsertion,
        final_text: &str,
        monitor: &InputMonitor,
    ) -> Result<InsertResult, InjectionError> {
        batch::finish(&self.backend, self.options, session, final_text, monitor)
    }
    pub(crate) fn update_provisional(
        &self,
        session: &mut ProvisionalInsertion,
        text: &str,
        monitor: &InputMonitor,
    ) -> Result<InsertResult, InjectionError> {
        batch::update(&self.backend, self.options, session, text, monitor, false)
    }
    /// Removes the whole provisional draft; never edits text outside it.
    pub(crate) fn retract_provisional(
        &self,
        session: &mut ProvisionalInsertion,
        monitor: &InputMonitor,
    ) -> Result<InsertResult, InjectionError> {
        batch::retract(&self.backend, session, monitor)
    }
    pub(crate) fn cancel_provisional(
        &self,
        session: &mut ProvisionalInsertion,
        monitor: &InputMonitor,
    ) {
        batch::cancel(&self.backend, session, monitor)
    }

    /// Reads the focused field's selection state without sending input.
    pub(crate) fn probe_selection(&self) -> SelectionProbe {
        batch::probe_selection(&self.backend)
    }

    pub(crate) fn capture_selection(&self) -> Result<SelectedText, InjectionError> {
        let (target, state) = batch::capture_selection(&self.backend)?;
        Ok(SelectedText {
            target,
            text: state.selected.clone(),
            state,
        })
    }

    pub(crate) fn replace_selection(
        &self,
        selection: &SelectedText,
        text: &str,
        monitor: &InputMonitor,
        checkpoint: u64,
    ) -> Result<InsertResult, InjectionError> {
        batch::replace_selection(
            &self.backend,
            self.options,
            &selection.target,
            &selection.state,
            text,
            monitor,
            checkpoint,
        )
    }

    /// `replace_selection` that also requires the field focused when the
    /// selection was read: another field with the same text and selection
    /// is refused. The pin holds until the paste is sent; verification after
    /// it keeps its usual rules, since some editors renew the id on paste.
    pub(crate) fn replace_selection_in_same_field(
        &self,
        selection: &SelectedText,
        text: &str,
        monitor: &InputMonitor,
        checkpoint: u64,
    ) -> Result<InsertResult, InjectionError> {
        let pinned = PinnedField {
            inner: &self.backend,
            identity: &selection.state.identity,
            pasted: std::cell::Cell::new(false),
        };
        batch::replace_selection(
            &pinned,
            self.options,
            &selection.target,
            &selection.state,
            text,
            monitor,
            checkpoint,
        )
    }

    pub(crate) fn replace_selection_monitored(
        &self,
        selection: &SelectedText,
        text: &str,
        monitor: &InputMonitor,
        checkpoint: u64,
        cancel: &watch::Receiver<bool>,
    ) -> Result<InsertResult, InjectionError> {
        batch::replace_selection_monitored(
            &self.backend,
            self.options,
            &selection.target,
            &selection.state,
            text,
            (monitor, checkpoint),
            cancel,
        )
    }

    /// True once after an attempt, with Restore clipboard on, left this app's
    /// text in place of the user's clipboard: a copy-only result, a queued
    /// but unconfirmed paste, or a restore that failed.
    pub(crate) fn take_clipboard_not_restored(&self) -> bool {
        self.backend.take_clipboard_not_restored()
    }

    /// Calls `report` on drop if an attempt lost the user's clipboard and no
    /// caller took that with `take_clipboard_not_restored`. Early returns
    /// (cancellation, errors, a copy before returning) then cannot forget to
    /// report it, while completions that report it themselves are not repeated.
    pub(crate) fn report_clipboard_loss_on_drop(
        &mut self,
        report: impl FnOnce() + Send + Sync + 'static,
    ) {
        self.report_loss = Some(Box::new(report));
    }

    /// Copies `text` as an outcome. With Restore clipboard on, an
    /// unpreservable clipboard is left untouched (`ClipboardProtected`).
    pub(crate) fn copy_to_clipboard(&self, text: &str) -> Result<(), InjectionError> {
        batch::copy(&self.backend, self.options, text)
    }

    pub(crate) fn insert_final(
        &self,
        text: &str,
        target: &TargetWindow,
        monitor: &InputMonitor,
        checkpoint: u64,
        cancel: &watch::Receiver<bool>,
        after_input: bool,
    ) -> Result<InsertResult, InjectionError> {
        batch::insert_final(
            &self.backend,
            self.options,
            text,
            target,
            monitor,
            checkpoint,
            cancel,
            final_guard(after_input),
        )
    }

    pub(crate) fn target_ready(&self, target: &TargetWindow) -> bool {
        batch::ready(&self.backend, target)
    }

    pub(crate) fn focus_identity(&self, target: &TargetWindow) -> Option<Vec<i32>> {
        self.backend.focus_identity(target).ok()
    }

    pub(crate) fn diagnose_insertion(
        &self,
        target: &TargetWindow,
        monitor: &InputMonitor,
        checkpoint: Option<u64>,
        after_paste: bool,
        after_input: bool,
    ) -> InsertionDetail {
        batch::diagnose(
            &self.backend,
            target,
            monitor,
            checkpoint,
            after_paste,
            final_guard(after_input),
        )
    }

    pub(crate) fn insert_monitored(
        &self,
        text: &str,
        target: &TargetWindow,
        monitor: &InputMonitor,
        checkpoint: u64,
        cancel: &watch::Receiver<bool>,
    ) -> Result<InsertResult, InjectionError> {
        batch::insert_monitored(
            &self.backend,
            self.options,
            text,
            target,
            monitor,
            checkpoint,
            cancel,
        )
    }
}
impl Drop for SystemTextInjector {
    fn drop(&mut self) {
        if let Some(report) = self.report_loss.take() {
            if self.take_clipboard_not_restored() {
                report();
            }
        }
    }
}
impl Default for SystemTextInjector {
    fn default() -> Self {
        Self::new(InjectionOptions::default())
    }
}
impl TextInjector for SystemTextInjector {
    fn capture_target(&self) -> Result<TargetWindow, InjectionError> {
        self.backend.capture_target()
    }
    #[cfg(test)]
    fn insert(&self, text: &str, target: &TargetWindow) -> Result<InsertResult, InjectionError> {
        batch::insert(&self.backend, self.options, text, target)
    }
}

#[derive(Clone, PartialEq, Eq)]
struct TargetText {
    identity: Vec<i32>,
    before: String,
    selected: String,
    after: String,
}

/// A contiguous window around a caret. Only a complete probe may authorize
/// an exact document comparison; an incomplete one is positive evidence only.
#[derive(Clone)]
struct TextProbe {
    state: TargetText,
    complete: bool,
}
const TEXT_PROBE_UNITS: i32 = 4096;

fn with_verification_deadline<T>(deadline: Instant, run: impl FnOnce() -> T) -> T {
    #[cfg(target_os = "windows")]
    {
        accessibility::with_deadline(deadline, run)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = deadline;
        run()
    }
}
fn normalize_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}
impl TargetText {
    fn same_content(&self, other: &Self) -> bool {
        self.before == other.before && self.selected == other.selected && self.after == other.after
    }

    fn replaced_with(&self, text: &str) -> Self {
        Self {
            identity: self.identity.clone(),
            before: format!("{}{}", self.before, normalize_text(text)),
            selected: String::new(),
            after: self.after.clone(),
        }
    }
    fn select_recent(&self, text: &str) -> Option<Self> {
        let text = normalize_text(text);
        if !self.selected.is_empty() || text.is_empty() {
            return None;
        }
        let prefix = self.before.strip_suffix(&text)?;
        Some(Self {
            identity: self.identity.clone(),
            before: prefix.to_owned(),
            selected: text,
            after: self.after.clone(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClipboardExclusion {
    ExcludeFromHistory,
}

/// A deferred final paste is strict only after input since recording started.
fn final_guard(after_input: bool) -> batch::FinalGuard {
    if after_input {
        batch::FinalGuard::AfterInput
    } else {
        batch::FinalGuard::Untouched
    }
}

/// A backend that, until a paste is sent, accepts the target only while the
/// field with `identity` is focused in it.
struct PinnedField<'a, B> {
    inner: &'a B,
    identity: &'a [i32],
    pasted: std::cell::Cell<bool>,
}

impl<B: Backend> PinnedField<'_, B> {
    fn same_field(&self, target: &TargetWindow) -> Result<(), InjectionError> {
        if self.pasted.get() {
            return Ok(());
        }
        match self.inner.focus_identity(target) {
            Ok(identity) if identity == self.identity => Ok(()),
            Ok(_) => Err(InjectionError::TargetChanged),
            Err(error) => Err(error),
        }
    }
}

impl<B: Backend> Backend for PinnedField<'_, B> {
    type Clipboard = B::Clipboard;
    fn capture_target(&self) -> Result<TargetWindow, InjectionError> {
        self.inner.capture_target()
    }
    fn is_terminal(&self, target: &TargetWindow) -> bool {
        self.inner.is_terminal(target)
    }
    fn validate_target(&self, target: &TargetWindow) -> Result<(), InjectionError> {
        self.inner.validate_target(target)?;
        self.same_field(target)
    }
    fn ime_composition_active(
        &self,
        target: &TargetWindow,
    ) -> Result<Option<bool>, InjectionError> {
        self.inner.ime_composition_active(target)
    }
    fn target_text(&self, target: &TargetWindow) -> Result<TargetText, InjectionError> {
        self.same_field(target)?;
        self.inner.target_text(target)
    }
    fn target_text_probe(
        &self,
        target: &TargetWindow,
    ) -> Result<Option<TextProbe>, InjectionError> {
        self.inner.target_text_probe(target)
    }
    fn selection_is_empty(&self, target: &TargetWindow) -> Result<bool, InjectionError> {
        self.inner.selection_is_empty(target)
    }
    fn focus_identity(&self, target: &TargetWindow) -> Result<Vec<i32>, InjectionError> {
        self.inner.focus_identity(target)
    }
    fn select_recent(
        &self,
        target: &TargetWindow,
        expected: &TargetText,
        text: &str,
    ) -> Result<bool, InjectionError> {
        self.inner.select_recent(target, expected, text)
    }
    fn clipboard_write(
        &self,
        text: &str,
        exclusion: ClipboardExclusion,
        preserve: bool,
    ) -> Result<ClipboardWrite<Self::Clipboard>, WriteError<Self::Clipboard>> {
        self.inner.clipboard_write(text, exclusion, preserve)
    }
    fn clipboard_write_terminal(
        &self,
        text: &str,
        target: &TargetWindow,
    ) -> Result<ClipboardWrite<Self::Clipboard>, WriteError<Self::Clipboard>> {
        self.inner.clipboard_write_terminal(text, target)
    }
    fn clipboard_sequence(&self) -> u32 {
        self.inner.clipboard_sequence()
    }
    fn clipboard_restore(
        &self,
        snapshot: Self::Clipboard,
        expected: ClipboardStamp,
    ) -> Result<bool, InjectionError> {
        self.inner.clipboard_restore(snapshot, expected)
    }
    fn paste(
        &self,
        target: &TargetWindow,
        policy: SafetyPolicy,
        sequence: u32,
    ) -> Result<bool, InjectionError> {
        // The last check before the paste is sent; afterwards the pin ends.
        if self.same_field(target).is_err() {
            return Ok(false);
        }
        self.pasted.set(true);
        self.inner.paste(target, policy, sequence)
    }
    fn paste_with_receipt(
        &self,
        target: &TargetWindow,
        policy: SafetyPolicy,
        sequence: u32,
        receipt: Option<&dyn TerminalReceipt>,
    ) -> Result<bool, InjectionError> {
        if self.same_field(target).is_err() {
            return Ok(false);
        }
        self.pasted.set(true);
        self.inner
            .paste_with_receipt(target, policy, sequence, receipt)
    }
    fn delete_selection(&self, target: &TargetWindow) -> Result<bool, InjectionError> {
        self.same_field(target)?;
        self.inner.delete_selection(target)
    }
    fn collapse_selection(
        &self,
        target: &TargetWindow,
        expected: &TargetText,
    ) -> Result<bool, InjectionError> {
        self.same_field(target)?;
        self.inner.collapse_selection(target, expected)
    }
    fn wait_for_target(&self) {
        self.inner.wait_for_target()
    }
    fn clipboard_not_restored(&self) {
        self.inner.clipboard_not_restored()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SafetyPolicy {
    Additive,
    /// Deferred pastes renew their checkpoint and must inspect IME and selection.
    FinalAdditive,
    Destructive,
}

impl SafetyPolicy {
    fn permits_ime(self, active: Option<bool>) -> bool {
        active == Some(false) || self == Self::Additive && active.is_none()
    }

    fn permits_selection<B: Backend>(self, backend: &B, target: &TargetWindow) -> bool {
        self != Self::FinalAdditive || backend.selection_is_empty(target).is_ok_and(|empty| empty)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectDispatch {
    Declined,
    Confirmed,
    Ambiguous,
}

#[cfg(any(windows, test))]
impl DirectDispatch {
    fn from_queued_inputs(sent: usize, expected: usize) -> Self {
        match sent {
            0 => Self::Declined,
            sent if sent == expected => Self::Confirmed,
            _ => Self::Ambiguous,
        }
    }
}

#[cfg(test)]
#[test]
fn direct_dispatch_distinguishes_zero_full_and_partial_queueing() {
    assert_eq!(
        DirectDispatch::from_queued_inputs(0, 4),
        DirectDispatch::Declined
    );
    assert_eq!(
        DirectDispatch::from_queued_inputs(4, 4),
        DirectDispatch::Confirmed
    );
    assert_eq!(
        DirectDispatch::from_queued_inputs(1, 4),
        DirectDispatch::Ambiguous
    );
}

trait Backend {
    type Clipboard;
    fn capture_target(&self) -> Result<TargetWindow, InjectionError>;
    fn is_terminal(&self, _target: &TargetWindow) -> bool {
        false
    }
    fn validate_target(&self, target: &TargetWindow) -> Result<(), InjectionError>;
    fn ime_composition_active(&self, target: &TargetWindow)
        -> Result<Option<bool>, InjectionError>;
    fn target_text(&self, target: &TargetWindow) -> Result<TargetText, InjectionError>;
    fn target_text_probe(
        &self,
        _target: &TargetWindow,
    ) -> Result<Option<TextProbe>, InjectionError> {
        Ok(None)
    }
    /// Inspect the selection independently of reading the full document.
    fn selection_is_empty(&self, target: &TargetWindow) -> Result<bool, InjectionError> {
        self.target_text(target)
            .map(|text| text.selected.is_empty())
    }
    /// UI Automation runtime id of the element focused in the target process.
    /// It identifies the field, never its content.
    fn focus_identity(&self, target: &TargetWindow) -> Result<Vec<i32>, InjectionError>;
    /// Returns true when selection was requested. The target may apply it later;
    /// callers must confirm the selected text before replacing or deleting it.
    fn select_recent(
        &self,
        target: &TargetWindow,
        expected: &TargetText,
        text: &str,
    ) -> Result<bool, InjectionError>;
    fn clipboard_write(
        &self,
        text: &str,
        exclusion: ClipboardExclusion,
        preserve: bool,
    ) -> Result<ClipboardWrite<Self::Clipboard>, WriteError<Self::Clipboard>>;
    fn clipboard_write_terminal(
        &self,
        text: &str,
        _target: &TargetWindow,
    ) -> Result<ClipboardWrite<Self::Clipboard>, WriteError<Self::Clipboard>> {
        self.clipboard_write(text, ClipboardExclusion::ExcludeFromHistory, true)
    }
    fn clipboard_sequence(&self) -> u32;
    fn clipboard_restore(
        &self,
        snapshot: Self::Clipboard,
        expected: ClipboardStamp,
    ) -> Result<bool, InjectionError>;
    /// Reports whether the transport dispatched the entire edit. The caller
    /// verifies the actual field contents before claiming delivery.
    fn try_direct_insert(
        &self,
        _target: &TargetWindow,
        _before: &TargetText,
        _text: &str,
        _still_allowed: &dyn Fn() -> bool,
    ) -> DirectDispatch {
        DirectDispatch::Declined
    }
    /// Returns true if the paste key's key-down was queued, including by a
    /// partial SendInput; a modifier alone does not paste.
    /// Errors must occur before any input is queued.
    fn paste(
        &self,
        target: &TargetWindow,
        policy: SafetyPolicy,
        sequence: u32,
    ) -> Result<bool, InjectionError>;
    fn paste_with_receipt(
        &self,
        target: &TargetWindow,
        policy: SafetyPolicy,
        sequence: u32,
        _receipt: Option<&dyn TerminalReceipt>,
    ) -> Result<bool, InjectionError> {
        self.paste(target, policy, sequence)
    }
    /// Returns true if any input was queued, including a partial SendInput.
    fn delete_selection(&self, target: &TargetWindow) -> Result<bool, InjectionError>;
    /// Collapses the selection to a caret at its end when the field still
    /// holds exactly `expected`. Returns true when the collapse was requested.
    fn collapse_selection(
        &self,
        target: &TargetWindow,
        expected: &TargetText,
    ) -> Result<bool, InjectionError>;
    fn wait_for_target(&self);
    /// Records that, with Restore clipboard on, an attempt left this app's
    /// text in place of the user's clipboard.
    fn clipboard_not_restored(&self);
}

#[derive(Debug)]
struct ClipboardWrite<C> {
    sequence: u32,
    previous: Option<C>,
    receipt: Option<Box<dyn TerminalReceipt>>,
}

impl<C> ClipboardWrite<C> {
    fn eager(sequence: u32, previous: Option<C>) -> Self {
        Self {
            sequence,
            previous,
            receipt: None,
        }
    }
}

#[derive(Clone, Copy)]
struct ClipboardStamp {
    sequence: u32,
    #[cfg_attr(not(windows), allow(dead_code))]
    owner: Option<isize>,
}

trait TerminalReceipt: std::fmt::Debug {
    fn stamp(&self) -> Option<ClipboardStamp>;
    fn arm(&self) {}
    fn wait_for_read(&self, cancelled: &dyn Fn() -> bool) -> Option<ClipboardStamp>;
}

#[derive(Debug)]
enum WriteError<C> {
    /// The current contents cannot be attributed to this write.
    Unowned(InjectionError),
    /// Publication changed the clipboard, but could not close it and obtain a sequence.
    CloseFailed,
    /// The write failed after replacing the clipboard and still owns it.
    AfterEmpty {
        error: InjectionError,
        write: ClipboardWrite<C>,
    },
}

/// Remembers a lost user clipboard until the caller reports it.
#[derive(Default)]
struct UnrestoredClipboard(AtomicBool);
impl UnrestoredClipboard {
    fn record(&self) {
        self.0.store(true, Ordering::Release);
    }
    fn take(&self) -> bool {
        self.0.swap(false, Ordering::AcqRel)
    }
}

/// Test builds can swap the platform backend for the batch mock target.
#[cfg(test)]
pub(crate) mod test_backend {
    pub(crate) use super::batch::tests::{calls, target, MockBackend};
    use super::*;
    use std::sync::{Arc, Mutex};

    pub(super) struct Switchable {
        pub(super) platform: PlatformBackend,
        pub(super) mock: Option<Arc<Mutex<MockBackend>>>,
    }

    impl Switchable {
        pub(super) fn new() -> Self {
            Self {
                platform: PlatformBackend::new(),
                mock: None,
            }
        }
        pub(super) fn take_clipboard_not_restored(&self) -> bool {
            match &self.mock {
                Some(mock) => mock
                    .lock()
                    .expect("mock backend lock")
                    .take_clipboard_not_restored(),
                None => self.platform.take_clipboard_not_restored(),
            }
        }
    }

    pub(super) enum Clipboard {
        Platform(<PlatformBackend as Backend>::Clipboard),
        Mock(String),
    }

    macro_rules! delegate {
        ($self:ident, |$backend:ident| $call:expr) => {
            match &$self.mock {
                Some(mock) => {
                    let $backend = &*mock.lock().expect("mock backend lock");
                    $call
                }
                None => {
                    let $backend = &$self.platform;
                    $call
                }
            }
        };
    }

    impl Backend for Switchable {
        type Clipboard = Clipboard;
        fn capture_target(&self) -> Result<TargetWindow, InjectionError> {
            delegate!(self, |b| b.capture_target())
        }
        fn is_terminal(&self, target: &TargetWindow) -> bool {
            delegate!(self, |b| b.is_terminal(target))
        }
        fn validate_target(&self, target: &TargetWindow) -> Result<(), InjectionError> {
            delegate!(self, |b| b.validate_target(target))
        }
        fn ime_composition_active(
            &self,
            target: &TargetWindow,
        ) -> Result<Option<bool>, InjectionError> {
            delegate!(self, |b| b.ime_composition_active(target))
        }
        fn target_text(&self, target: &TargetWindow) -> Result<TargetText, InjectionError> {
            delegate!(self, |b| b.target_text(target))
        }
        fn target_text_probe(
            &self,
            target: &TargetWindow,
        ) -> Result<Option<TextProbe>, InjectionError> {
            delegate!(self, |b| b.target_text_probe(target))
        }
        fn selection_is_empty(&self, target: &TargetWindow) -> Result<bool, InjectionError> {
            delegate!(self, |b| b.selection_is_empty(target))
        }
        fn focus_identity(&self, target: &TargetWindow) -> Result<Vec<i32>, InjectionError> {
            delegate!(self, |b| b.focus_identity(target))
        }
        fn select_recent(
            &self,
            target: &TargetWindow,
            expected: &TargetText,
            text: &str,
        ) -> Result<bool, InjectionError> {
            delegate!(self, |b| b.select_recent(target, expected, text))
        }
        fn clipboard_write(
            &self,
            text: &str,
            exclusion: ClipboardExclusion,
            preserve: bool,
        ) -> Result<ClipboardWrite<Self::Clipboard>, WriteError<Self::Clipboard>> {
            fn map_write<C>(
                result: Result<ClipboardWrite<C>, WriteError<C>>,
                wrap: impl FnOnce(C) -> Clipboard,
            ) -> Result<ClipboardWrite<Clipboard>, WriteError<Clipboard>> {
                let convert = |write: ClipboardWrite<C>| ClipboardWrite {
                    sequence: write.sequence,
                    previous: write.previous.map(wrap),
                    receipt: write.receipt,
                };
                match result {
                    Ok(write) => Ok(convert(write)),
                    Err(WriteError::Unowned(error)) => Err(WriteError::Unowned(error)),
                    Err(WriteError::CloseFailed) => Err(WriteError::CloseFailed),
                    Err(WriteError::AfterEmpty { error, write }) => Err(WriteError::AfterEmpty {
                        error,
                        write: convert(write),
                    }),
                }
            }
            match &self.mock {
                Some(mock) => map_write(
                    mock.lock()
                        .expect("mock backend lock")
                        .clipboard_write(text, exclusion, preserve),
                    Clipboard::Mock,
                ),
                None => map_write(
                    self.platform.clipboard_write(text, exclusion, preserve),
                    Clipboard::Platform,
                ),
            }
        }
        fn clipboard_sequence(&self) -> u32 {
            delegate!(self, |b| b.clipboard_sequence())
        }
        fn clipboard_restore(
            &self,
            snapshot: Self::Clipboard,
            expected: ClipboardStamp,
        ) -> Result<bool, InjectionError> {
            match (&self.mock, snapshot) {
                (Some(mock), Clipboard::Mock(snapshot)) => mock
                    .lock()
                    .expect("mock backend lock")
                    .clipboard_restore(snapshot, expected),
                (None, Clipboard::Platform(snapshot)) => {
                    self.platform.clipboard_restore(snapshot, expected)
                }
                _ => unreachable!("a snapshot is restored by the backend that took it"),
            }
        }
        fn try_direct_insert(
            &self,
            target: &TargetWindow,
            before: &TargetText,
            text: &str,
            still_allowed: &dyn Fn() -> bool,
        ) -> DirectDispatch {
            delegate!(self, |b| b.try_direct_insert(
                target,
                before,
                text,
                still_allowed
            ))
        }
        fn paste(
            &self,
            target: &TargetWindow,
            policy: SafetyPolicy,
            sequence: u32,
        ) -> Result<bool, InjectionError> {
            delegate!(self, |b| b.paste(target, policy, sequence))
        }
        fn delete_selection(&self, target: &TargetWindow) -> Result<bool, InjectionError> {
            delegate!(self, |b| b.delete_selection(target))
        }
        fn collapse_selection(
            &self,
            target: &TargetWindow,
            expected: &TargetText,
        ) -> Result<bool, InjectionError> {
            delegate!(self, |b| b.collapse_selection(target, expected))
        }
        fn wait_for_target(&self) {
            delegate!(self, |b| b.wait_for_target())
        }
        fn clipboard_not_restored(&self) {
            delegate!(self, |b| b.clipboard_not_restored())
        }
    }
}

#[cfg(not(target_os = "windows"))]
struct PlatformBackend {
    unrestored: UnrestoredClipboard,
}
#[cfg(not(target_os = "windows"))]
impl PlatformBackend {
    fn new() -> Self {
        Self {
            unrestored: UnrestoredClipboard::default(),
        }
    }
    fn take_clipboard_not_restored(&self) -> bool {
        self.unrestored.take()
    }
}
#[cfg(not(target_os = "windows"))]
impl Backend for PlatformBackend {
    type Clipboard = ();
    fn capture_target(&self) -> Result<TargetWindow, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }
    fn validate_target(&self, _: &TargetWindow) -> Result<(), InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }
    fn ime_composition_active(&self, _: &TargetWindow) -> Result<Option<bool>, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }
    fn target_text(&self, _: &TargetWindow) -> Result<TargetText, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }
    fn focus_identity(&self, _: &TargetWindow) -> Result<Vec<i32>, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }
    fn select_recent(
        &self,
        _: &TargetWindow,
        _: &TargetText,
        _: &str,
    ) -> Result<bool, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }
    fn clipboard_write(
        &self,
        _: &str,
        _: ClipboardExclusion,
        _: bool,
    ) -> Result<ClipboardWrite<Self::Clipboard>, WriteError<Self::Clipboard>> {
        Err(WriteError::Unowned(InjectionError::UnsupportedPlatform))
    }
    fn clipboard_sequence(&self) -> u32 {
        0
    }
    fn clipboard_restore(
        &self,
        _: Self::Clipboard,
        _: ClipboardStamp,
    ) -> Result<bool, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }
    fn paste(&self, _: &TargetWindow, _: SafetyPolicy, _: u32) -> Result<bool, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }
    fn delete_selection(&self, _: &TargetWindow) -> Result<bool, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }
    fn collapse_selection(&self, _: &TargetWindow, _: &TargetText) -> Result<bool, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }
    fn wait_for_target(&self) {}
    fn clipboard_not_restored(&self) {
        self.unrestored.record();
    }
}

#[cfg(target_os = "windows")]
use windows_backend::PlatformBackend;

#[cfg(target_os = "windows")]
mod windows_backend {
    use super::{
        accessibility, clipboard, keys, target, Backend, ClipboardExclusion, ClipboardStamp,
        ClipboardWrite, DirectDispatch, InjectionError, SafetyPolicy, TargetText, TargetWindow,
        TerminalReceipt, TextProbe, UnrestoredClipboard, WriteError, INJECTION_MARKER,
    };
    use std::mem::{size_of, zeroed};
    use std::thread;
    use std::time::Duration;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WPARAM};
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
        KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
        MAPVK_VK_TO_VSC, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_MENU, VK_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowExW, GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, GetWindowLongPtrW,
        GetWindowThreadProcessId, IsWindowUnicode, SendMessageTimeoutW, GUITHREADINFO, GWL_STYLE,
        SMTO_ABORTIFHUNG, SMTO_BLOCK, SMTO_ERRORONEXIT,
    };
    const ES_PASSWORD: isize = 0x20;
    pub(super) struct PlatformBackend {
        unrestored: UnrestoredClipboard,
    }
    impl PlatformBackend {
        pub(super) fn new() -> Self {
            Self {
                unrestored: UnrestoredClipboard::default(),
            }
        }
        pub(super) fn take_clipboard_not_restored(&self) -> bool {
            self.unrestored.take()
        }
    }

    fn focused_control(window: HWND, thread_id: u32) -> HWND {
        let mut info = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..unsafe { zeroed() }
        };
        if unsafe { GetGUIThreadInfo(thread_id, &mut info) }.is_ok() && !info.hwndFocus.0.is_null()
        {
            info.hwndFocus
        } else {
            window
        }
    }

    fn is_password_style(control: HWND) -> bool {
        let style = unsafe { GetWindowLongPtrW(control, GWL_STYLE) };
        style & ES_PASSWORD != 0
    }

    fn control_class(control: HWND) -> Option<String> {
        let mut class_name = [0u16; 64];
        let length = unsafe { GetClassNameW(control, &mut class_name) };
        if length == 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&class_name[..length as usize]))
    }

    fn is_known_credential_surface(control: HWND, hosted_app: bool) -> bool {
        control_class(control)
            .is_some_and(|class_name| target::is_credential_surface(&class_name, hosted_app))
    }

    /// Direct CoreWindow children of a frame, with their owners. A frame hosts
    /// one app; the bound only guards against a misbehaving window tree.
    fn core_windows(frame: HWND) -> Vec<target::ChildWindow> {
        let mut children = Vec::new();
        let mut after = HWND::default();
        for _ in 0..8 {
            let Ok(child) = (unsafe {
                FindWindowExW(
                    frame,
                    after,
                    w!("Windows.UI.Core.CoreWindow"),
                    PCWSTR::null(),
                )
            }) else {
                break;
            };
            let mut process_id = 0;
            let thread_id = unsafe { GetWindowThreadProcessId(child, Some(&mut process_id)) };
            children.push(target::ChildWindow {
                class_name: target::CORE_WINDOW_CLASS.into(),
                process_id,
                thread_id,
            });
            after = child;
        }
        children
    }

    fn uia_is_password(process_id: u32) -> Result<bool, ()> {
        accessibility::is_password(process_id).map_err(|_| ())
    }

    fn uia_ime_composition_active(process_id: u32) -> Result<Option<bool>, ()> {
        accessibility::ime_active(process_id).map_err(|_| ())
    }

    fn token_integrity_level(process: HANDLE) -> Result<u32, ()> {
        let mut token = HANDLE::default();
        unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.map_err(|_| ())?;
        let result = (|| {
            let mut size = 0;
            let _ = unsafe { GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut size) };
            if size == 0 {
                return Err(());
            }
            // usize-backed so the buffer meets TOKEN_MANDATORY_LABEL's
            // pointer alignment; a byte Vec does not guarantee it.
            let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
            if buffer.len() * size_of::<usize>() < size_of::<TOKEN_MANDATORY_LABEL>() {
                return Err(());
            }
            unsafe {
                GetTokenInformation(
                    token,
                    TokenIntegrityLevel,
                    Some(buffer.as_mut_ptr().cast()),
                    size,
                    &mut size,
                )
            }
            .map_err(|_| ())?;
            let label = unsafe { &*(buffer.as_ptr() as *const TOKEN_MANDATORY_LABEL) };
            let count = unsafe { *GetSidSubAuthorityCount(label.Label.Sid) } as u32;
            if count == 0 {
                return Err(());
            }
            let level = unsafe { *GetSidSubAuthority(label.Label.Sid, count - 1) };
            Ok(level)
        })();
        unsafe {
            let _ = CloseHandle(token);
        }
        result
    }

    fn target_process(process_id: u32) -> Result<HANDLE, InjectionError> {
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) };
        let Ok(process) = process else {
            return Err(InjectionError::PrivilegeMismatch);
        };
        Ok(process)
    }

    fn reject_higher_integrity(process: HANDLE) -> Result<(), InjectionError> {
        let current = token_integrity_level(unsafe { GetCurrentProcess() })
            .map_err(|_| InjectionError::PrivilegeMismatch)?;
        let target =
            token_integrity_level(process).map_err(|_| InjectionError::PrivilegeMismatch)?;
        if target > current {
            return Err(InjectionError::PrivilegeMismatch);
        }
        Ok(())
    }

    impl Backend for PlatformBackend {
        type Clipboard = clipboard::Snapshot;
        fn is_terminal(&self, target: &TargetWindow) -> bool {
            let window = HWND(target.window_handle as *mut _);
            control_class(window).is_some_and(|class| {
                [
                    "CASCADIA_HOSTING_WINDOW_CLASS",
                    "ConsoleWindowClass",
                    "VirtualConsoleClass",
                ]
                .iter()
                .any(|known| class.eq_ignore_ascii_case(known))
            }) || crate::app_context::from_target(target).is_some_and(|context| {
                context.category == "terminal"
                    || matches!(
                        context.app_key.as_deref(),
                        Some(
                            "conhost" | "openconsole" | "mintty" | "conemu" | "conemu64" | "kitty"
                        )
                    )
            })
        }
        fn capture_target(&self) -> Result<TargetWindow, InjectionError> {
            let window = unsafe { GetForegroundWindow() };
            if window.0.is_null() {
                return Err(InjectionError::NoForegroundWindow);
            }

            let mut process_id = 0;
            let thread_id = unsafe { GetWindowThreadProcessId(window, Some(&mut process_id)) };
            if thread_id == 0 || process_id == 0 {
                return Err(InjectionError::NoForegroundWindow);
            }
            // UI Automation reports the hosted app's process for a UWP field,
            // never the frame host's, so every check uses the hosted app.
            let hosted = target::is_frame_host(control_class(window).as_deref())
                .then(|| target::hosted_app(process_id, &core_windows(window)))
                .flatten();
            let (process_id, thread_id) = hosted.unwrap_or((process_id, thread_id));
            let process = target_process(process_id)?;
            let integrity_result = reject_higher_integrity(process);
            unsafe {
                let _ = CloseHandle(process);
            }
            integrity_result?;

            let control = focused_control(window, thread_id);
            let is_secure = target::secure_after_password_query(
                is_password_style(control),
                is_known_credential_surface(control, hosted.is_some()),
                uia_is_password(process_id),
            );
            Ok(TargetWindow {
                window_handle: window.0 as isize,
                control_handle: control.0 as isize,
                process_id,
                thread_id,
                is_secure,
            })
        }

        fn validate_target(&self, target: &TargetWindow) -> Result<(), InjectionError> {
            if target.is_secure {
                return Err(InjectionError::SecureTarget);
            }
            let current = self.capture_target()?;
            if current.window_handle != target.window_handle
                || current.control_handle != target.control_handle
                || current.process_id != target.process_id
                || current.thread_id != target.thread_id
            {
                return Err(InjectionError::TargetChanged);
            }
            if current.is_secure {
                return Err(InjectionError::SecureTarget);
            }
            Ok(())
        }

        fn ime_composition_active(
            &self,
            target: &TargetWindow,
        ) -> Result<Option<bool>, InjectionError> {
            uia_ime_composition_active(target.process_id)
                .map_err(|_| InjectionError::BackendFailure("failed to query IME composition"))
        }
        fn target_text(&self, target: &TargetWindow) -> Result<TargetText, InjectionError> {
            self.validate_target(target)?;
            accessibility::read(target)
        }
        fn target_text_probe(
            &self,
            target: &TargetWindow,
        ) -> Result<Option<TextProbe>, InjectionError> {
            self.validate_target(target)?;
            accessibility::probe(target).map(Some)
        }
        fn selection_is_empty(&self, target: &TargetWindow) -> Result<bool, InjectionError> {
            self.validate_target(target)?;
            accessibility::selection_is_empty(target)
        }
        fn focus_identity(&self, target: &TargetWindow) -> Result<Vec<i32>, InjectionError> {
            self.validate_target(target)?;
            accessibility::focused_identity(target)
        }
        fn select_recent(
            &self,
            target: &TargetWindow,
            expected: &TargetText,
            text: &str,
        ) -> Result<bool, InjectionError> {
            self.validate_target(target)?;
            if self.ime_composition_active(target)? != Some(false) || !modifiers_released() {
                return Ok(false);
            }
            accessibility::select_recent(target, expected, text)
        }
        fn clipboard_write(
            &self,
            text: &str,
            exclusion: ClipboardExclusion,
            preserve: bool,
        ) -> Result<ClipboardWrite<Self::Clipboard>, WriteError<Self::Clipboard>> {
            clipboard::write(text, exclusion, preserve)
        }
        fn clipboard_sequence(&self) -> u32 {
            clipboard::sequence()
        }
        fn clipboard_write_terminal(
            &self,
            text: &str,
            target: &TargetWindow,
        ) -> Result<ClipboardWrite<Self::Clipboard>, WriteError<Self::Clipboard>> {
            clipboard::write_terminal(text, target)
        }
        fn clipboard_restore(
            &self,
            snapshot: Self::Clipboard,
            expected: ClipboardStamp,
        ) -> Result<bool, InjectionError> {
            clipboard::restore_owned(snapshot, expected)
        }
        fn try_direct_insert(
            &self,
            target: &TargetWindow,
            before: &TargetText,
            text: &str,
            still_allowed: &dyn Fn() -> bool,
        ) -> DirectDispatch {
            if crate::environment::value("ASR_UNICODE_EDIT_INSERT").as_deref() == Some("1") {
                let result = self.try_unicode_edit(target, before, text, still_allowed);
                if result != DirectDispatch::Declined {
                    return result;
                }
            }
            // Restrict this opt-in transport to classic Unicode EDIT fields
            // and a single-line, additive caret insertion. Rich editors,
            // selection replacements and multiline conversion use paste.
            if crate::environment::value("ASR_NATIVE_EDIT_INSERT").as_deref() != Some("1")
                || target.process_id == std::process::id()
                || target.control_handle == target.window_handle
                || !before.selected.is_empty()
                || text.is_empty()
                || text.contains(['\0', '\r', '\n'])
                || before.before.contains('\n')
                || before.after.contains('\n')
            {
                return DirectDispatch::Declined;
            }
            let control = HWND(target.control_handle as *mut _);
            const ES_READONLY: isize = 0x800;
            if !control_class(control).is_some_and(|class| class.eq_ignore_ascii_case("Edit"))
                || !unsafe { IsWindowUnicode(control) }.as_bool()
                || unsafe { GetWindowLongPtrW(control, GWL_STYLE) } & (ES_PASSWORD | ES_READONLY)
                    != 0
                || self.ime_composition_active(target).ok().flatten() != Some(false)
                || self.validate_target(target).is_err()
            {
                return DirectDispatch::Declined;
            }

            const EM_GETSEL: u32 = 0x00b0;
            const EM_REPLACESEL: u32 = 0x00c2;
            const MESSAGE_TIMEOUT_MS: u32 = 150;
            let flags = SMTO_ABORTIFHUNG | SMTO_BLOCK | SMTO_ERRORONEXIT;
            let Some(expected_caret) = u32::try_from(before.before.encode_utf16().count()).ok()
            else {
                return DirectDispatch::Declined;
            };
            let payload: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let (mut start, mut end) = (u32::MAX, u32::MAX);
            // Both messages are below WM_USER. Windows marshals their
            // buffers for the target process. Keep pointers valid during send.
            let read = unsafe {
                SendMessageTimeoutW(
                    control,
                    EM_GETSEL,
                    WPARAM((&mut start as *mut u32) as usize),
                    LPARAM((&mut end as *mut u32) as isize),
                    flags,
                    MESSAGE_TIMEOUT_MS,
                    None,
                )
            };
            let foreground = unsafe { GetForegroundWindow() };
            if read.0 == 0
                || start != expected_caret
                || end != expected_caret
                || foreground.0 as isize != target.window_handle
                || focused_control(foreground, target.thread_id) != control
                || !modifiers_released()
                || !still_allowed()
            {
                return DirectDispatch::Declined;
            }
            let sent = unsafe {
                SendMessageTimeoutW(
                    control,
                    EM_REPLACESEL,
                    WPARAM(1), // Preserve the ordinary edit control's Undo.
                    LPARAM(payload.as_ptr() as isize),
                    flags,
                    MESSAGE_TIMEOUT_MS,
                    None,
                )
            };
            if sent.0 == 0 {
                DirectDispatch::Ambiguous
            } else {
                DirectDispatch::Confirmed
            }
        }
        fn paste(
            &self,
            target: &TargetWindow,
            policy: SafetyPolicy,
            sequence: u32,
        ) -> Result<bool, InjectionError> {
            self.paste_with_receipt(target, policy, sequence, None)
        }
        fn paste_with_receipt(
            &self,
            target: &TargetWindow,
            policy: SafetyPolicy,
            sequence: u32,
            receipt: Option<&dyn TerminalReceipt>,
        ) -> Result<bool, InjectionError> {
            self.validate_target(target)?;
            if !policy.permits_ime(self.ime_composition_active(target)?) || !modifiers_released() {
                return Ok(false);
            }
            if !policy.permits_selection(self, target) {
                return Ok(false);
            }
            // Ctrl+V, or Shift+Insert for the few apps that read Ctrl+V as a
            // control character. A failed process lookup keeps Ctrl+V.
            let (modifier, key) =
                keys::paste_chord(crate::app_context::from_target(target).as_ref()).keys();
            let (modifier, key) = (VIRTUAL_KEY(modifier), VIRTUAL_KEY(key));
            let inputs = [
                keyboard_input(modifier, false),
                keyboard_input(key, false),
                keyboard_input(key, true),
                keyboard_input(modifier, true),
            ];
            self.validate_target(target)?;
            // The selection check above is a UIA round trip; a modifier
            // pressed meanwhile would turn Ctrl+V into another chord
            // (e.g. Ctrl+Alt+V), so re-check right before sending.
            // Another reader can materialize the payload and advance its
            // sequence without replacing the delayed clipboard owner.
            let current = match receipt {
                Some(receipt) => receipt.stamp().is_some(),
                None => clipboard::sequence() == sequence,
            };
            if !modifiers_released() || !current {
                return Ok(false);
            }
            if let Some(receipt) = receipt {
                receipt.arm();
            }
            let sent = keys::chord_sent(unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) });
            if let Some(release_from) = sent.release_from {
                // Release only our chord's keys, never replay the paste.
                let _ = unsafe { SendInput(&inputs[release_from..], size_of::<INPUT>() as i32) };
            }
            Ok(sent.paste_queued)
        }
        fn delete_selection(&self, target: &TargetWindow) -> Result<bool, InjectionError> {
            self.validate_target(target)?;
            if self.ime_composition_active(target)? != Some(false) || !modifiers_released() {
                return Ok(false);
            }
            let inputs = [
                keyboard_input(VK_BACK, false),
                keyboard_input(VK_BACK, true),
            ];
            let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
            if sent > 0 && sent < inputs.len() as u32 {
                // Release the key only; the deletion may already be queued.
                let _ = unsafe { SendInput(&inputs[1..], size_of::<INPUT>() as i32) };
            }
            Ok(sent > 0)
        }
        fn collapse_selection(
            &self,
            target: &TargetWindow,
            expected: &TargetText,
        ) -> Result<bool, InjectionError> {
            self.validate_target(target)?;
            // Moving the caret could commit or cancel an open conversion, so
            // the IME must be known to be inactive.
            if self.ime_composition_active(target)? != Some(false) {
                return Ok(false);
            }
            accessibility::collapse_selection(target, expected)
        }
        fn wait_for_target(&self) {
            thread::sleep(Duration::from_millis(20));
        }
        fn clipboard_not_restored(&self) {
            self.unrestored.record();
        }
    }

    /// A marked key event that carries the key's scan code, as a physical
    /// keyboard's would, for targets that read the scan code instead of the
    /// virtual key. The event is still delivered by virtual key.
    fn keyboard_input(vk: VIRTUAL_KEY, key_up: bool) -> INPUT {
        let scan = unsafe { MapVirtualKeyW(u32::from(vk.0), MAPVK_VK_TO_VSC) };
        let mut flags = KEYBD_EVENT_FLAGS(0);
        if key_up {
            flags |= KEYEVENTF_KEYUP;
        }
        if keys::is_extended_key(vk.0) {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    // A scan code fits in the low byte; MAPVK_VK_TO_VSC
                    // returns 0 for a key without one.
                    wScan: (scan & 0xFF) as u16,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: INJECTION_MARKER,
                },
            },
        }
    }

    impl PlatformBackend {
        fn try_unicode_edit(
            &self,
            target: &TargetWindow,
            before: &TargetText,
            text: &str,
            still_allowed: &dyn Fn() -> bool,
        ) -> DirectDispatch {
            const MAX_UTF16_UNITS: usize = 256;
            if !before.selected.is_empty()
                || text.is_empty()
                || text.chars().any(char::is_control)
                || text.encode_utf16().take(MAX_UTF16_UNITS + 1).count() > MAX_UTF16_UNITS
            {
                return DirectDispatch::Declined;
            }
            let recognized_app = crate::app_context::from_target(target).is_some_and(|context| {
                matches!(
                    context.app_key.as_deref(),
                    Some(
                        "chrome"
                            | "msedge"
                            | "brave"
                            | "firefox"
                            | "opera"
                            | "code"
                            | "slack"
                            | "discord"
                    )
                )
            });
            if !recognized_app
                || self.validate_target(target).is_err()
                || self.ime_composition_active(target).ok().flatten() != Some(false)
                || !accessibility::writable_edit_is_current(target, before)
            {
                return DirectDispatch::Declined;
            }
            let inputs: Vec<INPUT> = text
                .encode_utf16()
                .flat_map(|unit| [unicode_input(unit, false), unicode_input(unit, true)])
                .collect();
            let foreground = unsafe { GetForegroundWindow() };
            if foreground.0 as isize != target.window_handle
                || focused_control(foreground, target.thread_id).0 as isize != target.control_handle
                || !modifiers_released()
                || !still_allowed()
            {
                return DirectDispatch::Declined;
            }
            let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) } as usize;
            // SendInput may enqueue a prefix. Release only its last key and
            // leave readback to the caller. Never send the omitted text again.
            if !sent.is_multiple_of(2) && sent < inputs.len() {
                let _ = unsafe { SendInput(&inputs[sent..sent + 1], size_of::<INPUT>() as i32) };
            }
            DirectDispatch::from_queued_inputs(sent, inputs.len())
        }
    }

    fn unicode_input(unit: u16, key_up: bool) -> INPUT {
        let mut flags = KEYEVENTF_UNICODE;
        if key_up {
            flags |= KEYEVENTF_KEYUP;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: unit,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: INJECTION_MARKER,
                },
            },
        }
    }
    fn modifiers_released() -> bool {
        use windows::Win32::UI::Input::KeyboardAndMouse::{VK_LWIN, VK_RWIN};
        [VK_SHIFT, VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN]
            .into_iter()
            .all(|key| unsafe { GetAsyncKeyState(key.0 as i32) } & i16::MIN == 0)
    }
}

//! Safe, best-effort text insertion into the foreground application.
//!
//! This module deliberately never reads or records window titles. Callers must
//! also avoid logging text passed to [`TextInjector::insert`].

use std::fmt;

use unicode_segmentation::UnicodeSegmentation;

use crate::input_monitor::InputMonitor;

/// Marks every SendInput event created by this application. The monitor helper
/// ignores only this marker; injected input from other software is treated as
/// external activity and stops provisional replacement.
pub(crate) const INJECTION_MARKER: usize = 0x4C56_494A;

/// Identity of the foreground window and focused control captured before ASR.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetWindow {
    pub window_handle: isize,
    pub control_handle: isize,
    pub process_id: u32,
    pub thread_id: u32,
    pub is_secure: bool,
}

/// The method that ultimately handled the generated text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertResult {
    UiAutomation,
    ClipboardPaste,
    UnicodeInput,
    /// Automatic insertion failed or was unsafe; the text remains available
    /// for an explicit user paste.
    ClipboardOnly,
}

/// State for a transcript already inserted into the captured target while the
/// correction API is still generating its replacement.
pub(crate) struct StreamingInsertion {
    target: TargetWindow,
    displayed: String,
    checkpoint: u64,
    active: bool,
    started: bool,
    initial_result: InsertResult,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InjectionError {
    UnsupportedPlatform,
    NoForegroundWindow,
    TargetChanged,
    SecureTarget,
    PrivilegeMismatch,
    ImeCompositionActive,
    ClipboardUnavailable,
    BackendFailure(&'static str),
}

impl fmt::Display for InjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::UnsupportedPlatform => "text injection is unsupported on this platform",
            Self::NoForegroundWindow => "no foreground target is available",
            Self::TargetChanged => "the foreground target changed before insertion",
            Self::SecureTarget => "text insertion into secure controls is disabled",
            Self::PrivilegeMismatch => "the target cannot be accessed at this privilege level",
            Self::ImeCompositionActive => "an IME composition is in progress",
            Self::ClipboardUnavailable => "the clipboard is unavailable",
            Self::BackendFailure(message) => message,
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for InjectionError {}

/// Platform-independent interface used by the dictation pipeline.
pub trait TextInjector: Send + Sync {
    fn capture_target(&self) -> Result<TargetWindow, InjectionError>;
    fn insert(&self, text: &str, target: &TargetWindow) -> Result<InsertResult, InjectionError>;
}

/// Controls whether a successful temporary clipboard paste restores the
/// clipboard value captured immediately before insertion.
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
    backend: PlatformBackend,
    options: InjectionOptions,
}

impl SystemTextInjector {
    pub fn new(options: InjectionOptions) -> Self {
        Self {
            backend: PlatformBackend::new(),
            options,
        }
    }

    /// Inserts the ASR draft only after the out-of-process input monitor is
    /// ready. Returning `None` means the caller must keep using the ordinary
    /// final-only insertion path.
    pub(crate) fn begin_streaming(
        &self,
        draft: &str,
        target: &TargetWindow,
        monitor: &InputMonitor,
    ) -> Result<Option<StreamingInsertion>, InjectionError> {
        begin_streaming_with(&self.backend, self.options, draft, target, monitor)
    }

    /// Replaces the draft on the first delta and appends subsequent deltas.
    /// Any external activity permanently abandons in-place mutation.
    pub(crate) fn push_stream_delta(
        &self,
        session: &mut StreamingInsertion,
        delta: &str,
        monitor: &InputMonitor,
    ) -> bool {
        push_stream_delta_with(&self.backend, session, delta, monitor)
    }

    /// Makes the target exactly match the provider's completed text when it is
    /// still safe. Otherwise, leaves all target text untouched and copies the
    /// completed result for an explicit user paste.
    pub(crate) fn finish_streaming(
        &self,
        session: &mut StreamingInsertion,
        final_text: &str,
        monitor: &InputMonitor,
    ) -> Result<InsertResult, InjectionError> {
        finish_streaming_with(&self.backend, session, final_text, monitor)
    }

    /// Removes the provisional text only when the target and activity
    /// checkpoint still prove that doing so cannot consume user input.
    pub(crate) fn cancel_streaming(
        &self,
        session: &mut StreamingInsertion,
        monitor: &InputMonitor,
    ) {
        if streaming_target_is_safe(&self.backend, session, monitor) {
            let _ = self.backend.replace_recent(
                grapheme_count(&session.displayed),
                "",
                &session.target,
            );
        }
        session.active = false;
    }
}

fn grapheme_count(text: &str) -> usize {
    UnicodeSegmentation::graphemes(text, true).count()
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

    fn insert(&self, text: &str, target: &TargetWindow) -> Result<InsertResult, InjectionError> {
        orchestrate_insert(&self.backend, self.options, text, target)
    }
}

trait Backend {
    fn capture_target(&self) -> Result<TargetWindow, InjectionError>;
    fn validate_target(&self, target: &TargetWindow) -> Result<(), InjectionError>;
    /// Reports whether the focused control currently holds an unconfirmed IME
    /// composition. Inserting text in that state would destroy the composition,
    /// so callers fall back to leaving the text on the clipboard.
    fn ime_composition_active(&self, target: &TargetWindow) -> Result<bool, InjectionError>;
    fn clipboard_snapshot(&self) -> Result<ClipboardSnapshot, InjectionError>;
    fn clipboard_write(
        &self,
        text: &str,
        exclusion: ClipboardExclusion,
    ) -> Result<u32, InjectionError>;
    fn clipboard_restore(
        &self,
        snapshot: &ClipboardSnapshot,
        expected_sequence: u32,
    ) -> Result<bool, InjectionError>;
    fn wait_for_paste(&self);
    fn ui_automation_insert(
        &self,
        text: &str,
        target: &TargetWindow,
    ) -> Result<bool, InjectionError>;
    fn paste(&self, target: &TargetWindow) -> Result<bool, InjectionError>;
    fn unicode_input(&self, text: &str, target: &TargetWindow) -> Result<bool, InjectionError>;
    fn replace_recent(
        &self,
        graphemes: usize,
        text: &str,
        target: &TargetWindow,
    ) -> Result<bool, InjectionError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ClipboardSnapshot {
    Empty,
    Text(String),
    NotSafelyRestorable,
}

/// Whether a clipboard write must opt out of clipboard history and cloud
/// clipboard synchronization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClipboardExclusion {
    /// Transient write that only exists to feed an immediate paste. The text
    /// must never reach clipboard history (Win+V) or the cloud clipboard.
    ExcludeFromHistory,
    /// Final fallback that intentionally leaves the text for the user to paste
    /// manually; excluding it from history would surprise the user.
    AllowHistory,
}

/// Leaves the generated text on the clipboard for an explicit user paste.
///
/// This is the "clipboard only" terminal fallback: the user is expected to
/// paste the text themselves, so it intentionally remains eligible for
/// clipboard history. Excluding it would make Win+V silently drop text the
/// user asked to keep.
fn clipboard_only_fallback<B: Backend>(
    backend: &B,
    text: &str,
    error: InjectionError,
) -> Result<InsertResult, InjectionError> {
    match backend.clipboard_write(text, ClipboardExclusion::AllowHistory) {
        Ok(_) => Ok(InsertResult::ClipboardOnly),
        Err(_) => Err(error),
    }
}

fn orchestrate_insert<B: Backend>(
    backend: &B,
    options: InjectionOptions,
    text: &str,
    target: &TargetWindow,
) -> Result<InsertResult, InjectionError> {
    if let Err(validation_error) = backend.validate_target(target) {
        return clipboard_only_fallback(backend, text, validation_error);
    }

    // IME guard: inserting while an unconfirmed composition is active would
    // destroy the in-progress conversion. Leave the text on the clipboard
    // instead so the user can paste once the composition is committed.
    if backend.ime_composition_active(target).unwrap_or(false) {
        return clipboard_only_fallback(backend, text, InjectionError::ImeCompositionActive);
    }

    if backend.ui_automation_insert(text, target).unwrap_or(false) {
        return Ok(InsertResult::UiAutomation);
    }

    let previous_clipboard = backend
        .clipboard_snapshot()
        .unwrap_or(ClipboardSnapshot::NotSafelyRestorable);
    if previous_clipboard == ClipboardSnapshot::NotSafelyRestorable {
        if backend.unicode_input(text, target).unwrap_or(false) {
            return Ok(InsertResult::UnicodeInput);
        }
        return clipboard_only_fallback(backend, text, InjectionError::ClipboardUnavailable);
    }

    // Transient write that exists only to feed the paste below. It must never
    // reach clipboard history or the cloud clipboard.
    let generated_sequence =
        backend.clipboard_write(text, ClipboardExclusion::ExcludeFromHistory)?;

    if backend.paste(target).unwrap_or(false) {
        if options.restore_clipboard {
            backend.wait_for_paste();
            // Paste has already succeeded. Restoration failure is not an
            // insertion failure and must not trigger duplicate input.
            let _ = backend.clipboard_restore(&previous_clipboard, generated_sequence);
        }
        return Ok(InsertResult::ClipboardPaste);
    }

    if backend.unicode_input(text, target).unwrap_or(false) {
        if options.restore_clipboard {
            let _ = backend.clipboard_restore(&previous_clipboard, generated_sequence);
        }
        return Ok(InsertResult::UnicodeInput);
    }

    // Every automatic path failed. The text currently on the clipboard was
    // written for the paste attempt and is excluded from history; re-write it
    // as a deliberate "clipboard only" result the user can paste manually.
    clipboard_only_fallback(backend, text, InjectionError::ClipboardUnavailable)
}

fn begin_streaming_with<B: Backend>(
    backend: &B,
    options: InjectionOptions,
    draft: &str,
    target: &TargetWindow,
    monitor: &InputMonitor,
) -> Result<Option<StreamingInsertion>, InjectionError> {
    if !monitor.start() {
        return Ok(None);
    }
    let Some(checkpoint) = monitor.checkpoint() else {
        return Ok(None);
    };
    let result = orchestrate_insert(backend, options, draft, target)?;
    if result == InsertResult::ClipboardOnly {
        return Ok(None);
    }
    if !monitor.unchanged_since(checkpoint) {
        return Ok(Some(StreamingInsertion {
            target: target.clone(),
            displayed: draft.to_owned(),
            checkpoint,
            active: false,
            started: false,
            initial_result: result,
        }));
    }
    Ok(Some(StreamingInsertion {
        target: target.clone(),
        displayed: draft.to_owned(),
        checkpoint,
        active: true,
        started: false,
        initial_result: result,
    }))
}

fn streaming_target_is_safe<B: Backend>(
    backend: &B,
    session: &StreamingInsertion,
    monitor: &InputMonitor,
) -> bool {
    session.active
        && monitor.unchanged_since(session.checkpoint)
        && backend.validate_target(&session.target).is_ok()
        && !backend
            .ime_composition_active(&session.target)
            .unwrap_or(true)
}

fn push_stream_delta_with<B: Backend>(
    backend: &B,
    session: &mut StreamingInsertion,
    delta: &str,
    monitor: &InputMonitor,
) -> bool {
    if delta.is_empty() {
        return session.active;
    }
    if !streaming_target_is_safe(backend, session, monitor) {
        session.active = false;
        return false;
    }
    let result = if !session.started {
        backend.replace_recent(grapheme_count(&session.displayed), delta, &session.target)
    } else {
        backend.unicode_input(delta, &session.target)
    };
    match result {
        Ok(true) => {
            if !session.started {
                session.displayed.clear();
            }
            session.displayed.push_str(delta);
            session.started = true;
            true
        }
        _ => {
            session.active = false;
            false
        }
    }
}

fn finish_streaming_with<B: Backend>(
    backend: &B,
    session: &mut StreamingInsertion,
    final_text: &str,
    monitor: &InputMonitor,
) -> Result<InsertResult, InjectionError> {
    if session.active
        && session.displayed == final_text
        && streaming_target_is_safe(backend, session, monitor)
    {
        return Ok(session.initial_result);
    }
    if session.active && streaming_target_is_safe(backend, session, monitor) {
        if backend
            .replace_recent(
                grapheme_count(&session.displayed),
                final_text,
                &session.target,
            )
            .unwrap_or(false)
        {
            session.displayed = final_text.to_owned();
            return Ok(session.initial_result);
        }
    }
    session.active = false;
    backend.clipboard_write(final_text, ClipboardExclusion::AllowHistory)?;
    Ok(InsertResult::ClipboardOnly)
}

#[cfg(not(target_os = "windows"))]
struct PlatformBackend;

#[cfg(not(target_os = "windows"))]
impl PlatformBackend {
    fn new() -> Self {
        Self
    }
}

#[cfg(not(target_os = "windows"))]
impl Backend for PlatformBackend {
    fn capture_target(&self) -> Result<TargetWindow, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }

    fn validate_target(&self, _target: &TargetWindow) -> Result<(), InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }

    fn ime_composition_active(&self, _target: &TargetWindow) -> Result<bool, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }

    fn clipboard_snapshot(&self) -> Result<ClipboardSnapshot, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }

    fn clipboard_write(
        &self,
        _text: &str,
        _exclusion: ClipboardExclusion,
    ) -> Result<u32, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }

    fn clipboard_restore(
        &self,
        _snapshot: &ClipboardSnapshot,
        _expected_sequence: u32,
    ) -> Result<bool, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }

    fn wait_for_paste(&self) {}

    fn ui_automation_insert(
        &self,
        _text: &str,
        _target: &TargetWindow,
    ) -> Result<bool, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }

    fn paste(&self, _target: &TargetWindow) -> Result<bool, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }

    fn unicode_input(&self, _text: &str, _target: &TargetWindow) -> Result<bool, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }

    fn replace_recent(
        &self,
        _graphemes: usize,
        _text: &str,
        _target: &TargetWindow,
    ) -> Result<bool, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }
}

#[cfg(target_os = "windows")]
mod windows_backend {
    use super::{
        Backend, ClipboardExclusion, ClipboardSnapshot, InjectionError, TargetWindow,
        INJECTION_MARKER,
    };
    use std::mem::{size_of, zeroed};
    use std::ptr;
    use std::sync::OnceLock;
    use std::thread;
    use std::time::Duration;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, GlobalFree, HANDLE, HGLOBAL, HWND};
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
        GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
        RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};
    use windows::Win32::UI::Input::Ime::{
        ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext, GCS_COMPSTR,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
        KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_LEFT, VK_MENU, VK_SHIFT, VK_V,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, GetWindowLongPtrW,
        GetWindowThreadProcessId, GUITHREADINFO, GWL_STYLE,
    };

    const CF_UNICODETEXT: u32 = 13;
    const ES_PASSWORD: isize = 0x20;
    const CLIPBOARD_RETRIES: usize = 10;
    const PASTE_SETTLE_DELAY: Duration = Duration::from_millis(150);

    /// Registers a clipboard format by name, caching the atom for the process
    /// lifetime. Returns `None` if registration fails so callers can degrade to
    /// a best-effort write without the exclusion marker.
    fn registered_format(name: &str, cache: &OnceLock<Option<u32>>) -> Option<u32> {
        *cache.get_or_init(|| {
            let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            let format = unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) };
            if format == 0 {
                None
            } else {
                Some(format)
            }
        })
    }

    fn format_exclude_from_monitoring() -> Option<u32> {
        static CACHE: OnceLock<Option<u32>> = OnceLock::new();
        registered_format("ExcludeClipboardContentFromMonitorProcessing", &CACHE)
    }

    fn format_can_include_in_history() -> Option<u32> {
        static CACHE: OnceLock<Option<u32>> = OnceLock::new();
        registered_format("CanIncludeInClipboardHistory", &CACHE)
    }

    fn format_can_upload_to_cloud() -> Option<u32> {
        static CACHE: OnceLock<Option<u32>> = OnceLock::new();
        registered_format("CanUploadToCloudClipboard", &CACHE)
    }

    /// Places the clipboard-history / cloud-clipboard opt-out markers alongside
    /// the already-set text. Best effort: a failure leaves the text usable but
    /// is reported via the boolean so the caller can log (never the text).
    ///
    /// Must be called while the clipboard is open and after the text format has
    /// been set, because writing additional formats does not re-empty it.
    fn apply_history_exclusion() -> bool {
        // `ExcludeClipboardContentFromMonitorProcessing` is a marker format with
        // a zero-length payload; clipboard history and the cloud clipboard treat
        // its mere presence as an opt-out.
        let mut all_ok = true;
        if let Some(format) = format_exclude_from_monitoring() {
            all_ok &= set_empty_format(format);
        } else {
            all_ok = false;
        }
        // The two DWORD formats carry a single `0` to deny inclusion/upload.
        if let Some(format) = format_can_include_in_history() {
            all_ok &= set_dword_format(format, 0);
        } else {
            all_ok = false;
        }
        if let Some(format) = format_can_upload_to_cloud() {
            all_ok &= set_dword_format(format, 0);
        } else {
            all_ok = false;
        }
        all_ok
    }

    fn set_empty_format(format: u32) -> bool {
        let allocation = match unsafe { GlobalAlloc(GMEM_MOVEABLE, 1) } {
            Ok(allocation) => allocation,
            Err(_) => return false,
        };
        // Zero-length marker: the handle just needs to be valid and owned by the
        // clipboard. Lock/unlock to satisfy the moveable-memory contract.
        let pointer = unsafe { GlobalLock(allocation) };
        if pointer.is_null() {
            unsafe {
                let _ = GlobalFree(allocation);
            }
            return false;
        }
        unsafe {
            *(pointer as *mut u8) = 0;
            let _ = GlobalUnlock(allocation);
        }
        if unsafe { SetClipboardData(format, HANDLE(allocation.0)) }.is_err() {
            unsafe {
                let _ = GlobalFree(allocation);
            }
            return false;
        }
        true
    }

    fn set_dword_format(format: u32, value: u32) -> bool {
        let allocation = match unsafe { GlobalAlloc(GMEM_MOVEABLE, size_of::<u32>()) } {
            Ok(allocation) => allocation,
            Err(_) => return false,
        };
        let pointer = unsafe { GlobalLock(allocation) } as *mut u32;
        if pointer.is_null() {
            unsafe {
                let _ = GlobalFree(allocation);
            }
            return false;
        }
        unsafe {
            *pointer = value;
            let _ = GlobalUnlock(allocation);
        }
        if unsafe { SetClipboardData(format, HANDLE(allocation.0)) }.is_err() {
            unsafe {
                let _ = GlobalFree(allocation);
            }
            return false;
        }
        true
    }

    pub(super) struct PlatformBackend;

    impl PlatformBackend {
        pub(super) fn new() -> Self {
            Self
        }
    }

    struct ClipboardGuard;

    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }

    fn open_clipboard() -> Result<ClipboardGuard, InjectionError> {
        for _ in 0..CLIPBOARD_RETRIES {
            if unsafe { OpenClipboard(HWND::default()) }.is_ok() {
                return Ok(ClipboardGuard);
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err(InjectionError::ClipboardUnavailable)
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

    fn is_known_credential_surface(control: HWND) -> bool {
        control_class(control).is_some_and(|class_name| {
            class_name.eq_ignore_ascii_case("Credential Dialog Xaml Host")
                || class_name.eq_ignore_ascii_case("Windows.UI.Core.CoreWindow")
                || class_name.eq_ignore_ascii_case("CredentialUIBroker")
        })
    }

    struct ComGuard(bool);

    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    fn uia_is_password(process_id: u32) -> Result<bool, ()> {
        let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
        let _guard = ComGuard(initialized);
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|_| ())?;
        let element = unsafe { automation.GetFocusedElement() }.map_err(|_| ())?;
        let focused_process = unsafe { element.CurrentProcessId() }.map_err(|_| ())?;
        if focused_process != process_id as i32 {
            return Err(());
        }
        unsafe { element.CurrentIsPassword() }
            .map(|value| value.as_bool())
            .map_err(|_| ())
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
            let mut buffer = vec![0u8; size as usize];
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

    fn keyboard_input(vk: VIRTUAL_KEY, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS(flags),
                    time: 0,
                    dwExtraInfo: INJECTION_MARKER,
                },
            },
        }
    }

    fn send(inputs: &[INPUT]) -> bool {
        (unsafe { SendInput(inputs, size_of::<INPUT>() as i32) }) == inputs.len() as u32
    }

    fn append_unicode_inputs(inputs: &mut Vec<INPUT>, text: &str) {
        for unit in text.encode_utf16() {
            inputs.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: KEYEVENTF_UNICODE,
                        time: 0,
                        dwExtraInfo: INJECTION_MARKER,
                    },
                },
            });
            inputs.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                        time: 0,
                        dwExtraInfo: INJECTION_MARKER,
                    },
                },
            });
        }
    }

    fn replacement_inputs(graphemes: usize, text: &str) -> Vec<INPUT> {
        let mut inputs = Vec::with_capacity(graphemes.saturating_mul(2) + 4 + text.len() * 2);
        if graphemes > 0 {
            inputs.push(keyboard_input(VK_SHIFT, 0));
            for _ in 0..graphemes {
                inputs.push(keyboard_input(VK_LEFT, 0));
                inputs.push(keyboard_input(VK_LEFT, KEYEVENTF_KEYUP.0));
            }
            inputs.push(keyboard_input(VK_SHIFT, KEYEVENTF_KEYUP.0));
            // Some target controls append Unicode input instead of replacing
            // the active selection. Delete the draft explicitly so the
            // corrected text cannot be inserted alongside it.
            inputs.push(keyboard_input(VK_BACK, 0));
            inputs.push(keyboard_input(VK_BACK, KEYEVENTF_KEYUP.0));
        }
        append_unicode_inputs(&mut inputs, text);
        inputs
    }

    fn modifiers_released() -> bool {
        [VK_SHIFT, VK_CONTROL, VK_MENU]
            .into_iter()
            .all(|key| unsafe { GetAsyncKeyState(key.0 as i32) } & i16::MIN == 0)
    }

    impl Backend for PlatformBackend {
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
            let process = target_process(process_id)?;
            let integrity_result = reject_higher_integrity(process);
            unsafe {
                let _ = CloseHandle(process);
            }
            integrity_result?;

            let control = focused_control(window, thread_id);
            let style_password = is_password_style(control);
            let known_credential_surface = is_known_credential_surface(control);
            let uia_password = uia_is_password(process_id);
            let is_secure = style_password
                || matches!(uia_password, Ok(true))
                || known_credential_surface && uia_password.is_err();
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

        fn ime_composition_active(&self, target: &TargetWindow) -> Result<bool, InjectionError> {
            let control = HWND(target.control_handle as *mut _);
            let context = unsafe { ImmGetContext(control) };
            if context.0.is_null() {
                // No IME context is associated with the control, so there can be
                // no in-progress composition to protect.
                return Ok(false);
            }
            // Querying with a null buffer returns the byte length of the current
            // composition string; a positive length means an unconfirmed
            // composition is being edited.
            let length = unsafe { ImmGetCompositionStringW(context, GCS_COMPSTR, None, 0) };
            unsafe {
                let _ = ImmReleaseContext(control, context);
            }
            Ok(length > 0)
        }

        fn clipboard_snapshot(&self) -> Result<ClipboardSnapshot, InjectionError> {
            let _guard = open_clipboard()?;
            let format = unsafe { EnumClipboardFormats(0) };
            if format == 0 {
                return Ok(ClipboardSnapshot::Empty);
            }
            if format != CF_UNICODETEXT || unsafe { EnumClipboardFormats(format) } != 0 {
                return Ok(ClipboardSnapshot::NotSafelyRestorable);
            }
            if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) }.is_err() {
                return Ok(ClipboardSnapshot::Empty);
            }
            let handle = unsafe { GetClipboardData(CF_UNICODETEXT) }
                .map_err(|_| InjectionError::ClipboardUnavailable)?;
            let global = HGLOBAL(handle.0);
            let pointer = unsafe { GlobalLock(global) } as *const u16;
            if pointer.is_null() {
                return Err(InjectionError::ClipboardUnavailable);
            }
            let mut length = 0;
            unsafe {
                while *pointer.add(length) != 0 {
                    length += 1;
                }
            }
            let value =
                String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(pointer, length) });
            unsafe {
                let _ = GlobalUnlock(global);
            }
            Ok(ClipboardSnapshot::Text(value))
        }

        fn clipboard_write(
            &self,
            text: &str,
            exclusion: ClipboardExclusion,
        ) -> Result<u32, InjectionError> {
            let _guard = open_clipboard()?;
            unsafe { EmptyClipboard() }.map_err(|_| InjectionError::ClipboardUnavailable)?;

            let utf16: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
            let bytes = utf16.len() * size_of::<u16>();
            let allocation = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }
                .map_err(|_| InjectionError::ClipboardUnavailable)?;
            let destination = unsafe { GlobalLock(allocation) } as *mut u16;
            if destination.is_null() {
                unsafe {
                    let _ = GlobalFree(allocation);
                }
                return Err(InjectionError::ClipboardUnavailable);
            }
            unsafe {
                ptr::copy_nonoverlapping(utf16.as_ptr(), destination, utf16.len());
                let _ = GlobalUnlock(allocation);
            }
            if unsafe { SetClipboardData(CF_UNICODETEXT, HANDLE(allocation.0)) }.is_err() {
                unsafe {
                    let _ = GlobalFree(allocation);
                }
                return Err(InjectionError::ClipboardUnavailable);
            }
            if exclusion == ClipboardExclusion::ExcludeFromHistory && !apply_history_exclusion() {
                // Best effort: the transcript is on the clipboard and the paste
                // must proceed. The exclusion markers are an additional privacy
                // guard, not a correctness requirement, so we only note the
                // failure here. The text is never logged.
                #[cfg(debug_assertions)]
                eprintln!("injection: clipboard history exclusion could not be applied");
            }
            Ok(unsafe { GetClipboardSequenceNumber() })
        }

        fn clipboard_restore(
            &self,
            snapshot: &ClipboardSnapshot,
            expected_sequence: u32,
        ) -> Result<bool, InjectionError> {
            if unsafe { GetClipboardSequenceNumber() } != expected_sequence {
                return Ok(false);
            }
            match snapshot {
                ClipboardSnapshot::Text(text) => {
                    // Restoring the user's own prior clipboard content: hand it
                    // back exactly as it was, including history eligibility.
                    self.clipboard_write(text, ClipboardExclusion::AllowHistory)?;
                    Ok(true)
                }
                ClipboardSnapshot::Empty => {
                    let _guard = open_clipboard()?;
                    unsafe { EmptyClipboard() }
                        .map_err(|_| InjectionError::ClipboardUnavailable)?;
                    Ok(true)
                }
                ClipboardSnapshot::NotSafelyRestorable => Ok(false),
            }
        }

        fn wait_for_paste(&self) {
            thread::sleep(PASTE_SETTLE_DELAY);
        }

        fn ui_automation_insert(
            &self,
            _text: &str,
            _target: &TargetWindow,
        ) -> Result<bool, InjectionError> {
            // The fallback slot is intentional. ValuePattern/TextPattern support
            // varies substantially by application and must be enabled only once
            // target-specific reliability has been established.
            Ok(false)
        }

        fn paste(&self, target: &TargetWindow) -> Result<bool, InjectionError> {
            self.validate_target(target)?;
            let inputs = [
                keyboard_input(VK_CONTROL, 0),
                keyboard_input(VK_V, 0),
                keyboard_input(VK_V, KEYEVENTF_KEYUP.0),
                keyboard_input(VK_CONTROL, KEYEVENTF_KEYUP.0),
            ];
            Ok(send(&inputs))
        }

        fn unicode_input(&self, text: &str, target: &TargetWindow) -> Result<bool, InjectionError> {
            let mut inputs = Vec::with_capacity(text.encode_utf16().count() * 2);
            append_unicode_inputs(&mut inputs, text);
            self.validate_target(target)?;
            Ok(send(&inputs))
        }

        fn replace_recent(
            &self,
            graphemes: usize,
            text: &str,
            target: &TargetWindow,
        ) -> Result<bool, InjectionError> {
            self.validate_target(target)?;
            if self.ime_composition_active(target)? || !modifiers_released() {
                return Ok(false);
            }
            let inputs = replacement_inputs(graphemes, text);
            self.validate_target(target)?;
            Ok(send(&inputs))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn replacement_deletes_selected_draft_before_typing_corrected_text() {
            let inputs = replacement_inputs(1, "x");
            let events = inputs
                .iter()
                .map(|input| unsafe {
                    let keyboard = input.Anonymous.ki;
                    (keyboard.wVk, keyboard.dwFlags)
                })
                .collect::<Vec<_>>();

            assert_eq!(
                events,
                [
                    (VK_SHIFT, Default::default()),
                    (VK_LEFT, Default::default()),
                    (VK_LEFT, KEYEVENTF_KEYUP),
                    (VK_SHIFT, KEYEVENTF_KEYUP),
                    (VK_BACK, Default::default()),
                    (VK_BACK, KEYEVENTF_KEYUP),
                    (VIRTUAL_KEY(0), KEYEVENTF_UNICODE),
                    (VIRTUAL_KEY(0), KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
                ]
            );
        }
    }
}

#[cfg(target_os = "windows")]
use windows_backend::PlatformBackend;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    struct MockBackend {
        current: RefCell<TargetWindow>,
        secure: Cell<bool>,
        ime_active: Cell<bool>,
        clipboard: RefCell<Option<String>>,
        snapshot: RefCell<ClipboardSnapshot>,
        sequence: Cell<u32>,
        change_sequence_on_wait: bool,
        validation_count: Cell<u32>,
        change_focus_before_send: bool,
        calls: RefCell<Vec<&'static str>>,
        exclusions: RefCell<Vec<ClipboardExclusion>>,
        uia: bool,
        paste: bool,
        unicode: bool,
        replace: bool,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                current: RefCell::new(target(1, false)),
                secure: Cell::new(false),
                ime_active: Cell::new(false),
                clipboard: RefCell::new(Some("original".into())),
                snapshot: RefCell::new(ClipboardSnapshot::Text("original".into())),
                sequence: Cell::new(0),
                change_sequence_on_wait: false,
                validation_count: Cell::new(0),
                change_focus_before_send: false,
                calls: RefCell::new(Vec::new()),
                exclusions: RefCell::new(Vec::new()),
                uia: false,
                paste: false,
                unicode: false,
                replace: false,
            }
        }
    }

    impl Backend for MockBackend {
        fn capture_target(&self) -> Result<TargetWindow, InjectionError> {
            Ok(self.current.borrow().clone())
        }

        fn validate_target(&self, expected: &TargetWindow) -> Result<(), InjectionError> {
            self.calls.borrow_mut().push("validate");
            let validation_count = self.validation_count.get() + 1;
            self.validation_count.set(validation_count);
            if self.change_focus_before_send && validation_count == 2 {
                *self.current.borrow_mut() = target(2, false);
            }
            if self.secure.get() || expected.is_secure {
                return Err(InjectionError::SecureTarget);
            }
            if *self.current.borrow() != *expected {
                return Err(InjectionError::TargetChanged);
            }
            Ok(())
        }

        fn ime_composition_active(&self, _target: &TargetWindow) -> Result<bool, InjectionError> {
            self.calls.borrow_mut().push("ime_check");
            Ok(self.ime_active.get())
        }

        fn clipboard_snapshot(&self) -> Result<ClipboardSnapshot, InjectionError> {
            self.calls.borrow_mut().push("clipboard_snapshot");
            Ok(self.snapshot.borrow().clone())
        }

        fn clipboard_write(
            &self,
            text: &str,
            exclusion: ClipboardExclusion,
        ) -> Result<u32, InjectionError> {
            self.calls.borrow_mut().push("clipboard_write");
            self.exclusions.borrow_mut().push(exclusion);
            *self.clipboard.borrow_mut() = Some(text.to_owned());
            self.sequence.set(self.sequence.get() + 1);
            Ok(self.sequence.get())
        }

        fn clipboard_restore(
            &self,
            snapshot: &ClipboardSnapshot,
            expected_sequence: u32,
        ) -> Result<bool, InjectionError> {
            self.calls.borrow_mut().push("clipboard_restore");
            if self.sequence.get() != expected_sequence {
                return Ok(false);
            }
            match snapshot {
                ClipboardSnapshot::Text(text) => *self.clipboard.borrow_mut() = Some(text.clone()),
                ClipboardSnapshot::Empty => *self.clipboard.borrow_mut() = None,
                ClipboardSnapshot::NotSafelyRestorable => return Ok(false),
            }
            Ok(true)
        }

        fn wait_for_paste(&self) {
            self.calls.borrow_mut().push("wait_for_paste");
            if self.change_sequence_on_wait {
                self.sequence.set(self.sequence.get() + 1);
            }
        }

        fn ui_automation_insert(
            &self,
            _text: &str,
            _target: &TargetWindow,
        ) -> Result<bool, InjectionError> {
            self.calls.borrow_mut().push("uia");
            Ok(self.uia)
        }

        fn paste(&self, _target: &TargetWindow) -> Result<bool, InjectionError> {
            self.validate_target(_target)?;
            self.calls.borrow_mut().push("paste");
            Ok(self.paste)
        }

        fn unicode_input(
            &self,
            _text: &str,
            _target: &TargetWindow,
        ) -> Result<bool, InjectionError> {
            self.validate_target(_target)?;
            self.calls.borrow_mut().push("unicode");
            Ok(self.unicode)
        }

        fn replace_recent(
            &self,
            _graphemes: usize,
            _text: &str,
            target: &TargetWindow,
        ) -> Result<bool, InjectionError> {
            self.validate_target(target)?;
            self.calls.borrow_mut().push("replace");
            Ok(self.replace)
        }
    }

    fn target(handle: isize, secure: bool) -> TargetWindow {
        TargetWindow {
            window_handle: handle,
            control_handle: handle + 10,
            process_id: handle as u32,
            thread_id: handle as u32 + 100,
            is_secure: secure,
        }
    }

    #[test]
    fn fallback_order_prefers_paste_before_unicode() {
        let mut backend = MockBackend::new();
        backend.unicode = true;
        let result = orchestrate_insert(
            &backend,
            InjectionOptions {
                restore_clipboard: false,
            },
            "generated",
            &target(1, false),
        )
        .unwrap();

        assert_eq!(result, InsertResult::UnicodeInput);
        assert_eq!(
            backend.calls.borrow().as_slice(),
            [
                "validate",
                "ime_check",
                "uia",
                "clipboard_snapshot",
                "clipboard_write",
                "validate",
                "paste",
                "validate",
                "unicode"
            ]
        );
        // The transient paste write must opt out of clipboard history.
        assert_eq!(
            backend.exclusions.borrow().as_slice(),
            [ClipboardExclusion::ExcludeFromHistory]
        );
    }

    #[test]
    fn secure_target_falls_back_to_clipboard_without_insertion() {
        let backend = MockBackend::new();
        let result = orchestrate_insert(
            &backend,
            InjectionOptions::default(),
            "generated",
            &target(1, true),
        );

        assert_eq!(result, Ok(InsertResult::ClipboardOnly));
        assert_eq!(backend.clipboard.borrow().as_deref(), Some("generated"));
        assert_eq!(
            backend.calls.borrow().as_slice(),
            ["validate", "clipboard_write"]
        );
    }

    #[test]
    fn changed_target_falls_back_to_clipboard_without_insertion() {
        let backend = MockBackend::new();
        *backend.current.borrow_mut() = target(2, false);
        let result = orchestrate_insert(
            &backend,
            InjectionOptions::default(),
            "generated",
            &target(1, false),
        );

        assert_eq!(result, Ok(InsertResult::ClipboardOnly));
        assert_eq!(backend.clipboard.borrow().as_deref(), Some("generated"));
        assert_eq!(
            backend.calls.borrow().as_slice(),
            ["validate", "clipboard_write"]
        );
    }

    #[test]
    fn restores_clipboard_after_successful_paste() {
        let mut backend = MockBackend::new();
        backend.paste = true;
        let result = orchestrate_insert(
            &backend,
            InjectionOptions::default(),
            "generated",
            &target(1, false),
        )
        .unwrap();

        assert_eq!(result, InsertResult::ClipboardPaste);
        assert_eq!(backend.clipboard.borrow().as_deref(), Some("original"));
        assert!(backend.calls.borrow().contains(&"wait_for_paste"));
    }

    #[test]
    fn total_failure_leaves_generated_text_in_clipboard() {
        let backend = MockBackend::new();
        let result = orchestrate_insert(
            &backend,
            InjectionOptions::default(),
            "generated",
            &target(1, false),
        )
        .unwrap();

        assert_eq!(result, InsertResult::ClipboardOnly);
        assert_eq!(backend.clipboard.borrow().as_deref(), Some("generated"));
    }

    #[test]
    fn focus_change_immediately_before_send_leaves_clipboard_only() {
        let mut backend = MockBackend::new();
        backend.change_focus_before_send = true;
        let result = orchestrate_insert(
            &backend,
            InjectionOptions::default(),
            "generated",
            &target(1, false),
        )
        .unwrap();

        assert_eq!(result, InsertResult::ClipboardOnly);
        assert_eq!(backend.clipboard.borrow().as_deref(), Some("generated"));
        assert!(!backend.calls.borrow().contains(&"paste"));
        assert!(!backend.calls.borrow().contains(&"unicode"));
    }

    #[test]
    fn non_text_clipboard_snapshot_is_not_partially_restored() {
        let mut backend = MockBackend::new();
        backend.unicode = true;
        *backend.snapshot.borrow_mut() = ClipboardSnapshot::NotSafelyRestorable;

        let result = orchestrate_insert(
            &backend,
            InjectionOptions::default(),
            "generated",
            &target(1, false),
        )
        .unwrap();

        assert_eq!(result, InsertResult::UnicodeInput);
        assert_eq!(backend.clipboard.borrow().as_deref(), Some("original"));
        assert!(!backend.calls.borrow().contains(&"clipboard_write"));
    }

    #[test]
    fn external_clipboard_change_prevents_restoration() {
        let mut backend = MockBackend::new();
        backend.paste = true;
        backend.change_sequence_on_wait = true;

        orchestrate_insert(
            &backend,
            InjectionOptions::default(),
            "generated",
            &target(1, false),
        )
        .unwrap();

        assert_eq!(backend.clipboard.borrow().as_deref(), Some("generated"));
    }

    #[test]
    fn active_ime_composition_skips_insertion_and_keeps_clipboard() {
        let mut backend = MockBackend::new();
        backend.ime_active.set(true);
        // Even though paste/unicode would succeed, the IME guard must short
        // circuit before any insertion is attempted.
        backend.paste = true;
        backend.unicode = true;

        let result = orchestrate_insert(
            &backend,
            InjectionOptions::default(),
            "generated",
            &target(1, false),
        )
        .unwrap();

        assert_eq!(result, InsertResult::ClipboardOnly);
        assert_eq!(backend.clipboard.borrow().as_deref(), Some("generated"));
        assert_eq!(
            backend.calls.borrow().as_slice(),
            ["validate", "ime_check", "clipboard_write"]
        );
        assert!(!backend.calls.borrow().contains(&"paste"));
        assert!(!backend.calls.borrow().contains(&"unicode"));
        // The clipboard-only fallback leaves the text eligible for history so
        // the user can paste it manually.
        assert_eq!(
            backend.exclusions.borrow().as_slice(),
            [ClipboardExclusion::AllowHistory]
        );
    }

    #[test]
    fn inactive_ime_composition_allows_normal_insertion() {
        let mut backend = MockBackend::new();
        backend.ime_active.set(false);
        backend.paste = true;

        let result = orchestrate_insert(
            &backend,
            InjectionOptions::default(),
            "generated",
            &target(1, false),
        )
        .unwrap();

        assert_eq!(result, InsertResult::ClipboardPaste);
        // The IME check runs but does not block the paste path.
        assert!(backend.calls.borrow().contains(&"ime_check"));
        assert!(backend.calls.borrow().contains(&"paste"));
    }

    #[test]
    fn streaming_replaces_draft_once_then_appends_deltas() {
        let mut backend = MockBackend::new();
        backend.paste = true;
        backend.replace = true;
        backend.unicode = true;
        let monitor = InputMonitor::default();
        monitor.test_set_available(true);
        let mut session = begin_streaming_with(
            &backend,
            InjectionOptions::default(),
            "raw transcript",
            &target(1, false),
            &monitor,
        )
        .unwrap()
        .unwrap();

        assert!(push_stream_delta_with(
            &backend,
            &mut session,
            "corrected ",
            &monitor
        ));
        assert!(push_stream_delta_with(
            &backend,
            &mut session,
            "text",
            &monitor
        ));
        assert_eq!(session.displayed, "corrected text");
        assert_eq!(
            finish_streaming_with(&backend, &mut session, "corrected text", &monitor).unwrap(),
            InsertResult::ClipboardPaste
        );
        let calls = backend.calls.borrow();
        assert_eq!(calls.iter().filter(|call| **call == "replace").count(), 1);
        assert_eq!(calls.iter().filter(|call| **call == "unicode").count(), 1);
    }

    #[test]
    fn external_input_abandons_replacement_and_copies_final_text() {
        let mut backend = MockBackend::new();
        backend.paste = true;
        backend.replace = true;
        backend.unicode = true;
        let monitor = InputMonitor::default();
        monitor.test_set_available(true);
        let mut session = begin_streaming_with(
            &backend,
            InjectionOptions::default(),
            "draft",
            &target(1, false),
            &monitor,
        )
        .unwrap()
        .unwrap();
        monitor.test_record_input();

        assert!(!push_stream_delta_with(
            &backend,
            &mut session,
            "generated",
            &monitor
        ));
        assert_eq!(
            finish_streaming_with(&backend, &mut session, "generated final", &monitor).unwrap(),
            InsertResult::ClipboardOnly
        );
        assert_eq!(
            backend.clipboard.borrow().as_deref(),
            Some("generated final")
        );
        assert!(!backend.calls.borrow().contains(&"replace"));
    }

    #[test]
    fn grapheme_count_treats_combining_text_and_emoji_as_caret_units() {
        assert_eq!(grapheme_count("e\u{301}"), 1);
        assert_eq!(grapheme_count("👨‍👩‍👧‍👦"), 1);
        assert_eq!(grapheme_count("ab"), 2);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn system_backend_is_non_destructive_when_unsupported() {
        let injector = SystemTextInjector::default();
        assert_eq!(
            injector.capture_target(),
            Err(InjectionError::UnsupportedPlatform)
        );
    }
}

//! Batch text insertion with verified provisional-range replacement.
//! Target text is inspected transiently for verification, never logged.

use crate::input_monitor::InputMonitor;
use std::fmt;

#[cfg(target_os = "windows")]
mod accessibility;
mod batch;
#[cfg(target_os = "windows")]
mod clipboard;

pub(crate) const INJECTION_MARKER: usize = 0x4C56_494A;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetWindow {
    pub window_handle: isize,
    pub control_handle: isize,
    pub process_id: u32,
    pub thread_id: u32,
    pub is_secure: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertResult {
    ClipboardPaste,
    ClipboardOnly,
    /// Input was queued, but the target edit was not confirmed. Do not retry,
    /// restore the old clipboard, or overwrite a possibly pending payload.
    PasteUnverified,
}

pub(crate) struct ProvisionalInsertion {
    target: TargetWindow,
    displayed: String,
    after: TargetText,
    checkpoint: u64,
    result: InsertResult,
    finished: bool,
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
        formatter.write_str(match self {
            Self::UnsupportedPlatform => "text injection is unsupported on this platform",
            Self::NoForegroundWindow => "no foreground target is available",
            Self::TargetChanged => "the foreground target changed before insertion",
            Self::SecureTarget => "text insertion into secure controls is disabled",
            Self::PrivilegeMismatch => "the target cannot be accessed at this privilege level",
            Self::ImeCompositionActive => "an IME composition is in progress",
            Self::ClipboardUnavailable => "the clipboard is unavailable",
            Self::BackendFailure(message) => message,
        })
    }
}
impl std::error::Error for InjectionError {}

pub trait TextInjector: Send + Sync {
    fn capture_target(&self) -> Result<TargetWindow, InjectionError>;
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
    backend: PlatformBackend,
    options: InjectionOptions,
}

pub(crate) struct SelectedText {
    target: TargetWindow,
    text: String,
    state: TargetText,
}
impl SelectedText {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }
}
impl SystemTextInjector {
    pub fn new(options: InjectionOptions) -> Self {
        Self {
            backend: PlatformBackend::new(),
            options,
        }
    }
    /// No session means nothing was queued into the target. A queued but
    /// unconfirmed draft still owns a session so completion cannot paste again.
    pub(crate) fn begin_provisional(
        &self,
        draft: &str,
        target: &TargetWindow,
        monitor: &InputMonitor,
    ) -> Result<Option<ProvisionalInsertion>, InjectionError> {
        batch::begin(&self.backend, self.options, draft, target, monitor)
    }
    pub(crate) fn finish_provisional(
        &self,
        session: &mut ProvisionalInsertion,
        final_text: &str,
        monitor: &InputMonitor,
    ) -> Result<InsertResult, InjectionError> {
        batch::finish(&self.backend, self.options, session, final_text, monitor)
    }
    pub(crate) fn cancel_provisional(
        &self,
        session: &mut ProvisionalInsertion,
        monitor: &InputMonitor,
    ) {
        batch::cancel(&self.backend, session, monitor)
    }

    pub(crate) fn capture_selection(&self) -> Result<SelectedText, InjectionError> {
        let target = self.capture_target()?;
        let state = self.backend.target_text(&target)?;
        if state.selected.is_empty() {
            return Err(InjectionError::BackendFailure("no text is selected"));
        }
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

    pub(crate) fn copy_to_clipboard(&self, text: &str) -> Result<(), InjectionError> {
        self.backend
            .clipboard_write(text, ClipboardExclusion::ExcludeFromHistory)
            .map(|_| ())
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
fn normalize_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}
impl TargetText {
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
    AllowHistory,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SafetyPolicy {
    Additive,
    Destructive,
}

impl SafetyPolicy {
    fn permits_ime(self, active: Option<bool>) -> bool {
        active == Some(false) || self == Self::Additive && active.is_none()
    }
}

trait Backend {
    type Clipboard;
    fn capture_target(&self) -> Result<TargetWindow, InjectionError>;
    fn validate_target(&self, target: &TargetWindow) -> Result<(), InjectionError>;
    fn ime_composition_active(&self, target: &TargetWindow)
        -> Result<Option<bool>, InjectionError>;
    fn target_text(&self, target: &TargetWindow) -> Result<TargetText, InjectionError>;
    fn select_recent(
        &self,
        target: &TargetWindow,
        expected: &TargetText,
        text: &str,
    ) -> Result<bool, InjectionError>;
    fn clipboard_snapshot(&self) -> Result<Self::Clipboard, InjectionError>;
    fn clipboard_write(
        &self,
        text: &str,
        exclusion: ClipboardExclusion,
    ) -> Result<u32, InjectionError>;
    fn clipboard_restore(
        &self,
        snapshot: Self::Clipboard,
        expected_sequence: u32,
    ) -> Result<bool, InjectionError>;
    /// Returns true if any input was queued, including a partial SendInput.
    fn paste(&self, target: &TargetWindow, policy: SafetyPolicy) -> Result<bool, InjectionError>;
    fn delete_selection(&self, target: &TargetWindow) -> Result<bool, InjectionError>;
    fn wait_for_target(&self);
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
    fn select_recent(
        &self,
        _: &TargetWindow,
        _: &TargetText,
        _: &str,
    ) -> Result<bool, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }
    fn clipboard_snapshot(&self) -> Result<Self::Clipboard, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }
    fn clipboard_write(&self, _: &str, _: ClipboardExclusion) -> Result<u32, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }
    fn clipboard_restore(&self, _: Self::Clipboard, _: u32) -> Result<bool, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }
    fn paste(&self, _: &TargetWindow, _: SafetyPolicy) -> Result<bool, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }
    fn delete_selection(&self, _: &TargetWindow) -> Result<bool, InjectionError> {
        Err(InjectionError::UnsupportedPlatform)
    }
    fn wait_for_target(&self) {}
}

#[cfg(target_os = "windows")]
use windows_backend::PlatformBackend;

#[cfg(target_os = "windows")]
mod windows_backend {
    use super::{
        accessibility, clipboard, Backend, ClipboardExclusion, InjectionError, SafetyPolicy,
        TargetText, TargetWindow, INJECTION_MARKER,
    };
    use std::mem::{size_of, zeroed};
    use std::thread;
    use std::time::Duration;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationTextEditPattern, UIA_TextEditPatternId,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
        VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_MENU, VK_SHIFT, VK_V,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, GetWindowLongPtrW,
        GetWindowThreadProcessId, GUITHREADINFO, GWL_STYLE,
    };
    const ES_PASSWORD: isize = 0x20;
    pub(super) struct PlatformBackend;
    impl PlatformBackend {
        pub(super) fn new() -> Self {
            Self
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

    fn uia_ime_composition_active(process_id: u32) -> Result<Option<bool>, ()> {
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
        let pattern = match unsafe {
            element.GetCurrentPatternAs::<IUIAutomationTextEditPattern>(UIA_TextEditPatternId)
        } {
            Ok(pattern) => pattern,
            Err(_) => return Ok(None),
        };
        match unsafe { pattern.GetActiveComposition() } {
            Ok(_) => Ok(Some(true)),
            // The Windows binding represents the documented null range (no
            // active conversion) as an empty-interface error.
            Err(error) if error.code() == windows::core::Error::empty().code() => Ok(Some(false)),
            Err(_) => Err(()),
        }
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

    impl Backend for PlatformBackend {
        type Clipboard = clipboard::Snapshot;
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
        fn clipboard_snapshot(&self) -> Result<Self::Clipboard, InjectionError> {
            clipboard::snapshot()
        }
        fn clipboard_write(
            &self,
            text: &str,
            exclusion: ClipboardExclusion,
        ) -> Result<u32, InjectionError> {
            clipboard::write(text, exclusion)
        }
        fn clipboard_restore(
            &self,
            snapshot: Self::Clipboard,
            expected_sequence: u32,
        ) -> Result<bool, InjectionError> {
            clipboard::restore(snapshot, expected_sequence)
        }
        fn paste(
            &self,
            target: &TargetWindow,
            policy: SafetyPolicy,
        ) -> Result<bool, InjectionError> {
            self.validate_target(target)?;
            if !policy.permits_ime(self.ime_composition_active(target)?) || !modifiers_released() {
                return Ok(false);
            }
            let inputs = [
                keyboard_input(VK_CONTROL, 0),
                keyboard_input(VK_V, 0),
                keyboard_input(VK_V, KEYEVENTF_KEYUP.0),
                keyboard_input(VK_CONTROL, KEYEVENTF_KEYUP.0),
            ];
            self.validate_target(target)?;
            let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
            if sent > 0 && sent < inputs.len() as u32 {
                // Release only our shortcut keys, never replay the paste.
                let _ = unsafe { SendInput(&inputs[2..], size_of::<INPUT>() as i32) };
            }
            Ok(sent > 0)
        }
        fn delete_selection(&self, target: &TargetWindow) -> Result<bool, InjectionError> {
            self.validate_target(target)?;
            if self.ime_composition_active(target)? != Some(false) || !modifiers_released() {
                return Ok(false);
            }
            let inputs = [
                keyboard_input(VK_BACK, 0),
                keyboard_input(VK_BACK, KEYEVENTF_KEYUP.0),
            ];
            Ok(unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) } == inputs.len() as u32)
        }
        fn wait_for_target(&self) {
            thread::sleep(Duration::from_millis(20));
        }
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
    fn modifiers_released() -> bool {
        use windows::Win32::UI::Input::KeyboardAndMouse::{VK_LWIN, VK_RWIN};
        [VK_SHIFT, VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN]
            .into_iter()
            .all(|key| unsafe { GetAsyncKeyState(key.0 as i32) } & i16::MIN == 0)
    }
}

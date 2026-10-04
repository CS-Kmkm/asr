//! Opt-in coverage of the real asynchronous Chromium UI Automation provider.
use super::*;
use std::{thread, time::Duration};
use windows::Win32::{
    System::{DataExchange::GetClipboardSequenceNumber, Threading::AttachThreadInput},
    UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow},
};

struct ClipboardCleanup<'a> {
    original: Option<clipboard::Snapshot>,
    draft: &'a str,
    corrected: &'a str,
}
impl Drop for ClipboardCleanup<'_> {
    fn drop(&mut self) {
        let sequence = unsafe { GetClipboardSequenceNumber() };
        if clipboard_win::get_clipboard_string()
            .is_ok_and(|text| text == self.draft || text == self.corrected)
        {
            if let Some(original) = self.original.take() {
                let _ = clipboard::restore(original, sequence);
            }
        }
    }
}

struct Apartment(bool);
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

#[test]
#[ignore = "interactive diagnostic: shows a native Edit window, takes focus and uses the clipboard"]
fn native_edit_replaces_provisional_draft_with_quiet_input() {
    for (draft, corrected) in [
        ("draft", "corrected"),
        ("仮の\r\n文章📝", "補正済みの\r\n文章📝"),
    ] {
        native_edit_round_trip(draft, corrected);
    }
}

/// A hidden top-level Win32 Edit control serviced by its own thread, so window
/// messages cross threads as they cross processes in real use.
struct EditProbe {
    window: windows::Win32::Foundation::HWND,
    thread_id: u32,
    thread: Option<thread::JoinHandle<()>>,
}

impl EditProbe {
    fn new(text: &str, caret: usize, visible: bool) -> Self {
        use windows::core::HSTRING;
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::System::Threading::GetCurrentThreadId;
        use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DispatchMessageW, GetMessageW, SendMessageW, SetWindowTextW,
            TranslateMessage, ES_AUTOVSCROLL, ES_MULTILINE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE,
            WS_OVERLAPPEDWINDOW, WS_VISIBLE,
        };
        let text = HSTRING::from(text);
        let (sender, receiver) = std::sync::mpsc::channel::<(isize, u32)>();
        let thread = thread::spawn(move || unsafe {
            let style = WS_OVERLAPPEDWINDOW
                | WINDOW_STYLE((ES_MULTILINE | ES_AUTOVSCROLL) as u32)
                | if visible { WS_VISIBLE } else { WINDOW_STYLE(0) };
            let window = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                windows::core::w!("EDIT"),
                windows::core::w!("ASR Native Edit Probe"),
                style,
                200,
                200,
                480,
                200,
                None,
                None,
                None,
                None,
            )
            .expect("native Edit window");
            SetWindowTextW(window, &text).unwrap();
            // EM_SETSEL places the caret.
            SendMessageW(window, 0x00B1, WPARAM(caret), LPARAM(caret as isize));
            let _ = SetFocus(window);
            sender
                .send((window.0 as isize, GetCurrentThreadId()))
                .unwrap();
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        });
        let (handle, thread_id) = receiver.recv().unwrap();
        Self {
            window: windows::Win32::Foundation::HWND(handle as *mut std::ffi::c_void),
            thread_id,
            thread: Some(thread),
        }
    }

    fn target(&self) -> TargetWindow {
        TargetWindow {
            window_handle: self.window.0 as isize,
            control_handle: self.window.0 as isize,
            process_id: std::process::id(),
            thread_id: self.thread_id,
            is_secure: false,
        }
    }
}

impl Drop for EditProbe {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Exercises the window-message path on a real Edit control. Needs no focus,
/// input or clipboard, so it runs with the ordinary suite.
#[test]
fn native_edit_messages_read_and_select_the_recent_draft() {
    for (draft, caret_text) in [
        ("draft", "prefix draft"),
        ("仮の\r\n文章📝", "prefix 仮の\r\n文章📝"),
    ] {
        let full = format!("{caret_text} suffix");
        let caret = caret_text.encode_utf16().count();
        let probe = EditProbe::new(&full, caret, false);
        let target = probe.target();
        assert!(native_edit::is_native_edit(&target));
        let state = native_edit::read(&target).unwrap();
        assert_eq!(state.before, normalize_text(caret_text));
        assert_eq!(state.selected, "");
        assert_eq!(state.after, " suffix");

        assert!(native_edit::select_recent(&target, &state, draft).unwrap());
        let selected = native_edit::read(&target).unwrap();
        assert_eq!(selected.before, "prefix ");
        assert_eq!(selected.selected, normalize_text(draft));
        assert_eq!(selected.after, " suffix");
        // A stale expectation never moves the selection.
        assert!(!native_edit::select_recent(&target, &state, draft).unwrap());
        // The IME query answers or reports that it cannot; it never fails.
        let _ = native_edit::ime_open(&target);
    }
}

fn edit_text(window: windows::Win32::Foundation::HWND) -> String {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_GETTEXT, WM_GETTEXTLENGTH};
    unsafe {
        let length = SendMessageW(window, WM_GETTEXTLENGTH, WPARAM(0), LPARAM(0)).0 as usize;
        let mut buffer = vec![0u16; length + 1];
        let copied = SendMessageW(
            window,
            WM_GETTEXT,
            WPARAM(buffer.len()),
            LPARAM(buffer.as_mut_ptr() as isize),
        )
        .0 as usize;
        normalize_text(&String::from_utf16_lossy(&buffer[..copied]))
    }
}

/// Drives the real Windows backend against a top-level Win32 Edit window owned
/// by another thread. Whichever path reads its text (UI Automation or window
/// messages), its IME state is either known closed or unobservable, so the
/// quiet-input policy must allow replacing the verified draft.
fn native_edit_round_trip(draft: &str, corrected: &str) {
    let probe = EditProbe::new("prefix  suffix", 7, true);
    let window = probe.window;
    let handle = window.0 as isize;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let _clipboard = ClipboardCleanup {
            original: Some(clipboard::snapshot().unwrap()),
            draft,
            corrected,
        };
        let current_thread = windows::Win32::System::Threading::GetCurrentThreadId();
        let foreground_thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let attached = AttachThreadInput(current_thread, foreground_thread, true).as_bool();
        let _ = SetForegroundWindow(window);
        if attached {
            let _ = AttachThreadInput(current_thread, foreground_thread, false);
        }
        thread::sleep(Duration::from_millis(300));
        let backend = PlatformBackend::new();
        let target = backend.capture_target().unwrap();
        // Never send input unless the probe window is the captured target.
        assert_eq!(target.control_handle, handle, "probe window is not focused");
        assert!(backend.quiet_input_rules_out_composition(&target));
        assert_ne!(backend.ime_composition_active(&target).unwrap(), Some(true));
        assert_eq!(backend.target_text(&target).unwrap().before, "prefix ");

        let monitor = InputMonitor::default();
        monitor.test_set_available(true);
        let checkpoint = monitor.checkpoint().unwrap();
        let mut session = batch::begin_live(
            &backend,
            InjectionOptions::default(),
            draft,
            &target,
            &monitor,
            checkpoint,
        )
        .unwrap()
        .unwrap();
        assert_eq!(session.result, InsertResult::ClipboardPaste);
        assert!(session.replacement.is_some());
        assert_eq!(
            edit_text(window),
            format!("prefix {} suffix", normalize_text(draft))
        );
        let result = batch::finish(
            &backend,
            InjectionOptions::default(),
            &mut session,
            corrected,
            &monitor,
        )
        .unwrap();
        assert_eq!(result, InsertResult::ClipboardPaste);
        assert_eq!(
            edit_text(window),
            format!("prefix {} suffix", normalize_text(corrected))
        );
    }));
    drop(probe);
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[test]
#[ignore = "interactive diagnostic: uses an isolated Chrome profile and synthetic local page"]
fn chromium_confirms_provisional_selection_before_replacement() {
    for (draft, corrected) in [
        ("draft", "corrected"),
        ("仮の\r\n文章📝", "補正済みの\r\n文章📝"),
    ] {
        chromium_round_trip(draft, corrected);
    }
}

fn chromium_round_trip(draft: &str, corrected: &str) {
    use windows::Win32::{System::Com::*, UI::Accessibility::*};
    let fixture = tempfile::tempdir().unwrap();
    let page = fixture.path().join("probe.html");
    std::fs::write(&page, "<title>ASR Correction Probe</title><textarea aria-label='ASR Correction Probe' autofocus>prefix  suffix</textarea><script>document.querySelector('textarea').setSelectionRange(7,7)</script>").unwrap();
    let mut child =
        std::process::Command::new("C:/Program Files/Google/Chrome/Application/chrome.exe")
            .arg(format!(
                "--user-data-dir={}",
                fixture.path().join("profile").display()
            ))
            .arg(format!(
                "--app=file:///{}",
                page.display().to_string().replace('\\', "/")
            ))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-extensions",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let _clipboard = ClipboardCleanup {
            original: Some(clipboard::snapshot().unwrap()),
            draft,
            corrected,
        };
        let _apartment = Apartment(CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok());
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).unwrap();
        let process_condition = automation
            .CreatePropertyCondition(
                UIA_ProcessIdPropertyId,
                &windows::core::VARIANT::from(child.id() as i32),
            )
            .unwrap();
        let root = automation.GetRootElement().unwrap();
        let mut browser = None;
        for _ in 0..50 {
            if let Ok(el) = root.FindFirst(TreeScope_Children, &process_condition) {
                browser = Some(el);
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let browser = browser.expect("isolated browser window");
        let hwnd = browser.CurrentNativeWindowHandle().unwrap();
        let current_thread = windows::Win32::System::Threading::GetCurrentThreadId();
        let foreground_thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let attached = AttachThreadInput(current_thread, foreground_thread, true).as_bool();
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(current_thread, foreground_thread, false);
        }
        let name = automation
            .CreatePropertyCondition(
                UIA_NamePropertyId,
                &windows::core::VARIANT::from("ASR Correction Probe"),
            )
            .unwrap();
        let edit_type = automation
            .CreatePropertyCondition(
                UIA_ControlTypePropertyId,
                &windows::core::VARIANT::from(UIA_EditControlTypeId.0),
            )
            .unwrap();
        let condition = automation.CreateAndCondition(&name, &edit_type).unwrap();
        let mut edit = None;
        for _ in 0..50 {
            if let Ok(el) = browser.FindFirst(TreeScope_Descendants, &condition) {
                edit = Some(el);
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let edit = edit.expect("isolated textarea");
        edit.SetFocus().unwrap();
        thread::sleep(Duration::from_millis(200));
        let backend = PlatformBackend::new();
        let target = backend.capture_target().unwrap();
        assert_eq!(target.process_id, child.id());
        assert_eq!(
            backend.ime_composition_active(&target).unwrap(),
            Some(false)
        );
        let monitor = InputMonitor::default();
        monitor.test_set_available(true);
        let mut session = batch::begin(
            &backend,
            InjectionOptions::default(),
            draft,
            &target,
            &monitor,
        )
        .unwrap()
        .unwrap();
        assert_eq!(session.result, InsertResult::ClipboardPaste);
        assert!(session.replacement.is_some());
        for text in [corrected, draft] {
            assert_eq!(
                batch::update(
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
            let value: IUIAutomationValuePattern =
                edit.GetCurrentPatternAs(UIA_ValuePatternId).unwrap();
            assert_eq!(
                value.CurrentValue().unwrap().to_string(),
                format!("prefix {} suffix", normalize_text(text))
            );
        }
        let result = batch::finish(
            &backend,
            InjectionOptions::default(),
            &mut session,
            corrected,
            &monitor,
        )
        .unwrap();
        assert_eq!(result, InsertResult::ClipboardPaste);
        let value: IUIAutomationValuePattern =
            edit.GetCurrentPatternAs(UIA_ValuePatternId).unwrap();
        assert_eq!(
            value.CurrentValue().unwrap().to_string(),
            format!("prefix {} suffix", normalize_text(corrected))
        );
    }));
    let _ = child.kill();
    let _ = child.wait();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

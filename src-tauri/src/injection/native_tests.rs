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

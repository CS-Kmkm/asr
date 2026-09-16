//! Out-of-process observation of external keyboard and pointer activity.
//!
//! The helper never records text, window titles, or clipboard contents. It
//! reports only monotonically increasing activity metadata so the parent can
//! stop replacing a provisional transcript as soon as the user interacts.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

mod shortcut;

#[cfg(all(target_os = "windows", not(test)))]
use std::{
    io::{BufRead, BufReader, Write},
    os::windows::process::CommandExt,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

#[cfg(all(target_os = "windows", not(test)))]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub(crate) struct InputMonitor {
    sequence: std::sync::Arc<AtomicU64>,
    available: std::sync::Arc<AtomicBool>,
    shortcut_pending: std::sync::Arc<AtomicBool>,
    recording_shortcut: std::sync::Mutex<Option<String>>,
    started_by_shortcut: AtomicBool,
    cancellation: std::sync::Mutex<Option<tokio::sync::watch::Receiver<bool>>>,
    #[cfg(all(target_os = "windows", not(test)))]
    ready: Arc<(Mutex<bool>, Condvar)>,
    #[cfg(all(target_os = "windows", not(test)))]
    generation: Arc<AtomicU64>,
    #[cfg(all(target_os = "windows", not(test)))]
    child: Mutex<Option<RunningMonitor>>,
}

#[cfg(all(target_os = "windows", not(test)))]
struct RunningMonitor {
    child: Child,
    stdin: ChildStdin,
}

impl Default for InputMonitor {
    fn default() -> Self {
        Self {
            sequence: std::sync::Arc::new(AtomicU64::new(0)),
            available: std::sync::Arc::new(AtomicBool::new(false)),
            shortcut_pending: std::sync::Arc::new(AtomicBool::new(false)),
            recording_shortcut: std::sync::Mutex::new(None),
            started_by_shortcut: AtomicBool::new(false),
            cancellation: std::sync::Mutex::new(None),
            #[cfg(all(target_os = "windows", not(test)))]
            ready: Arc::new((Mutex::new(false), Condvar::new())),
            #[cfg(all(target_os = "windows", not(test)))]
            generation: Arc::new(AtomicU64::new(0)),
            #[cfg(all(target_os = "windows", not(test)))]
            child: Mutex::new(None),
        }
    }
}

impl InputMonitor {
    pub(crate) fn observe_cancellation(&self, cancel: Option<tokio::sync::watch::Receiver<bool>>) {
        if let Ok(mut slot) = self.cancellation.lock() {
            *slot = cancel;
        }
    }

    fn cancelled(&self) -> bool {
        self.cancellation
            .lock()
            .map(|slot| slot.as_ref().is_some_and(|cancel| *cancel.borrow()))
            .unwrap_or(true)
    }
    pub(crate) fn start_for_recording(&self, shortcut: &str, from_shortcut: bool) -> bool {
        if shortcut::ShortcutFilter::parse(shortcut).is_none() {
            return false;
        }
        let Ok(mut configured) = self.recording_shortcut.lock() else {
            return false;
        };
        *configured = Some(shortcut.to_owned());
        self.started_by_shortcut
            .store(from_shortcut, Ordering::Release);
        drop(configured);
        self.start()
    }

    pub(crate) fn shortcut_pending(&self) -> bool {
        #[cfg(all(target_os = "windows", not(test)))]
        {
            use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
            if [0x10, 0x11, 0x12, 0x5B, 0x5C]
                .iter()
                .any(|key| unsafe { GetAsyncKeyState(*key) } & i16::MIN != 0)
            {
                return true;
            }
        }
        self.shortcut_pending.load(Ordering::Acquire)
    }

    pub(crate) async fn wait_for_shortcut_release(&self) {
        for _ in 0..100 {
            if !self.shortcut_pending() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Starts the monitor lazily. Tests and non-Windows builds deliberately
    /// return false so they never recursively spawn the test/app executable.
    pub(crate) fn start(&self) -> bool {
        #[cfg(all(target_os = "windows", not(test)))]
        {
            if self.available.load(Ordering::Acquire) {
                return true;
            }
            let Ok(mut slot) = self.child.lock() else {
                return false;
            };
            if let Some(running) = slot.as_mut() {
                if running.child.try_wait().ok().flatten().is_none() {
                    return self.wait_until_ready();
                }
                *slot = None;
            }

            let Ok(executable) = std::env::current_exe() else {
                return false;
            };
            let mut command = Command::new(executable);
            command
                .arg("--input-monitor")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW);
            if let Ok(shortcut) = self.recording_shortcut.lock() {
                if let Some(shortcut) = shortcut.as_ref() {
                    command.arg("--recording-shortcut").arg(shortcut);
                    if self.started_by_shortcut.load(Ordering::Acquire) {
                        command.arg("--start-shortcut");
                    }
                }
            }
            let Ok(mut child) = command.spawn() else {
                return false;
            };
            let Some(stdin) = child.stdin.take() else {
                let _ = child.kill();
                return false;
            };
            let Some(stdout) = child.stdout.take() else {
                let _ = child.kill();
                return false;
            };
            // Each helper process starts its own counter at zero. Reset the
            // parent-side value before accepting events from the new process,
            // otherwise fetch_max would ignore early activity after a restart.
            self.sequence.store(0, Ordering::Release);
            self.available.store(false, Ordering::Release);
            let generation_id = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
            if let Ok(mut ready) = self.ready.0.lock() {
                *ready = false;
            }
            let sequence = Arc::clone(&self.sequence);
            let available = Arc::clone(&self.available);
            let shortcut_pending = Arc::clone(&self.shortcut_pending);
            let ready = Arc::clone(&self.ready);
            let generation = Arc::clone(&self.generation);
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if generation.load(Ordering::Acquire) != generation_id {
                        break;
                    }
                    let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                        continue;
                    };
                    if let Some(pending) = event.get("pending").and_then(serde_json::Value::as_bool)
                    {
                        shortcut_pending.store(pending, Ordering::Release);
                    }
                    match event.get("kind").and_then(serde_json::Value::as_str) {
                        Some("ready") => {
                            available.store(true, Ordering::Release);
                            if let Ok(mut state) = ready.0.lock() {
                                *state = true;
                                ready.1.notify_all();
                            }
                        }
                        Some("input") => {
                            if let Some(next) =
                                event.get("sequence").and_then(serde_json::Value::as_u64)
                            {
                                sequence.fetch_max(next, Ordering::AcqRel);
                            }
                        }
                        _ => {}
                    }
                }
                if generation.load(Ordering::Acquire) == generation_id {
                    available.store(false, Ordering::Release);
                }
            });
            *slot = Some(RunningMonitor { child, stdin });
            drop(slot);
            return self.wait_until_ready();
        }
        #[cfg(any(not(target_os = "windows"), test))]
        self.available.load(Ordering::Acquire)
    }

    #[cfg(all(target_os = "windows", not(test)))]
    fn wait_until_ready(&self) -> bool {
        let Ok(ready) = self.ready.0.lock() else {
            return false;
        };
        if *ready {
            return true;
        }
        self.ready
            .1
            .wait_timeout_while(ready, Duration::from_secs(2), |value| !*value)
            .ok()
            .is_some_and(|(value, _)| *value)
    }

    pub(crate) fn checkpoint(&self) -> Option<u64> {
        self.available
            .load(Ordering::Acquire)
            .then(|| self.sequence.load(Ordering::Acquire))
    }

    pub(crate) fn unchanged_since(&self, checkpoint: u64) -> bool {
        !self.cancelled()
            && self.available.load(Ordering::Acquire)
            && self.sequence.load(Ordering::Acquire) == checkpoint
    }

    pub(crate) fn shutdown(&self) {
        #[cfg(all(target_os = "windows", not(test)))]
        {
            // Invalidate the reader before stopping its child so a delayed EOF
            // cannot mark a later helper process unavailable.
            self.generation.fetch_add(1, Ordering::AcqRel);
            if let Ok(mut slot) = self.child.lock() {
                if let Some(mut running) = slot.take() {
                    let _ = running.stdin.write_all(b"shutdown\n");
                    let _ = running.stdin.flush();
                    let deadline = Instant::now() + Duration::from_millis(500);
                    while Instant::now() < deadline {
                        if running.child.try_wait().ok().flatten().is_some() {
                            self.available.store(false, Ordering::Release);
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let _ = running.child.kill();
                    let _ = running.child.wait();
                }
            }
        }
        self.available.store(false, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn test_set_available(&self, value: bool) {
        self.available.store(value, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn test_record_input(&self) {
        self.sequence.fetch_add(1, Ordering::AcqRel);
    }
}

#[cfg(target_os = "windows")]
pub fn run_worker() {
    windows_worker::run();
}

#[cfg(not(target_os = "windows"))]
pub fn run_worker() {}

#[cfg(target_os = "windows")]
mod windows_worker {
    use std::{
        io::{BufRead, Write},
        mem::zeroed,
        sync::{
            atomic::{AtomicU64, Ordering},
            mpsc::{channel, Sender},
            OnceLock,
        },
    };

    use windows::Win32::{
        Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM},
        System::Threading::GetCurrentThreadId,
        UI::WindowsAndMessaging::{
            CallNextHookEx, GetMessageW, PeekMessageW, PostThreadMessageW, SetWindowsHookExW,
            UnhookWindowsHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT,
            PM_NOREMOVE, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
            WM_MBUTTONDOWN, WM_MOUSEHWHEEL, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_SYSKEYDOWN,
            WM_SYSKEYUP, WM_XBUTTONDOWN,
        },
    };

    use crate::injection::INJECTION_MARKER;

    use super::shortcut::ShortcutFilter;
    static FILTER: OnceLock<std::sync::Mutex<ShortcutFilter>> = OnceLock::new();
    static EVENTS: OnceLock<Sender<(&'static str, u64, bool)>> = OnceLock::new();
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32
            && matches!(
                wparam.0 as u32,
                WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP
            )
        {
            let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            if event.dwExtraInfo != INJECTION_MARKER {
                if let Some(sender) = EVENTS.get() {
                    let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
                    let (activity, pending) = FILTER
                        .get()
                        .and_then(|filter| filter.lock().ok())
                        .map(|mut filter| (filter.event(event.vkCode, down), filter.pending()))
                        .unwrap_or((down, false));
                    let sequence = if activity {
                        SEQUENCE.fetch_add(1, Ordering::AcqRel) + 1
                    } else {
                        SEQUENCE.load(Ordering::Acquire)
                    };
                    let _ = sender.send(("keyboard", sequence, pending));
                }
            }
        }
        unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
    }

    unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32
            && matches!(
                wparam.0 as u32,
                WM_LBUTTONDOWN
                    | WM_RBUTTONDOWN
                    | WM_MBUTTONDOWN
                    | WM_XBUTTONDOWN
                    | WM_MOUSEWHEEL
                    | WM_MOUSEHWHEEL
            )
        {
            let event = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            if event.dwExtraInfo != INJECTION_MARKER {
                if let Some(sender) = EVENTS.get() {
                    let sequence = SEQUENCE.fetch_add(1, Ordering::AcqRel) + 1;
                    let pending = FILTER
                        .get()
                        .and_then(|filter| filter.lock().ok())
                        .is_some_and(|filter| filter.pending());
                    let _ = sender.send(("pointer", sequence, pending));
                }
            }
        }
        unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
    }

    pub(super) fn run() {
        let mut arguments = std::env::args();
        if arguments.any(|arg| arg == "--recording-shortcut") {
            let Some(mut filter) = arguments.next().as_deref().and_then(ShortcutFilter::parse)
            else {
                return;
            };
            filter.initialize(std::env::args().any(|arg| arg == "--start-shortcut"), |vk| unsafe {
                windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(vk as i32)
            } & i16::MIN != 0);
            let _ = FILTER.set(std::sync::Mutex::new(filter));
        }
        let (sender, receiver) = channel();
        let _ = EVENTS.set(sender);
        std::thread::spawn(move || {
            while let Ok((device, sequence, pending)) = receiver.recv() {
                // Do not hold stdout's process-wide lock while waiting for an
                // event; the hook thread must leave the ready handshake free
                // to use the same pipe.
                let stdout = std::io::stdout();
                let mut output = stdout.lock();
                let _ = writeln!(
                    output,
                    "{}",
                    serde_json::json!({
                        "kind": "input",
                        "device": device,
                        "sequence": sequence,
                        "pending": pending
                    })
                );
                let _ = output.flush();
            }
        });

        let keyboard = unsafe {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), HINSTANCE::default(), 0)
        };
        let Ok(keyboard) = keyboard else {
            return;
        };
        let mouse =
            unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), HINSTANCE::default(), 0) };
        let Ok(mouse) = mouse else {
            let _ = unsafe { UnhookWindowsHookEx(keyboard) };
            return;
        };
        let thread_id = unsafe { GetCurrentThreadId() };
        let mut message: MSG = unsafe { zeroed() };
        let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };
        std::thread::spawn(move || {
            let mut command = String::new();
            let _ = std::io::stdin().lock().read_line(&mut command);
            let _ = unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        });
        let pending = FILTER
            .get()
            .and_then(|filter| filter.lock().ok())
            .is_some_and(|filter| filter.pending());
        println!(
            "{}",
            serde_json::json!({ "kind": "ready", "pending": pending })
        );
        let _ = std::io::stdout().flush();

        while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {}
        let _ = unsafe { UnhookWindowsHookEx(keyboard) };
        let _ = unsafe { UnhookWindowsHookEx(mouse) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_changes_only_after_external_activity() {
        let monitor = InputMonitor::default();
        monitor.test_set_available(true);
        let checkpoint = monitor.checkpoint().unwrap();
        assert!(monitor.unchanged_since(checkpoint));
        monitor.test_record_input();
        assert!(!monitor.unchanged_since(checkpoint));
    }
}

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
    process::{Child, ChildStdin, Stdio},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

#[cfg(all(target_os = "windows", not(test)))]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub(crate) struct InputMonitor {
    sequence: std::sync::Arc<AtomicU64>,
    available: std::sync::Arc<AtomicBool>,
    shortcut_pending: std::sync::Arc<AtomicBool>,
    recording_shortcuts: std::sync::Mutex<Vec<String>>,
    #[cfg(test)]
    async_modifier_pending: AtomicBool,
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
    /// Set once the helper's output ended. The helper exits on its own when
    /// its hooks are gone, and may still be shutting down when it is next
    /// started; that process must be replaced, not waited on.
    closed: Arc<AtomicBool>,
}

impl Default for InputMonitor {
    fn default() -> Self {
        Self {
            sequence: std::sync::Arc::new(AtomicU64::new(0)),
            available: std::sync::Arc::new(AtomicBool::new(false)),
            shortcut_pending: std::sync::Arc::new(AtomicBool::new(false)),
            recording_shortcuts: std::sync::Mutex::new(Vec::new()),
            #[cfg(test)]
            async_modifier_pending: AtomicBool::new(false),
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

impl Drop for InputMonitor {
    fn drop(&mut self) {
        self.shutdown();
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
    pub(crate) fn start_for_recording(
        &self,
        shortcut: &str,
        alternatives: &[String],
        from_shortcut: bool,
    ) -> bool {
        if shortcut::ShortcutFilters::parse(shortcut, alternatives).is_none() {
            return false;
        }
        let Ok(mut configured) = self.recording_shortcuts.lock() else {
            return false;
        };
        *configured = std::iter::once(shortcut.to_owned())
            .chain(alternatives.iter().cloned())
            .collect();
        self.started_by_shortcut
            .store(from_shortcut, Ordering::Release);
        drop(configured);
        self.start()
    }

    pub(crate) fn shortcut_pending(&self) -> bool {
        #[cfg(test)]
        if self.async_modifier_pending.load(Ordering::Acquire) {
            return true;
        }
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
                if !running.closed.load(Ordering::Acquire)
                    && running.child.try_wait().ok().flatten().is_none()
                {
                    return self.wait_until_ready();
                }
                // The helper exited, or closed its output and is exiting:
                // reap it and start a fresh one.
                let _ = running.child.kill();
                let _ = running.child.wait();
                *slot = None;
            }

            let Ok(executable) = std::env::current_exe() else {
                return false;
            };
            let mut command = crate::environment::helper_command(executable);
            command
                .arg("--input-monitor")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW);
            if let Ok(shortcuts) = self.recording_shortcuts.lock() {
                if !shortcuts.is_empty() {
                    command
                        .arg("--recording-shortcuts")
                        .arg(serde_json::to_string(&*shortcuts).unwrap_or_default());
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
            let closed = Arc::new(AtomicBool::new(false));
            let output_closed = Arc::clone(&closed);
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
                // End of output: the helper exited (including after it found
                // its hooks removed) or crashed. Fail closed until a later
                // start() replaces it.
                if generation.load(Ordering::Acquire) == generation_id {
                    available.store(false, Ordering::Release);
                }
                output_closed.store(true, Ordering::Release);
            });
            *slot = Some(RunningMonitor {
                child,
                stdin,
                closed,
            });
            drop(slot);
            self.wait_until_ready()
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
            && !self.shortcut_pending()
            && self.sequence.load(Ordering::Acquire) == checkpoint
    }

    /// After our own paste is queued, its injected modifier (Ctrl or Shift)
    /// may briefly be visible to GetAsyncKeyState. The helper ignores marked
    /// injected keys but still reports physical shortcut activity and
    /// increments the input sequence.
    pub(crate) fn unchanged_since_injected_paste(&self, checkpoint: u64) -> bool {
        !self.cancelled()
            && self.available.load(Ordering::Acquire)
            && !self.shortcut_pending.load(Ordering::Acquire)
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

    #[cfg(test)]
    pub(crate) fn test_set_shortcut_pending(&self, value: bool) {
        self.shortcut_pending.store(value, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn test_set_async_modifier_pending(&self, value: bool) {
        self.async_modifier_pending.store(value, Ordering::Release);
    }
}

/// Windows removes a low-level hook without notice when its callback misses
/// the LowLevelHooksTimeout (capped at 1 s since Windows 10 1709), after which
/// the helper would keep reporting "no input". `GetLastInputInfo` keeps
/// advancing for keyboard and mouse input, so the helper compares it with the
/// timestamp of the last event either hook observed. Both are event
/// timestamps on the same clock; a caller of `SendInput` may set them, so they
/// are not guaranteed to be monotonic.
///
/// The tolerance is twice the longest hook timeout, so a live hook that is
/// merely slow is never judged removed. It also absorbs input that updates the
/// session's input time without reaching the hooks (input on the secure
/// desktop, input another app's hook swallows, possibly some pen or touch
/// paths) when it comes within the tolerance of hook-visible input, which
/// later hook-visible input also clears. A false positive
/// only ends this helper, so the current guard fails closed and the next
/// `start()` launches a new one.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const HOOK_STALE_TOLERANCE_MS: u32 = 2_000;
/// How often the helper samples `GetLastInputInfo`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const HOOK_WATCHDOG_PERIOD_MS: u64 = 250;

/// True when the session's last input is stamped more than `tolerance_ms`
/// after the last event the hooks observed. Ticks compare across the 49.7-day
/// wraparound; the input counts as newer only while it is less than half the
/// tick range ahead.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn hooks_stale(last_system_input_tick: u32, last_hook_tick: u32, tolerance_ms: u32) -> bool {
    let ahead = last_system_input_tick.wrapping_sub(last_hook_tick);
    ahead < 1 << 31 && ahead > tolerance_ms
}

/// One reading of the session's input time and of the hooks' progress.
#[derive(Clone, Copy)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
struct HookReading {
    /// `GetTickCount` when the reading was taken.
    now: u32,
    /// `GetLastInputInfo().dwTime`.
    last_system_input_tick: u32,
    /// Timestamp of the latest event either hook observed (latest, not max).
    last_hook_tick: u32,
    /// Number of hook callbacks so far (wrapping).
    hook_calls: u32,
}

/// Applies `hooks_stale` to input the hooks have had time to observe.
///
/// The session's input time is updated before the hooks run, so a reading
/// taken in between shows input "ahead" of a live hook. The watchdog therefore
/// holds each reading for `tolerance_ms`. Any hook callback during that time
/// proves the hooks were still installed after the reading; only when there
/// was none does it compare the held input time with the hooks' last event,
/// which is then unchanged since the reading. So a timestamp out of order (a
/// `SendInput` caller may set it) cannot make live hooks look removed: an
/// event the hooks observe after the reading counts as a callback whatever
/// its timestamp.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
struct HookWatchdog {
    tolerance_ms: u32,
    held: Option<HookReading>,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl HookWatchdog {
    fn new(tolerance_ms: u32) -> Self {
        Self {
            tolerance_ms,
            held: None,
        }
    }

    /// Takes one reading and returns true when the hooks missed input and
    /// have not run for at least `tolerance_ms` since.
    fn poll(&mut self, reading: HookReading) -> bool {
        if let Some(held) = self.held {
            if reading.now.wrapping_sub(held.now) < self.tolerance_ms {
                return false;
            }
            if reading.hook_calls == held.hook_calls
                && hooks_stale(
                    held.last_system_input_tick,
                    reading.last_hook_tick,
                    self.tolerance_ms,
                )
            {
                return true;
            }
        }
        self.held = Some(reading);
        false
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
            atomic::{AtomicU32, AtomicU64, Ordering},
            mpsc::{channel, Sender},
            OnceLock,
        },
        time::Duration,
    };

    use windows::Win32::{
        Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM},
        System::{
            SystemInformation::GetTickCount,
            Threading::{
                GetCurrentThread, GetCurrentThreadId, SetThreadPriority,
                THREAD_PRIORITY_TIME_CRITICAL,
            },
        },
        UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO},
        UI::WindowsAndMessaging::{
            CallNextHookEx, GetMessageW, PeekMessageW, PostThreadMessageW, SetWindowsHookExW,
            UnhookWindowsHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT,
            PM_NOREMOVE, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
            WM_MBUTTONDOWN, WM_MOUSEHWHEEL, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_SYSKEYDOWN,
            WM_SYSKEYUP, WM_XBUTTONDOWN,
        },
    };

    use crate::injection::INJECTION_MARKER;

    use super::shortcut::ShortcutFilters;
    static FILTER: OnceLock<std::sync::Mutex<ShortcutFilters>> = OnceLock::new();
    static EVENTS: OnceLock<Sender<(&'static str, u64, bool)>> = OnceLock::new();
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    /// Timestamp of the latest event either hook observed, for every message
    /// (mouse moves and injected input included) so that any input the
    /// session sees is matched by a hook call while the hooks are installed.
    /// It is the event's own `time`, the clock `GetLastInputInfo` reports,
    /// and is stored as the latest value, not the maximum.
    static LAST_HOOK_TICK: AtomicU32 = AtomicU32::new(0);
    /// Hook callbacks so far, so that the watchdog can tell that the hooks ran
    /// whatever timestamp the event carried.
    static HOOK_CALLS: AtomicU32 = AtomicU32::new(0);

    fn record_hook_event(time: u32) {
        LAST_HOOK_TICK.store(time, Ordering::Release);
        HOOK_CALLS.fetch_add(1, Ordering::AcqRel);
    }

    fn last_system_input_tick() -> Option<u32> {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        unsafe { GetLastInputInfo(&mut info) }
            .as_bool()
            .then_some(info.dwTime)
    }

    unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            record_hook_event(unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) }.time);
        }
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
        if code == HC_ACTION as i32 {
            record_hook_event(unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) }.time);
        }
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
        if arguments.any(|arg| arg == "--recording-shortcuts") {
            let Some(mut filter) = arguments
                .next()
                .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
                .and_then(|chords| ShortcutFilters::parse(chords.first()?, &chords[1..]))
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

        // This thread installs the hooks, so Windows calls them on it; a slow
        // call past LowLevelHooksTimeout gets them removed. It is idle in
        // GetMessageW except for those short callbacks, which take no lock
        // that a lower-priority thread holds (the filter lock is only used
        // here and channel sends never block), so the highest non-realtime
        // priority cannot starve other threads or invert. HIGHEST (+2) could
        // still lose to normal-priority threads boosted for foreground input.
        let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
        // Input before the hooks predates them and cannot count against them:
        // start from the session's current input time. Input in the moments
        // until they are installed stays well within the tolerance.
        LAST_HOOK_TICK.store(
            last_system_input_tick().unwrap_or_else(|| unsafe { GetTickCount() }),
            Ordering::Release,
        );
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
        std::thread::spawn(move || watch_hooks(thread_id));
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

    /// Ends the helper once Windows has evidently removed its hooks. The
    /// parent sees its output end, reports the monitor unavailable (so guards
    /// fail closed), and starts a fresh helper on its next `start()`.
    fn watch_hooks(hook_thread: u32) {
        let mut watchdog = super::HookWatchdog::new(super::HOOK_STALE_TOLERANCE_MS);
        loop {
            std::thread::sleep(Duration::from_millis(super::HOOK_WATCHDOG_PERIOD_MS));
            let hook_calls = HOOK_CALLS.load(Ordering::Acquire);
            let Some(last_system_input_tick) = last_system_input_tick() else {
                continue;
            };
            let reading = super::HookReading {
                now: unsafe { GetTickCount() },
                last_system_input_tick,
                last_hook_tick: LAST_HOOK_TICK.load(Ordering::Acquire),
                hook_calls,
            };
            if watchdog.poll(reading) {
                let _ = unsafe { PostThreadMessageW(hook_thread, WM_QUIT, WPARAM(0), LPARAM(0)) };
                return;
            }
        }
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

    #[test]
    fn pending_recording_shortcut_blocks_an_unchanged_checkpoint() {
        let monitor = InputMonitor::default();
        monitor.test_set_available(true);
        let checkpoint = monitor.checkpoint().unwrap();
        monitor.test_set_shortcut_pending(true);
        assert!(!monitor.unchanged_since(checkpoint));
        monitor.test_set_shortcut_pending(false);
        assert!(monitor.unchanged_since(checkpoint));
    }

    #[test]
    fn injected_paste_guard_still_rejects_physical_shortcut_and_input() {
        let monitor = InputMonitor::default();
        monitor.test_set_available(true);
        let checkpoint = monitor.checkpoint().unwrap();
        assert!(monitor.unchanged_since_injected_paste(checkpoint));
        monitor.test_set_async_modifier_pending(true);
        assert!(!monitor.unchanged_since(checkpoint));
        assert!(monitor.unchanged_since_injected_paste(checkpoint));
        monitor.test_set_async_modifier_pending(false);
        monitor.test_set_shortcut_pending(true);
        assert!(!monitor.unchanged_since_injected_paste(checkpoint));
        monitor.test_set_shortcut_pending(false);
        monitor.test_record_input();
        assert!(!monitor.unchanged_since_injected_paste(checkpoint));
    }

    const TOLERANCE: u32 = HOOK_STALE_TOLERANCE_MS;

    #[test]
    fn hooks_are_stale_only_when_input_outran_them_beyond_the_tolerance() {
        // Equal ticks: the hooks saw the latest input.
        assert!(!hooks_stale(50_000, 50_000, TOLERANCE));
        // A hook call after the latest input (a mouse move, our own paste).
        assert!(!hooks_stale(50_000, 50_001, TOLERANCE));
        assert!(!hooks_stale(50_000, 90_000, TOLERANCE));
        // Session input ahead of the hooks, within the tolerance.
        assert!(!hooks_stale(51_500, 50_000, TOLERANCE));
        assert!(!hooks_stale(50_000 + TOLERANCE, 50_000, TOLERANCE));
        // Beyond the tolerance.
        assert!(hooks_stale(50_001 + TOLERANCE, 50_000, TOLERANCE));
        assert!(hooks_stale(900_000, 50_000, TOLERANCE));
    }

    #[test]
    fn hook_staleness_compares_across_the_tick_wraparound() {
        // Input after the 49.7-day wrap, hooks last seen just before it.
        assert!(hooks_stale(1_600, u32::MAX - 500, TOLERANCE));
        assert!(!hooks_stale(1_000, u32::MAX - 500, TOLERANCE));
        // Hooks seen after the wrap, input just before it.
        assert!(!hooks_stale(u32::MAX - 100, 100, TOLERANCE));
    }

    fn reading(now: u32, input: u32, hook: u32, hook_calls: u32) -> HookReading {
        HookReading {
            now,
            last_system_input_tick: input,
            last_hook_tick: hook,
            hook_calls,
        }
    }

    #[test]
    fn the_watchdog_waits_for_a_live_hook_to_observe_new_input() {
        let mut watchdog = HookWatchdog::new(TOLERANCE);
        // The hooks last ran ten minutes ago; input arrives and the session's
        // input time is read before the hook callback has run.
        let (input, idle_hook) = (600_000, 0);
        assert!(hooks_stale(input, idle_hook, TOLERANCE));
        assert!(!watchdog.poll(reading(input, input, idle_hook, 7)));
        // The live hook observes that input right after; nothing is reported.
        assert!(!watchdog.poll(reading(input + 250, input, input, 8)));
        assert!(!watchdog.poll(reading(input + TOLERANCE, input, input, 8)));
        assert!(!watchdog.poll(reading(input + 2 * TOLERANCE, input, input, 8)));
        assert!(!watchdog.poll(reading(input + 3 * TOLERANCE, input, input, 8)));
    }

    #[test]
    fn the_watchdog_reports_input_the_hooks_never_saw() {
        let mut watchdog = HookWatchdog::new(TOLERANCE);
        let hook = 100_000;
        // The hooks were removed; the user keeps typing without a pause.
        let mut now = 100_000;
        let mut reported = None;
        while now < 110_000 {
            now += 250;
            if watchdog.poll(reading(now, now - 10, hook, 42)) {
                reported = Some(now);
                break;
            }
        }
        let reported = reported.expect("removed hooks are reported");
        assert!(reported - hook <= 3 * TOLERANCE, "{reported}");
        // Input that stopped before the hooks were removed is not evidence.
        let mut watchdog = HookWatchdog::new(TOLERANCE);
        for step in 1..40 {
            assert!(!watchdog.poll(reading(hook + step * 250, hook, hook, 42)));
        }
    }

    #[test]
    fn out_of_order_timestamps_do_not_make_live_hooks_look_removed() {
        // A SendInput caller stamps an event an hour ahead. The session's
        // input time and the hooks both take that stamp, so they agree.
        let now = 1_000_000;
        let ahead = now + 3_600_000;
        let mut watchdog = HookWatchdog::new(TOLERANCE);
        assert!(!watchdog.poll(reading(now, ahead, ahead, 1)));
        // A normally stamped event follows: the hooks' latest stamp moves
        // back, behind the held input time, but the hooks did run.
        assert!(hooks_stale(ahead, now + 500, TOLERANCE));
        assert!(!watchdog.poll(reading(now + 500, now + 500, now + 500, 2)));
        assert!(!watchdog.poll(reading(now + TOLERANCE, now + 500, now + 500, 2)));
        assert!(!watchdog.poll(reading(now + 3 * TOLERANCE, now + 500, now + 500, 2)));
        // An event stamped in the past that the hooks never saw is no
        // evidence either.
        let mut watchdog = HookWatchdog::new(TOLERANCE);
        assert!(!watchdog.poll(reading(now, now - 60_000, now, 5)));
        assert!(!watchdog.poll(reading(now + TOLERANCE, now - 60_000, now, 5)));
        // With the hooks gone, a future-stamped input is still reported.
        let mut watchdog = HookWatchdog::new(TOLERANCE);
        assert!(!watchdog.poll(reading(now, ahead, now, 5)));
        assert!(watchdog.poll(reading(now + TOLERANCE, ahead, now, 5)));
    }

    #[test]
    fn the_watchdog_ages_samples_across_the_tick_wraparound() {
        let mut watchdog = HookWatchdog::new(TOLERANCE);
        let hook = u32::MAX - 3_000;
        let input = u32::MAX - 100;
        assert!(!watchdog.poll(reading(input, input, hook, 3)));
        // 1.25 s later the counter has wrapped; the sample is not old enough.
        assert!(!watchdog.poll(reading(input.wrapping_add(1_250), input, hook, 3)));
        assert!(watchdog.poll(reading(input.wrapping_add(TOLERANCE), input, hook, 3)));
        // A hook call counter that wrapped still shows the hooks ran.
        let mut watchdog = HookWatchdog::new(TOLERANCE);
        assert!(!watchdog.poll(reading(input, input, hook, u32::MAX)));
        assert!(!watchdog.poll(reading(input.wrapping_add(TOLERANCE), input, input, 0)));
    }
}

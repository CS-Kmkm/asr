//! Clipboard read attribution for the final terminal paste.
use super::*;
use crate::injection::TerminalReceipt;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Instant;
use windows::Win32::Foundation::SetLastError;
use windows::Win32::System::DataExchange::GetOpenClipboardWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetForegroundWindow, GetWindowLongPtrW, GetWindowThreadProcessId, PeekMessageW,
    SetWindowLongPtrW, GA_ROOT, GWLP_USERDATA, MSG, PM_NOREMOVE, WM_RENDERALLFORMATS,
    WM_RENDERFORMAT,
};

const READ_TIMEOUT: Duration = Duration::from_secs(2);
const START_TIMEOUT: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(5);
const PENDING: u8 = 0;
const TARGET_READ: u8 = 1;
const OTHER_READ: u8 = 2;

#[derive(Debug, Default)]
struct Signal {
    registered: AtomicBool,
    dispatched: AtomicBool,
    released: AtomicBool,
    read: AtomicU8,
}

#[derive(Debug)]
pub(super) struct Receipt {
    owner: isize,
    signal: Arc<Signal>,
}

impl Receipt {
    pub(super) fn start(text: &str, target: &TargetWindow) -> Option<Self> {
        let text = text.to_owned();
        let window = target.window_handle;
        let process = target.process_id;
        let signal = Arc::new(Signal::default());
        let worker_signal = Arc::clone(&signal);
        let (ready, connected) = mpsc::sync_channel(0);
        thread::Builder::new()
            .name("terminal-clipboard".into())
            .spawn(move || own_clipboard(&text, window, process, worker_signal, &ready))
            .ok()?;
        let owner = connected.recv_timeout(START_TIMEOUT).ok()?;
        Some(Self { owner, signal })
    }

    pub(super) fn owner(&self) -> HWND {
        HWND(self.owner as *mut _)
    }

    pub(super) fn registered(&self) {
        self.signal.registered.store(true, Ordering::Release);
    }
}

impl TerminalReceipt for Receipt {
    fn stamp(&self) -> Option<ClipboardStamp> {
        let sequence = sequence();
        owns_clipboard(self.owner()).then_some(ClipboardStamp {
            sequence,
            owner: Some(self.owner),
        })
    }

    fn arm(&self) {
        self.signal.dispatched.store(true, Ordering::Release);
    }

    fn wait_for_read(&self, cancelled: &dyn Fn() -> bool) -> Option<ClipboardStamp> {
        let deadline = Instant::now() + READ_TIMEOUT;
        while !cancelled() && Instant::now() < deadline && owns_clipboard(self.owner()) {
            match self.signal.read.load(Ordering::Acquire) {
                TARGET_READ => {
                    // Take the sequence only after the reader released its lock.
                    if unsafe { OpenClipboard(None) }.is_ok() {
                        let stamp = self.stamp();
                        return unsafe { CloseClipboard() }.ok().and(stamp);
                    }
                }
                PENDING => {}
                _ => return None,
            }
            thread::sleep(POLL);
        }
        None
    }
}

impl Drop for Receipt {
    fn drop(&mut self) {
        self.signal.released.store(true, Ordering::Release);
    }
}

struct RenderState {
    payload: Option<Entry>,
    target_window: isize,
    target_process: u32,
    signal: Arc<Signal>,
}

fn owns_clipboard(owner: HWND) -> bool {
    unsafe { GetClipboardOwner() }.is_ok_and(|current| current == owner)
}

fn own_clipboard(
    text: &str,
    window: isize,
    process: u32,
    signal: Arc<Signal>,
    ready: &mpsc::SyncSender<isize>,
) {
    let Ok(payload) = unicode_entry(text) else {
        return;
    };
    let Ok(owner) = owner_window() else { return };
    let state = Box::new(RefCell::new(RenderState {
        payload: Some(payload),
        target_window: window,
        target_process: process,
        signal,
    }));
    // Windows calls the procedure on this thread. Keep the state alive and
    // installed through DestroyWindow, including any final render message.
    let installed = unsafe {
        SetLastError(ERROR_SUCCESS);
        let _ = SetWindowLongPtrW(
            owner,
            GWLP_USERDATA,
            std::ptr::from_ref(state.as_ref()) as isize,
        );
        GetLastError() == ERROR_SUCCESS
    };
    if installed && ready.send(owner.0 as isize).is_ok() {
        let mut msg = MSG::default();
        loop {
            // PeekMessage also dispatches incoming sent messages such as
            // WM_RENDERFORMAT. No message posted to this window is needed.
            unsafe {
                let _ = PeekMessageW(&mut msg, owner, 0, 0, PM_NOREMOVE);
            }
            if state.borrow().signal.released.load(Ordering::Acquire) {
                if !state.borrow().signal.registered.load(Ordering::Acquire)
                    || !owns_clipboard(owner)
                    || state.borrow().payload.is_none()
                {
                    break;
                }
                // A copy-only or timed-out attempt must leave usable text.
                // If another reader holds the clipboard open, keep serving
                // render requests until we can materialize it or lose ownership.
                render_all(owner, &state);
            }
            thread::sleep(POLL);
        }
    }
    unsafe {
        let _ = DestroyWindow(owner);
    }
}

pub(super) fn handle_message(owner: HWND, msg: u32, format: WPARAM) -> bool {
    if msg != WM_RENDERFORMAT && msg != WM_RENDERALLFORMATS {
        return false;
    }
    let pointer = unsafe { GetWindowLongPtrW(owner, GWLP_USERDATA) } as *const RefCell<RenderState>;
    // SAFETY: Only the owner thread accesses this pointer, and its boxed
    // state outlives the window. Ordinary eager owners have zero user data.
    let Some(state) = (unsafe { pointer.as_ref() }) else {
        return false;
    };
    if msg == WM_RENDERALLFORMATS {
        render_all(owner, state);
    } else if format.0 == CF_UNICODETEXT as usize {
        render_requested(state);
    }
    true
}

fn render_requested(state: &RefCell<RenderState>) {
    let Ok(mut state) = state.try_borrow_mut() else {
        return;
    };
    let attributed = state.signal.dispatched.load(Ordering::Acquire)
        && is_target_reader(state.target_window, state.target_process);
    // The requesting process already holds the clipboard open.
    if render(&mut state) {
        let read = if attributed { TARGET_READ } else { OTHER_READ };
        state.signal.read.store(read, Ordering::Release);
    }
}

fn is_target_reader(window: isize, process: u32) -> bool {
    let Ok(reader) = (unsafe { GetOpenClipboardWindow() }) else {
        return false;
    };
    let mut actual_process = 0;
    let thread = unsafe { GetWindowThreadProcessId(reader, Some(&mut actual_process)) };
    thread != 0
        && actual_process == process
        && unsafe { GetAncestor(reader, GA_ROOT) }.0 as isize == window
        && unsafe { GetForegroundWindow() }.0 as isize == window
}

fn render_all(owner: HWND, state: &RefCell<RenderState>) {
    let registered = state
        .try_borrow()
        .is_ok_and(|state| state.signal.registered.load(Ordering::Acquire));
    if !registered || unsafe { OpenClipboard(owner) }.is_err() {
        return;
    }
    if owns_clipboard(owner) {
        if let Ok(mut state) = state.try_borrow_mut() {
            render(&mut state);
        }
    }
    unsafe {
        let _ = CloseClipboard();
    }
}

fn render(state: &mut RenderState) -> bool {
    if let Some(payload) = state.payload.as_mut() {
        if set_entry(payload).is_ok() {
            state.payload = None;
            return true;
        }
    }
    false
}

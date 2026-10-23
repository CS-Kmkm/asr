use super::{normalize_text, InjectionError, TargetText, TargetWindow};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    SAFEARRAY,
};
use windows::Win32::System::Ole::{
    SafeArrayDestroy, SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, IUIAutomation2, IUIAutomationElement, IUIAutomationTextPattern,
    IUIAutomationTextRange, TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_Start,
    TextUnit_Character, UIA_TextPatternId,
};

const MAX_TEXT_UNITS: i32 = 1_048_576;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(2);
type Job = Box<dyn FnOnce(&IUIAutomation2, Instant) + Send>;
struct Worker {
    sender: mpsc::SyncSender<Job>,
    ready: Mutex<Option<mpsc::Receiver<()>>>,
}

impl Worker {
    fn wait_ready(&self, deadline: Instant) -> Result<(), mpsc::RecvTimeoutError> {
        let mut ready = self
            .ready
            .try_lock()
            .map_err(|_| mpsc::RecvTimeoutError::Timeout)?;
        if let Some(receiver) = ready.as_ref() {
            receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
            *ready = None;
        }
        Ok(())
    }
}

fn check_selection_budget(deadline: Instant) -> Result<(), InjectionError> {
    if Instant::now() + Duration::from_millis(600) >= deadline {
        Err(unavailable())
    } else {
        Ok(())
    }
}

/// COM objects never leave this MTA. A bounded queue prevents an unresponsive
/// provider from building up requests; expired jobs cannot change a selection.
fn dispatch<T: Send + 'static>(
    run: impl FnOnce(&IUIAutomation2, Instant) -> Result<T, InjectionError> + Send + 'static,
) -> Result<T, InjectionError> {
    static WORKER: OnceLock<Mutex<Option<Arc<Worker>>>> = OnceLock::new();
    let deadline = Instant::now() + OPERATION_TIMEOUT;
    let slot = WORKER.get_or_init(|| Mutex::new(None));
    let mut held = slot.lock().map_err(|_| unavailable())?;
    if held.is_none() {
        let (sender, receiver) = mpsc::sync_channel::<Job>(1);
        let (ready, initialized) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("text-accessibility".into())
            .spawn(move || {
                let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                if result.is_err() {
                    return;
                }
                let _apartment = Apartment(true);
                let Ok(automation) = (unsafe {
                    CoCreateInstance::<_, IUIAutomation2>(
                        &CUIAutomation8,
                        None,
                        CLSCTX_INPROC_SERVER,
                    )
                }) else {
                    return;
                };
                if unsafe { automation.SetConnectionTimeout(500) }.is_err()
                    || unsafe { automation.SetTransactionTimeout(500) }.is_err()
                {
                    return;
                }
                if ready.send(()).is_err() {
                    return;
                }
                while let Ok(job) = receiver.recv() {
                    // Each job also checks its own absolute deadline.
                    job(&automation, Instant::now());
                }
            })
            .map_err(|_| unavailable())?;
        // Retain a pending initializer as well as a ready worker. A provider
        // hung in COM activation must not cause one new thread per request.
        *held = Some(Arc::new(Worker {
            sender,
            ready: Mutex::new(Some(initialized)),
        }));
    }
    let worker = held.as_ref().ok_or_else(unavailable)?.clone();
    drop(held);
    match worker.wait_ready(deadline) {
        Ok(()) => (),
        Err(mpsc::RecvTimeoutError::Timeout) => return Err(unavailable()),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let mut held = slot.lock().map_err(|_| unavailable())?;
            if held
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &worker))
            {
                *held = None;
            }
            return Err(unavailable());
        }
    }
    let (reply, result) = mpsc::sync_channel(1);
    let sent = worker.sender.try_send(Box::new(move |automation, _| {
        let response = if Instant::now() >= deadline {
            Err(unavailable())
        } else {
            run(automation, deadline)
        };
        let _ = reply.send(response);
    }));
    let response = match sent {
        Ok(()) => result.recv_timeout(deadline.saturating_duration_since(Instant::now())),
        Err(mpsc::TrySendError::Full(_)) => return Err(unavailable()),
        Err(mpsc::TrySendError::Disconnected(_)) => Err(mpsc::RecvTimeoutError::Disconnected),
    };
    if matches!(response, Err(mpsc::RecvTimeoutError::Disconnected)) {
        let mut held = slot.lock().map_err(|_| unavailable())?;
        if held
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &worker))
        {
            *held = None;
        }
    }
    response.map_err(|_| unavailable())?
}

struct Apartment(bool);
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

struct TextControl {
    element: IUIAutomationElement,
    pattern: IUIAutomationTextPattern,
    deadline: Instant,
}

fn unavailable() -> InjectionError {
    InjectionError::BackendFailure("the focused control does not expose a verifiable text range")
}

impl TextControl {
    fn focused(
        target: &TargetWindow,
        automation: &IUIAutomation2,
        deadline: Instant,
    ) -> Result<Self, InjectionError> {
        let element = unsafe { automation.GetFocusedElement() }.map_err(|_| unavailable())?;
        if unsafe { element.CurrentProcessId() }.map_err(|_| unavailable())?
            != target.process_id as i32
        {
            return Err(InjectionError::TargetChanged);
        }
        if unsafe { element.CurrentIsPassword() }
            .map_err(|_| unavailable())?
            .as_bool()
        {
            return Err(InjectionError::SecureTarget);
        }
        let pattern =
            unsafe { element.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) }
                .map_err(|_| unavailable())?;
        Ok(Self {
            element,
            pattern,
            deadline,
        })
    }

    fn check_deadline(&self) -> Result<(), InjectionError> {
        if Instant::now() >= self.deadline {
            Err(unavailable())
        } else {
            Ok(())
        }
    }

    fn check_selection_budget(&self) -> Result<(), InjectionError> {
        // Reserve a full configured transaction timeout plus reply margin.
        // Do not start a mutating COM call at the end of the caller's budget.
        check_selection_budget(self.deadline)
    }

    fn selection(&self) -> Result<IUIAutomationTextRange, InjectionError> {
        self.check_deadline()?;
        let selections = unsafe { self.pattern.GetSelection() }.map_err(|_| unavailable())?;
        if unsafe { selections.Length() }.map_err(|_| unavailable())? != 1 {
            return Err(unavailable());
        }
        unsafe { selections.GetElement(0) }.map_err(|_| unavailable())
    }

    fn state(&self) -> Result<TargetText, InjectionError> {
        self.check_deadline()?;
        let selection = self.selection()?;
        let document = unsafe { self.pattern.DocumentRange() }.map_err(|_| unavailable())?;
        let before = unsafe { document.Clone() }.map_err(|_| unavailable())?;
        let after = unsafe { document.Clone() }.map_err(|_| unavailable())?;
        unsafe {
            before
                .MoveEndpointByRange(
                    TextPatternRangeEndpoint_End,
                    &selection,
                    TextPatternRangeEndpoint_Start,
                )
                .map_err(|_| unavailable())?;
            after
                .MoveEndpointByRange(
                    TextPatternRangeEndpoint_Start,
                    &selection,
                    TextPatternRangeEndpoint_End,
                )
                .map_err(|_| unavailable())?;
        }
        Ok(TargetText {
            identity: runtime_id(&self.element)?,
            before: read_range(&before)?,
            selected: read_range(&selection)?,
            after: read_range(&after)?,
        })
    }
}

fn runtime_id(element: &IUIAutomationElement) -> Result<Vec<i32>, InjectionError> {
    struct Array(*mut SAFEARRAY);
    impl Drop for Array {
        fn drop(&mut self) {
            unsafe {
                let _ = SafeArrayDestroy(self.0);
            }
        }
    }
    let array = Array(unsafe { element.GetRuntimeId() }.map_err(|_| unavailable())?);
    if array.0.is_null() {
        return Err(unavailable());
    }
    let lower = unsafe { SafeArrayGetLBound(array.0, 1) }.map_err(|_| unavailable())?;
    let upper = unsafe { SafeArrayGetUBound(array.0, 1) }.map_err(|_| unavailable())?;
    if upper < lower || i64::from(upper) - i64::from(lower) > 64 {
        return Err(unavailable());
    }
    let mut result = Vec::new();
    for index in lower..=upper {
        let mut value = 0i32;
        unsafe { SafeArrayGetElement(array.0, &index, (&mut value as *mut i32).cast()) }
            .map_err(|_| unavailable())?;
        result.push(value);
    }
    Ok(result)
}

fn read_range(range: &IUIAutomationTextRange) -> Result<String, InjectionError> {
    Ok(normalize_text(&read_range_raw(range)?))
}

fn read_range_raw(range: &IUIAutomationTextRange) -> Result<String, InjectionError> {
    let text = unsafe { range.GetText(MAX_TEXT_UNITS + 1) }.map_err(|_| unavailable())?;
    if text.len() > MAX_TEXT_UNITS as usize {
        return Err(unavailable());
    }
    Ok(text.to_string())
}

fn bulk_recent_range<R>(
    candidate: R,
    text: &str,
    move_start: impl FnOnce(&R, i32) -> Result<(), InjectionError>,
    read: impl FnOnce(&R) -> Result<String, InjectionError>,
) -> Result<Option<R>, InjectionError> {
    let Ok(units) = i32::try_from(text.encode_utf16().count()) else {
        return Ok(None);
    };
    if move_start(&candidate, -units).is_err() {
        return Ok(None);
    }
    Ok(read(&candidate)
        .ok()
        .filter(|actual| actual == text)
        .map(|_| candidate))
}

pub(super) fn read(target: &TargetWindow) -> Result<TargetText, InjectionError> {
    let target = target.clone();
    dispatch(move |automation, deadline| {
        TextControl::focused(&target, automation, deadline)?.state()
    })
}

pub(super) fn selection_is_empty(target: &TargetWindow) -> Result<bool, InjectionError> {
    let target = target.clone();
    dispatch(move |automation, deadline| selection_is_empty_inner(&target, automation, deadline))
}

fn selection_is_empty_inner(
    target: &TargetWindow,
    automation: &IUIAutomation2,
    deadline: Instant,
) -> Result<bool, InjectionError> {
    let control = TextControl::focused(target, automation, deadline)?;
    let selection = control.selection()?;
    // Compare endpoints instead of GetText: an empty embedded object can be
    // selected without returning characters. A final paste must be additive.
    unsafe {
        selection.CompareEndpoints(
            TextPatternRangeEndpoint_Start,
            &selection,
            TextPatternRangeEndpoint_End,
        )
    }
    .map(|distance| distance == 0)
    .map_err(|_| unavailable())
}

/// Runtime id of the focused element, whether or not it exposes text. Single
/// window apps (browsers, Electron, WPF) keep one focus window for every
/// field, so only this id tells their fields apart.
pub(super) fn focused_identity(target: &TargetWindow) -> Result<Vec<i32>, InjectionError> {
    let target = target.clone();
    dispatch(move |automation, _| focused_identity_inner(&target, automation))
}

fn focused_identity_inner(
    target: &TargetWindow,
    automation: &IUIAutomation2,
) -> Result<Vec<i32>, InjectionError> {
    let element = unsafe { automation.GetFocusedElement() }.map_err(|_| unavailable())?;
    if unsafe { element.CurrentProcessId() }.map_err(|_| unavailable())? != target.process_id as i32
    {
        return Err(InjectionError::TargetChanged);
    }
    runtime_id(&element)
}

/// Moves the start of the selection to its end and selects that caret,
/// only while the field still shows exactly `expected`. No text changes.
pub(super) fn collapse_selection(
    target: &TargetWindow,
    expected: &TargetText,
) -> Result<bool, InjectionError> {
    let (target, expected) = (target.clone(), expected.clone());
    dispatch(move |automation, deadline| {
        collapse_selection_inner(&target, &expected, automation, deadline)
    })
}

fn collapse_selection_inner(
    target: &TargetWindow,
    expected: &TargetText,
    automation: &IUIAutomation2,
    deadline: Instant,
) -> Result<bool, InjectionError> {
    let control = TextControl::focused(target, automation, deadline)?;
    if !control.state()?.same_content(expected) {
        return Ok(false);
    }
    let range = control.selection()?;
    let end = unsafe { range.Clone() }.map_err(|_| unavailable())?;
    unsafe {
        range
            .MoveEndpointByRange(
                TextPatternRangeEndpoint_Start,
                &end,
                TextPatternRangeEndpoint_End,
            )
            .map_err(|_| unavailable())?;
        control.check_selection_budget()?;
        range.Select().map_err(|_| unavailable())?;
    }
    Ok(true)
}

pub(super) fn select_recent(
    target: &TargetWindow,
    expected: &TargetText,
    text: &str,
) -> Result<bool, InjectionError> {
    let (target, expected, text) = (target.clone(), expected.clone(), text.to_owned());
    dispatch(move |automation, deadline| {
        select_recent_inner(&target, &expected, &text, automation, deadline)
    })
}

fn select_recent_inner(
    target: &TargetWindow,
    expected: &TargetText,
    text: &str,
    automation: &IUIAutomation2,
    deadline: Instant,
) -> Result<bool, InjectionError> {
    let control = TextControl::focused(target, automation, deadline)?;
    if !control.state()?.same_content(expected) {
        return Ok(false);
    }
    let Some(_) = expected.select_recent(text) else {
        return Ok(false);
    };
    let range = control.selection()?;
    let text = normalize_text(text);
    // Most providers accept the UTF-16 length in one move. Treat it only as
    // a hint: CRLF, surrogate pairs and graphemes can use different units.
    // Never select the candidate unless its complete normalized text matches.
    if let Some(candidate) = bulk_recent_range(
        unsafe { range.Clone() }.map_err(|_| unavailable())?,
        &text,
        |range, units| {
            unsafe {
                range.MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Character, units)
            }
            .map(|_| ())
            .map_err(|_| unavailable())
        },
        read_range,
    )? {
        if !control.state()?.same_content(expected) {
            return Ok(false);
        }
        control.check_selection_budget()?;
        unsafe { candidate.Select() }.map_err(|_| unavailable())?;
        return Ok(true);
    }
    // UIA providers disagree on grapheme/caret units, and some spend two units
    // on CRLF even though read_range normalizes it to LF. Walk until the
    // provider reaches the document boundary, accumulating only each newly
    // added unit so cross-process text transfer remains linear.
    let mut raw_actual = String::new();
    loop {
        control.check_deadline()?;
        let previous = unsafe { range.Clone() }.map_err(|_| unavailable())?;
        if unsafe {
            range.MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Character, -1)
        }
        .map_err(|_| unavailable())?
            == 0
        {
            return Ok(false);
        }
        let added = unsafe { range.Clone() }.map_err(|_| unavailable())?;
        unsafe {
            added
                .MoveEndpointByRange(
                    TextPatternRangeEndpoint_End,
                    &previous,
                    TextPatternRangeEndpoint_Start,
                )
                .map_err(|_| unavailable())?;
        }
        raw_actual.insert_str(0, &read_range_raw(&added)?);
        let actual = normalize_text(&raw_actual);
        if actual == text {
            // Verify the complete candidate range immediately before Select().
            if read_range(&range)? != text || !control.state()?.same_content(expected) {
                return Ok(false);
            }
            control.check_selection_budget()?;
            unsafe { range.Select() }.map_err(|_| unavailable())?;
            // Providers such as Chromium apply Select asynchronously. The batch
            // layer confirms the selection while checking focus, input and IME.
            return Ok(true);
        }
        if !text.ends_with(&actual) {
            return Ok(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    static WORKER_TEST: Mutex<()> = Mutex::new(());

    #[test]
    fn pending_initializer_is_retained_across_timeouts_then_becomes_ready() {
        let (ready, initialized) = mpsc::sync_channel(1);
        let (sender, _jobs) = mpsc::sync_channel(1);
        let worker = Worker {
            sender,
            ready: Mutex::new(Some(initialized)),
        };
        for _ in 0..5 {
            assert_eq!(
                worker.wait_ready(Instant::now()),
                Err(mpsc::RecvTimeoutError::Timeout)
            );
            assert!(worker.ready.lock().unwrap().is_some());
        }
        ready.send(()).unwrap();
        worker
            .wait_ready(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert!(worker.ready.lock().unwrap().is_none());
        worker.wait_ready(Instant::now()).unwrap();
    }

    #[test]
    fn bulk_selection_verifies_provider_units_before_accepting_a_range() {
        for (units, wanted, matches) in [
            (vec!["prefix", "a", "b", "c"], "abc", true),
            (vec!["prefix", "a", "\r", "\n", "b"], "a\nb", false),
            (vec!["prefix", "😀"], "😀", false),
            (vec!["prefix", "e\u{301}"], "e\u{301}", false),
        ] {
            let moves = Cell::new(0);
            let result = bulk_recent_range(
                Cell::new(units.len()),
                wanted,
                |range, count| {
                    moves.set(moves.get() + 1);
                    range.set(range.get().saturating_sub(count.unsigned_abs() as usize));
                    Ok(())
                },
                |range| Ok(normalize_text(&units[range.get()..].concat())),
            )
            .unwrap();
            assert_eq!(result.is_some(), matches, "{wanted:?}");
            assert_eq!(moves.get(), 1);
        }
    }

    #[test]
    fn rejected_bulk_moves_and_reads_leave_the_unit_fallback_available() {
        let rejected_move = bulk_recent_range(
            (),
            "text",
            |_, _| Err(unavailable()),
            |_| panic!("must not read rejected move"),
        );
        assert!(rejected_move.unwrap().is_none());
        let rejected_read = bulk_recent_range((), "text", |_, _| Ok(()), |_| Err(unavailable()));
        assert!(rejected_read.unwrap().is_none());
    }

    #[test]
    fn selection_is_not_started_without_a_full_transaction_budget() {
        let _serial = WORKER_TEST.lock().unwrap();
        use std::sync::atomic::{AtomicBool, Ordering};
        let selected = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicBool::new(false));
        let started = Arc::clone(&entered);
        let observed = Arc::clone(&selected);
        let response = dispatch(move |_, deadline| {
            started.store(true, Ordering::SeqCst);
            // A slow read leaves less time than a single allowed Select.
            std::thread::sleep(Duration::from_millis(1_500));
            check_selection_budget(deadline)?;
            observed.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(500));
            Ok(())
        });
        assert!(response.is_err());
        assert!(entered.load(Ordering::SeqCst));
        assert!(!selected.load(Ordering::SeqCst));
    }

    #[test]
    fn accessibility_instance_and_timeouts_are_reused_on_its_mta_thread() {
        let _serial = WORKER_TEST.lock().unwrap();
        let inspect = |automation: &IUIAutomation2, _: Instant| {
            Ok((
                std::thread::current().id(),
                unsafe { automation.ConnectionTimeout() }.unwrap(),
                unsafe { automation.TransactionTimeout() }.unwrap(),
            ))
        };
        let first = dispatch(inspect).unwrap();
        let second = dispatch(inspect).unwrap();
        assert_eq!(first, second);
        assert_ne!(first.0, std::thread::current().id());
        assert_eq!((first.1, first.2), (500, 500));
    }
}

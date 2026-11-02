//! Per-take span instrumentation.
//!
//! Every recording take gets a random, ephemeral take id when it starts. Its
//! spans and exactly one terminal outcome are recorded under that id in
//! `take_spans`, apart from the historical `metrics` events. The id is not
//! derived from the operation id, time or a History row, and diagnostics
//! never export it. Records are content-free: fixed codes and milliseconds.
//!
//! A take ends either before a stop (start failure, cancel while starting or
//! recording) through [`PendingTakes::end`], or in the stop pipeline, which
//! concludes it from the delivery result and releases it right after the
//! lifecycle accepts a new take. Its row is written on the deferred
//! completion queue, never on the measured path.
use crate::state::PipelineMode;
use crate::storage::{Storage, TakeOutcome, TakeSpan, TakeSpanRecord, TakeStage};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

struct TraceState {
    take_id: String,
    mode: PipelineMode,
    /// When the stop was requested; `None` for a take that never stopped.
    stop: Option<Instant>,
    /// The stage the take is in, which names where a take without output
    /// failed or was cancelled.
    stage: TakeStage,
    outcome: Option<TakeOutcome>,
    spans: [Option<i64>; TakeSpan::ALL.len()],
    storage: Storage,
    /// The completion-queue place of this take's row, taken when the take
    /// is concluded, before its lifecycle guard is released.
    reservation: Option<crate::storage::CompletionReservation>,
    released: bool,
}

impl TraceState {
    fn record(&self) -> TakeSpanRecord {
        TakeSpanRecord {
            take_id: self.take_id.clone(),
            mode: self.mode,
            // A take released without a conclusion still has one outcome.
            outcome: self.outcome.unwrap_or(TakeOutcome::Failed(self.stage)),
            spans: self.spans,
        }
    }
}

/// A shared handle to one take's spans. A disabled trace (dictation tests)
/// records nothing. Clones refer to the same take.
#[derive(Clone, Default)]
pub(crate) struct TakeTrace(Option<Arc<Mutex<TraceState>>>);

fn duration_ms(started: Instant) -> i64 {
    i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)
}

/// 128 random bits as hex. Not derived from any operation, time or row id.
fn random_take_id() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // Still unrelated to any identifier: per-process random hash keys.
        use std::hash::{BuildHasher, Hasher};
        for chunk in bytes.chunks_mut(8) {
            let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
            hasher.write_usize(chunk.as_ptr() as usize);
            chunk.copy_from_slice(&hasher.finish().to_le_bytes());
        }
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl TakeTrace {
    pub(crate) fn disabled() -> Self {
        Self(None)
    }

    pub(crate) fn new(storage: &Storage, mode: PipelineMode) -> Self {
        Self(Some(Arc::new(Mutex::new(TraceState {
            take_id: random_take_id(),
            mode,
            stop: None,
            stage: TakeStage::Start,
            outcome: None,
            spans: [None; TakeSpan::ALL.len()],
            storage: storage.clone(),
            reservation: None,
            released: false,
        }))))
    }

    fn lock(state: &Mutex<TraceState>) -> MutexGuard<'_, TraceState> {
        state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn with(&self, update: impl FnOnce(&mut TraceState)) {
        if let Some(state) = &self.0 {
            update(&mut Self::lock(state));
        }
    }

    /// Records a span that happened. The first measurement of a span wins.
    pub(crate) fn span(&self, span: TakeSpan, ms: i64) {
        self.with(|state| {
            state.spans[span.index()].get_or_insert(ms);
        });
    }

    pub(crate) fn span_since(&self, span: TakeSpan, started: Instant) {
        self.span(span, duration_ms(started));
    }

    pub(crate) fn stage(&self, stage: TakeStage) {
        self.with(|state| state.stage = stage);
    }

    /// An explicit outcome for an exit whose result does not name it, such
    /// as an insertion failure returned as an error. The first one wins.
    pub(crate) fn outcome(&self, outcome: TakeOutcome) {
        self.with(|state| {
            state.outcome.get_or_insert(outcome);
        });
    }

    pub(crate) fn stopped(&self, at: Instant) {
        self.with(|state| state.stop = Some(at));
    }

    /// Marks the output commit as returned: stop to delivery complete.
    pub(crate) fn delivered(&self) {
        self.with(|state| {
            if let Some(stop) = state.stop {
                state.spans[TakeSpan::StopToDelivery.index()].get_or_insert(duration_ms(stop));
            }
        });
    }

    /// Fixes the terminal outcome from the stop pipeline's result: a
    /// delivered take by its insertion label, otherwise an explicit outcome
    /// or a failure/cancel at the stage it reached.
    ///
    /// Also reserves the row's place on the completion queue. Every caller
    /// concludes while it still owns the lifecycle, so a Delete all or
    /// retention purge that starts once a new take is accepted waits for
    /// this row and removes it, instead of the row landing after it.
    pub(crate) fn conclude(&self, delivered: Result<&str, ()>, cancelled: bool) {
        self.with(|state| {
            if !state.released && state.reservation.is_none() {
                state.reservation = Some(state.storage.reserve_completion());
            }
            state.outcome = Some(match delivered {
                Ok(label) => outcome_for_label(label),
                Err(()) => state.outcome.unwrap_or(if cancelled {
                    TakeOutcome::Cancelled(state.stage)
                } else {
                    TakeOutcome::Failed(state.stage)
                }),
            });
        });
    }

    /// Ends the take: records stop to next take (when it stopped) and queues
    /// its row on the completion queue, after every job queued before it.
    /// Only the first call writes.
    pub(crate) fn release(&self) {
        let Some(shared) = &self.0 else {
            return;
        };
        let (reservation, job_storage) = {
            let mut state = Self::lock(shared);
            if state.released {
                return;
            }
            state.released = true;
            if let Some(stop) = state.stop {
                state.spans[TakeSpan::StopToNextTake.index()].get_or_insert(duration_ms(stop));
            }
            let reservation = state
                .reservation
                .take()
                .unwrap_or_else(|| state.storage.reserve_completion());
            (reservation, state.storage.clone())
        };
        let shared = Arc::clone(shared);
        // Read at run time, so a completion job queued earlier by this take
        // has already added its own span.
        reservation.defer(move || {
            let record = Self::lock(&shared).record();
            let _ = job_storage.add_take_spans(&record);
        });
    }

    /// Releases the take when dropped; declared before the lifecycle guard
    /// so it runs right after the lifecycle accepts a new take.
    pub(crate) fn release_on_drop(&self) -> ReleaseOnDrop {
        ReleaseOnDrop(self.clone())
    }
}

pub(crate) struct ReleaseOnDrop(TakeTrace);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// The outcome of a take that delivered with `label` (its `RecordingResult`
/// insertion code).
pub(crate) fn outcome_for_label(label: &str) -> TakeOutcome {
    match label {
        "clipboard_paste" | "provisional_replace" | "selection" | "caret" => TakeOutcome::Pasted,
        "paste_unverified" => TakeOutcome::PasteUnverified,
        "clipboard_only" => TakeOutcome::ClipboardOnly,
        "panel" => TakeOutcome::Panel,
        "search" => TakeOutcome::Search,
        // `insertion_failed`, `clipboard_protected`: nothing received it.
        _ => TakeOutcome::InsertionFailed,
    }
}

/// The take of the recording that is starting or running, until a stop
/// claims it. Only one recording runs at a time; the operation id keeps a
/// late call from touching another take.
pub(crate) struct PendingTakes {
    pending: Mutex<Option<(u64, TakeTrace, Instant)>>,
}

pub(crate) static PENDING: PendingTakes = PendingTakes::new();

impl PendingTakes {
    pub(crate) const fn new() -> Self {
        Self {
            pending: Mutex::new(None),
        }
    }

    fn slot(&self) -> MutexGuard<'_, Option<(u64, TakeTrace, Instant)>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts the take of `operation_id`, requested at `requested`.
    pub(crate) fn begin(
        &self,
        operation_id: u64,
        storage: &Storage,
        mode: PipelineMode,
        requested: Instant,
    ) {
        let stale = self
            .slot()
            .replace((operation_id, TakeTrace::new(storage, mode), requested));
        // Unreachable while one recording runs at a time; never lose a take.
        if let Some((_, trace, _)) = stale {
            trace.release();
        }
    }

    /// The microphone is capturing: start request to ready.
    pub(crate) fn ready(&self, operation_id: u64) {
        if let Some((id, trace, requested)) = self.slot().as_ref() {
            if *id == operation_id {
                trace.span_since(TakeSpan::RecordReady, *requested);
                trace.stage(TakeStage::Recording);
            }
        }
    }

    /// Ends a take that never reached a stop: a failed or cancelled start, or
    /// a recording cancelled before it was stopped.
    pub(crate) fn end(&self, operation_id: u64, cancelled: bool) {
        let ended = {
            let mut slot = self.slot();
            match slot.as_ref() {
                Some((id, _, _)) if *id == operation_id => slot.take(),
                _ => None,
            }
        };
        if let Some((_, trace, _)) = ended {
            trace.conclude(Err(()), cancelled);
            trace.release();
        }
    }

    /// Hands the take of `operation_id` to its stop pipeline.
    pub(crate) fn claim(&self, operation_id: u64) -> Option<TakeTrace> {
        let mut slot = self.slot();
        match slot.as_ref() {
            Some((id, _, _)) if *id == operation_id => slot.take().map(|(_, trace, _)| trace),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn drained(storage: &Storage) -> Vec<crate::storage::TakeRowForTest> {
        assert!(storage.drain_deferred_completions(Duration::from_secs(10)));
        storage.take_records_for_test()
    }

    #[test]
    fn spans_keep_missing_apart_from_zero_and_the_first_measurement() {
        let storage = Storage::in_memory().unwrap();
        let trace = TakeTrace::new(&storage, PipelineMode::Dictate);
        trace.span(TakeSpan::Asr, 0);
        trace.span(TakeSpan::Asr, 99);
        trace.span(TakeSpan::Paste, 7);
        trace.conclude(Ok("clipboard_paste"), false);
        trace.release();
        trace.release();
        let rows = drained(&storage);
        assert_eq!(rows.len(), 1, "one record per take");
        let row = &rows[0];
        assert_eq!(row.outcome, "pasted");
        assert_eq!(row.stage, None);
        assert_eq!(row.span(TakeSpan::Asr), Some(0));
        assert_eq!(row.span(TakeSpan::Paste), Some(7));
        assert_eq!(row.span(TakeSpan::Settle), None);
        // Never stopped, so neither stop-relative span exists.
        assert_eq!(row.span(TakeSpan::StopToNextTake), None);
        assert_eq!(row.take_id.len(), 32);
        assert!(row.take_id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn take_ids_are_random_and_unrelated_to_operations() {
        let storage = Storage::in_memory().unwrap();
        for _ in 0..2 {
            let trace = TakeTrace::new(&storage, PipelineMode::Edit);
            trace.release();
        }
        let rows = drained(&storage);
        assert_ne!(rows[0].take_id, rows[1].take_id);
        for row in &rows {
            assert_ne!(row.take_id, "1");
            assert_ne!(row.take_id, "2");
        }
    }

    #[test]
    fn stop_relative_spans_follow_the_stop_request() {
        let storage = Storage::in_memory().unwrap();
        let trace = TakeTrace::new(&storage, PipelineMode::Dictate);
        trace.delivered();
        trace.stopped(Instant::now() - Duration::from_millis(40));
        trace.delivered();
        trace.conclude(Ok("paste_unverified"), false);
        drop(trace.release_on_drop());
        let rows = drained(&storage);
        let row = &rows[0];
        assert_eq!(row.outcome, "paste_unverified");
        let delivered = row.span(TakeSpan::StopToDelivery).unwrap();
        let next = row.span(TakeSpan::StopToNextTake).unwrap();
        assert!(delivered >= 40 && next >= delivered, "{delivered} {next}");
    }

    #[test]
    fn conclusion_prefers_delivery_then_explicit_outcome_then_stage() {
        let storage = Storage::in_memory().unwrap();
        /// Delivery result, explicit outcome, cancelled; expected outcome and stage.
        type Case = (
            Result<&'static str, ()>,
            Option<TakeOutcome>,
            bool,
            &'static str,
            Option<&'static str>,
        );
        let cases: [Case; 9] = [
            (Ok("provisional_replace"), None, false, "pasted", None),
            (Ok("selection"), None, true, "pasted", None),
            (Ok("clipboard_only"), None, false, "clipboard_only", None),
            (Ok("panel"), None, false, "panel", None),
            (Ok("search"), None, false, "search", None),
            (
                Ok("clipboard_protected"),
                None,
                false,
                "insertion_failed",
                None,
            ),
            (
                Err(()),
                Some(TakeOutcome::InsertionFailed),
                false,
                "insertion_failed",
                None,
            ),
            (Err(()), None, true, "cancelled", Some("route")),
            (Err(()), None, false, "failed", Some("route")),
        ];
        for (delivered, explicit, cancelled, _, _) in cases {
            let trace = TakeTrace::new(&storage, PipelineMode::Dictate);
            trace.stage(TakeStage::Route);
            if let Some(outcome) = explicit {
                trace.outcome(outcome);
            }
            trace.conclude(delivered, cancelled);
            trace.release();
        }
        let rows = drained(&storage);
        for (row, (_, _, _, outcome, stage)) in rows.iter().zip(cases) {
            assert_eq!(row.outcome, outcome);
            assert_eq!(row.stage.as_deref(), stage);
        }
    }

    #[test]
    fn a_delete_all_after_the_lifecycle_release_removes_the_take_row() {
        let storage = Storage::in_memory().unwrap();
        let trace = TakeTrace::new(&storage, PipelineMode::Dictate);
        trace.stopped(Instant::now());
        // Concluded while the stop pipeline still owns the lifecycle.
        trace.conclude(Ok("clipboard_paste"), false);
        // The lifecycle is released; a Delete all starts before the row's
        // job is queued, and must wait for it.
        let deleting = {
            let storage = storage.clone();
            std::thread::spawn(move || storage.delete_all_history().unwrap())
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(!deleting.is_finished(), "Delete all overtook the take row");
        trace.release();
        deleting.join().unwrap();
        assert!(drained(&storage).is_empty());
    }

    #[test]
    fn a_disabled_trace_records_nothing() {
        let storage = Storage::in_memory().unwrap();
        let trace = TakeTrace::disabled();
        trace.span(TakeSpan::Asr, 1);
        trace.conclude(Ok("clipboard_paste"), false);
        trace.release();
        assert!(drained(&storage).is_empty());
    }

    #[test]
    fn pending_takes_end_by_start_or_recording_stage_and_hand_over_to_stop() {
        let storage = Storage::in_memory().unwrap();
        let pending = PendingTakes::new();
        // A start that fails before the microphone captures.
        pending.begin(1, &storage, PipelineMode::Dictate, Instant::now());
        pending.end(1, false);
        // A start cancelled before capturing.
        pending.begin(2, &storage, PipelineMode::Ask, Instant::now());
        pending.end(2, true);
        // A recording cancelled while running; a stale id changes nothing.
        pending.begin(3, &storage, PipelineMode::Edit, Instant::now());
        pending.ready(3);
        pending.ready(99);
        pending.end(99, true);
        pending.end(3, true);
        // A stopped recording is claimed once by its stop pipeline.
        pending.begin(4, &storage, PipelineMode::Translate, Instant::now());
        pending.ready(4);
        assert!(pending.claim(5).is_none());
        let trace = pending.claim(4).expect("pending take");
        assert!(pending.claim(4).is_none());
        pending.end(4, true);
        trace.conclude(Ok("clipboard_paste"), false);
        trace.release();
        let rows = drained(&storage);
        let outcomes: Vec<_> = rows
            .iter()
            .map(|row| (row.outcome.as_str(), row.stage.as_deref()))
            .collect();
        assert_eq!(
            outcomes,
            [
                ("failed", Some("start")),
                ("cancelled", Some("start")),
                ("cancelled", Some("recording")),
                ("pasted", None),
            ]
        );
        assert_eq!(rows[0].span(TakeSpan::RecordReady), None);
        assert!(rows[2].span(TakeSpan::RecordReady).is_some());
        assert!(rows[3].span(TakeSpan::RecordReady).is_some());
    }
}

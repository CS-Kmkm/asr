//! Explicit local evaluation output. Never accepts commands or exports content.
use crate::state::{PipelineMode, PipelinePhase};
use crate::storage::{TakeSpan, TakeSpanRecord};
use serde::Serialize;
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, OnceLock,
    },
    time::{Duration, Instant},
};

static OBSERVER: OnceLock<Option<Observer>> = OnceLock::new();

#[derive(Clone, Serialize)]
struct Event {
    kind: &'static str,
    phase: Option<&'static str>,
    operation_id: Option<u64>,
    mode: Option<&'static str>,
    outcome: Option<&'static str>,
    spans_ms: Option<std::collections::BTreeMap<&'static str, Option<i64>>>,
}

#[derive(Serialize)]
struct Frame<'a> {
    schema: &'static str,
    pid: u32,
    sequence: u64,
    elapsed_ms: u128,
    healthy: bool,
    heartbeat: bool,
    #[serde(flatten)]
    event: &'a Event,
}

struct Observer {
    sender: mpsc::SyncSender<Event>,
    healthy: Arc<AtomicBool>,
}

fn mode_code(mode: PipelineMode) -> &'static str {
    match mode {
        PipelineMode::Dictate => "dictate",
        PipelineMode::Translate => "translate",
        PipelineMode::Edit => "edit",
        PipelineMode::Ask => "ask",
        PipelineMode::SelectedText => "selected_text",
    }
}

fn phase_event(phase: PipelinePhase, id: Option<u64>, mode: Option<PipelineMode>) -> Event {
    Event {
        kind: "lifecycle",
        phase: Some(match phase {
            PipelinePhase::Idle => "idle",
            PipelinePhase::Starting => "starting",
            PipelinePhase::Recording => "recording",
            PipelinePhase::Processing => "processing",
        }),
        operation_id: id,
        mode: mode.map(mode_code),
        outcome: None,
        spans_ms: None,
    }
}

fn write_frame(file: &mut impl Write, frame: &Frame<'_>) -> io::Result<()> {
    serde_json::to_writer(&mut *file, frame)?;
    file.write_all(b"\n")?;
    file.flush()
}

impl Observer {
    fn open(path: &Path) -> io::Result<Self> {
        // No parent creation, truncation, append to an old run, or path output.
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        Self::with_file(file)
    }

    fn with_file(mut file: File) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(128);
        let healthy = Arc::new(AtomicBool::new(true));
        let writer_health = Arc::clone(&healthy);
        std::thread::Builder::new()
            .name("evaluation-writer".into())
            .spawn(move || {
                let started = Instant::now();
                let mut sequence = 0;
                let mut current = phase_event(PipelinePhase::Idle, None, None);
                loop {
                    let heartbeat = match receiver.recv_timeout(Duration::from_millis(250)) {
                        Ok(event) => {
                            current = event;
                            sequence += 1;
                            false
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => true,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    if write_frame(
                        &mut file,
                        &Frame {
                            schema: "asr-evaluation/1",
                            pid: std::process::id(),
                            sequence,
                            elapsed_ms: started.elapsed().as_millis(),
                            healthy: writer_health.load(Ordering::Relaxed),
                            heartbeat,
                            event: &current,
                        },
                    )
                    .is_err()
                    {
                        writer_health.store(false, Ordering::Relaxed);
                        break;
                    }
                }
            })?;
        Ok(Self { sender, healthy })
    }

    fn send(&self, event: Event) {
        if self.sender.try_send(event).is_err() {
            // A dropped event poisons this entire run. Product work continues.
            self.healthy.store(false, Ordering::Relaxed);
        }
    }
}

pub(crate) fn initialize() {
    OBSERVER.get_or_init(|| {
        std::env::var_os("ASR_EVAL_EVENTS").and_then(|path| {
            match Observer::open(Path::new(&path)) {
                Ok(observer) => Some(observer),
                Err(_) => {
                    eprintln!("Evaluation output unavailable; use a new writable file path.");
                    None
                }
            }
        })
    });
}

pub(crate) fn lifecycle(phase: PipelinePhase, id: Option<u64>, mode: Option<PipelineMode>) {
    if let Some(Some(observer)) = OBSERVER.get() {
        observer.send(phase_event(phase, id, mode));
    }
}

pub(crate) fn take(operation_id: Option<u64>, record: &TakeSpanRecord) {
    if let Some(Some(observer)) = OBSERVER.get() {
        observer.send(take_event(operation_id, record));
    }
}

/// Evaluation-only diagnostic in the app's redirected stderr, independent of
/// History retention. Keep the strict JSONL event protocol unchanged.
pub(crate) fn insertion_detail(operation_id: u64, detail: Option<&str>) {
    if let Some(Some(_)) = OBSERVER.get() {
        let _ = writeln!(
            std::io::stderr().lock(),
            "ASR_EVALUATION_INSERTION {}",
            insertion_diagnostic(operation_id, detail)
        );
    }
}

fn insertion_diagnostic(operation_id: u64, detail: Option<&str>) -> String {
    // A future caller must not accidentally export an error body as a code.
    let detail = match detail {
        None => None,
        Some(
            code @ ("input_monitor_unavailable"
            | "user_activity"
            | "target_changed"
            | "target_protected"
            | "ime_active"
            | "ime_unknown"
            | "selection_active"
            | "selection_unknown"
            | "target_unreadable"
            | "not_confirmed"
            | "clipboard_unavailable"
            | "backend_failure"
            | "no_external_target"
            | "clipboard_not_restored"
            | "clipboard_protected"
            | "translation_failed"),
        ) => Some(code),
        Some(_) => Some("unrecognized_reason"),
    };
    serde_json::json!({"schema":"asr-insertion-diagnostic/1", "operation_id":operation_id,
                       "insertion_detail":detail})
    .to_string()
}

fn take_event(operation_id: Option<u64>, record: &TakeSpanRecord) -> Event {
    Event {
        kind: "take",
        phase: None,
        operation_id,
        mode: Some(mode_code(record.mode)),
        outcome: Some(record.outcome.code()),
        spans_ms: Some(
            TakeSpan::ALL
                .into_iter()
                .map(|span| (span.code(), record.spans[span.index()]))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insertion_diagnostic_exports_only_allowlisted_reason_codes() {
        let value: serde_json::Value =
            serde_json::from_str(&insertion_diagnostic(2, Some("ime_unknown"))).unwrap();
        assert_eq!(value["insertion_detail"], "ime_unknown");
        assert_eq!(value["operation_id"], 2);
        assert_eq!(value.as_object().unwrap().len(), 3);
        let private = "private transcript\nwindow title\nsecret";
        let redacted = insertion_diagnostic(2, Some(private));
        assert!(!redacted.contains("private"));
        assert!(redacted.contains("unrecognized_reason"));
    }

    #[test]
    fn real_settings_serializer_exposes_explicit_safety_flags() {
        let value = serde_json::to_value(crate::types::Settings::default()).unwrap();
        for key in [
            "textCorrectionEnabled",
            "automaticVoiceMode",
            "liveTargetInsertion",
        ] {
            assert_eq!(value[key], false);
        }
        assert_eq!(value["holdToTalk"]["dictate"], false);
        assert_eq!(value["shortcuts"]["dictate"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn shared_fixture_matches_producer_including_success_cancel_and_idle() {
        use crate::storage::{TakeOutcome, TakeStage};
        let mut record = TakeSpanRecord {
            take_id: "private-not-exported".into(),
            mode: PipelineMode::Dictate,
            outcome: TakeOutcome::Pasted,
            spans: [None; TakeSpan::ALL.len()],
            route: None,
            key_presses: Some(2),
        };
        record.spans[TakeSpan::RecordReady.index()] = Some(12);
        record.spans[TakeSpan::StopToDelivery.index()] = Some(20);
        let idle = || phase_event(PipelinePhase::Idle, None, None);
        let mut events = vec![
            idle(),
            phase_event(
                PipelinePhase::Starting,
                Some(7),
                Some(PipelineMode::Dictate),
            ),
            phase_event(
                PipelinePhase::Recording,
                Some(7),
                Some(PipelineMode::Dictate),
            ),
            phase_event(
                PipelinePhase::Processing,
                Some(7),
                Some(PipelineMode::Dictate),
            ),
            idle(),
            take_event(Some(7), &record),
            phase_event(
                PipelinePhase::Starting,
                Some(8),
                Some(PipelineMode::Dictate),
            ),
            phase_event(
                PipelinePhase::Recording,
                Some(8),
                Some(PipelineMode::Dictate),
            ),
            idle(),
        ];
        record.outcome = TakeOutcome::Cancelled(TakeStage::Recording);
        record.spans = [None; TakeSpan::ALL.len()];
        events.push(take_event(Some(8), &record));
        let actual: Vec<serde_json::Value> = events
            .iter()
            .enumerate()
            .map(|(index, event)| {
                serde_json::to_value(Frame {
                    schema: "asr-evaluation/1",
                    pid: 123,
                    sequence: index as u64,
                    elapsed_ms: index as u128 * 10,
                    healthy: true,
                    heartbeat: false,
                    event,
                })
                .unwrap()
            })
            .collect();
        let expected: Vec<serde_json::Value> =
            include_str!("../../docs/reference/evaluation-events-v1.jsonl")
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn refuses_existing_output_without_overwriting() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        std::fs::write(&path, "preserved").unwrap();
        assert!(Observer::open(&path).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "preserved");
    }

    #[test]
    fn phase_frame_has_no_text_or_app_state_payload() {
        let mut bytes = Vec::new();
        write_frame(
            &mut bytes,
            &Frame {
                schema: "asr-evaluation/1",
                pid: 1,
                sequence: 2,
                elapsed_ms: 12,
                healthy: true,
                heartbeat: false,
                event: &phase_event(PipelinePhase::Recording, Some(7), Some(PipelineMode::Edit)),
            },
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["phase"], "recording");
        assert_eq!(value["operation_id"], 7);
        assert_eq!(value.as_object().unwrap().len(), 12);
    }

    #[test]
    fn full_queue_invalidates_without_blocking() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let observer = Observer {
            sender,
            healthy: Arc::new(AtomicBool::new(true)),
        };
        observer.send(phase_event(PipelinePhase::Starting, Some(1), None));
        observer.send(phase_event(PipelinePhase::Recording, Some(1), None));
        assert!(!observer.healthy.load(Ordering::Relaxed));
    }

    #[test]
    fn writer_failure_is_propagated() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::PermissionDenied.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(write_frame(
            &mut Broken,
            &Frame {
                schema: "asr-evaluation/1",
                pid: 1,
                sequence: 0,
                elapsed_ms: 0,
                healthy: true,
                heartbeat: true,
                event: &phase_event(PipelinePhase::Idle, None, None),
            }
        )
        .is_err());
    }
}
